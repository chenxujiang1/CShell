use cshell_application::{
    CatalogChange, CatalogSnapshot, ProfileCatalog, ProfileRepository, ProfileService,
};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, SettingSource,
    SshConnectionRecord, TerminalOverrides,
};
use cshell_storage::{SqliteProfileRepository, StorageError, restore_backup};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{SqlitePool, query, query_scalar};
use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};

async fn raw_pool(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
}

fn folder(name: &str, parent_id: Option<FolderId>) -> ProfileFolder {
    ProfileFolder {
        id: FolderId::new(),
        name: name.into(),
        parent_id,
        terminal: TerminalOverrides::default(),
    }
}

fn profile(name: &str, folder_id: Option<FolderId>) -> ProfileRecord {
    ProfileRecord {
        id: ProfileId::new(),
        name: name.into(),
        kind: ProfileKind::Ssh,
        folder_id,
        tags: BTreeSet::from(["production".into(), "ops".into()]),
        favorite: true,
        terminal: TerminalOverrides::default(),
    }
}

fn backups(directory: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    Ok(std::fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|item| item.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".backup-") && name.ends_with(".sqlite"))
        })
        .collect())
}

#[tokio::test]
async fn catalog_round_trip_preserves_tags_and_inheritance() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("cshell.db");
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    let mut parent = folder("Shared", None);
    parent.terminal.theme = Some("dark".into());
    let mut child = folder("Production", Some(parent.id));
    child.terminal.logging = Some(true);
    let mut record = profile("web-01", Some(child.id));
    record.terminal.terminal_type = Some("screen-256color".into());
    service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertFolder(child.clone()),
                CatalogChange::UpsertProfile(record.clone()),
                CatalogChange::UpsertFolder(parent.clone()),
            ],
        )
        .await?;
    let resolved = service.resolve(record.id).await?;
    assert_eq!(resolved.theme.source, SettingSource::Folder(parent.id));
    assert_eq!(resolved.logging.source, SettingSource::Folder(child.id));
    assert_eq!(
        resolved.terminal_type.source,
        SettingSource::Profile(record.id)
    );
    service.into_repository().close().await;

    let reopened = SqliteProfileRepository::open(&path).await?;
    let snapshot = reopened.load().await?;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.folders.len(), 2);
    assert_eq!(snapshot.profiles, vec![record]);
    reopened.close().await;
    Ok(())
}

#[tokio::test]
async fn compare_and_swap_rejects_stale_writer() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("cshell.db");
    let first = SqliteProfileRepository::open(&path).await?;
    let second = SqliteProfileRepository::open(&path).await?;
    let mut first_catalog = ProfileCatalog::from_snapshot(first.load().await?)?;
    first_catalog.apply_batch(0, &[CatalogChange::UpsertProfile(profile("first", None))])?;
    first.commit(0, first_catalog.snapshot()).await?;

    let mut stale_catalog = ProfileCatalog::from_snapshot(CatalogSnapshot::default())?;
    stale_catalog.apply_batch(0, &[CatalogChange::UpsertProfile(profile("stale", None))])?;
    let result = second.commit(0, stale_catalog.snapshot()).await;
    assert!(matches!(
        result,
        Err(cshell_application::ProfileRepositoryError::Conflict)
    ));
    let snapshot = second.load().await?;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.profiles.len(), 1);
    assert_eq!(snapshot.profiles[0].name, "first");
    first.close().await;
    second.close().await;
    Ok(())
}

#[tokio::test]
async fn failed_insert_rolls_back_revision_and_rows() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("cshell.db");
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    let pool = raw_pool(&path).await?;
    query(
        "CREATE TRIGGER reject_profile BEFORE INSERT ON profile_records
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
    )
    .execute(&pool)
    .await?;
    pool.close().await;

    let result = service
        .apply_batch(0, &[CatalogChange::UpsertProfile(profile("blocked", None))])
        .await;
    assert!(matches!(
        result,
        Err(cshell_application::ProfileServiceError::Repository(
            cshell_application::ProfileRepositoryError::Unavailable
        ))
    ));
    let snapshot = service.load().await?.snapshot();
    assert_eq!(snapshot.revision, 0);
    assert!(snapshot.profiles.is_empty());
    service.into_repository().close().await;
    Ok(())
}

#[tokio::test]
async fn migration_makes_backup_and_failed_upgrade_keeps_original() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let good = temp.path().join("legacy.db");
    let pool = raw_pool(&good).await?;
    query("CREATE TABLE legacy_data (value TEXT NOT NULL)")
        .execute(&pool)
        .await?;
    query("INSERT INTO legacy_data (value) VALUES ('kept')")
        .execute(&pool)
        .await?;
    pool.close().await;
    let repository = SqliteProfileRepository::open(&good).await?;
    assert_eq!(repository.load().await?.revision, 0);
    repository.close().await;
    let backup_paths = backups(temp.path())?;
    assert_eq!(backup_paths.len(), 1);
    let backup_pool = raw_pool(&backup_paths[0]).await?;
    let old_version: i64 = query_scalar("PRAGMA user_version")
        .fetch_one(&backup_pool)
        .await?;
    let value: String = query_scalar("SELECT value FROM legacy_data")
        .fetch_one(&backup_pool)
        .await?;
    assert_eq!(old_version, 0);
    assert_eq!(value, "kept");
    backup_pool.close().await;

    let failed = temp.path().join("broken.db");
    let pool = raw_pool(&failed).await?;
    query("CREATE TABLE profile_catalog_meta (wrong_column TEXT)")
        .execute(&pool)
        .await?;
    query("INSERT INTO profile_catalog_meta (wrong_column) VALUES ('original')")
        .execute(&pool)
        .await?;
    pool.close().await;
    assert!(SqliteProfileRepository::open(&failed).await.is_err());
    let pool = raw_pool(&failed).await?;
    let version: i64 = query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
    let old_value: String = query_scalar("SELECT wrong_column FROM profile_catalog_meta")
        .fetch_one(&pool)
        .await?;
    assert_eq!(version, 0);
    assert_eq!(old_value, "original");
    pool.close().await;
    assert_eq!(backups(temp.path())?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn verified_backup_restores_previous_catalog_and_bad_copy_is_rejected()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("cshell.db");
    let repository = SqliteProfileRepository::open(&path).await?;
    let mut catalog = ProfileCatalog::from_snapshot(repository.load().await?)?;
    let record = profile("restore-me", None);
    catalog.apply_batch(0, &[CatalogChange::UpsertProfile(record.clone())])?;
    repository.commit(0, catalog.snapshot()).await?;
    let backup = repository.backup().await?;
    let mut changed = ProfileCatalog::from_snapshot(repository.load().await?)?;
    changed.apply_batch(1, &[CatalogChange::RemoveProfile(record.id)])?;
    repository.commit(1, changed.snapshot()).await?;
    repository.close().await;

    restore_backup(&path, &backup).await?;
    let restored = SqliteProfileRepository::open(&path).await?;
    let snapshot = restored.load().await?;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.profiles, vec![record]);
    restored.close().await;

    let corrupt = temp.path().join("corrupt.sqlite");
    tokio::fs::write(&corrupt, b"not a sqlite database").await?;
    assert!(matches!(
        restore_backup(&path, &corrupt).await,
        Err(StorageError::CorruptBackup)
    ));
    let intact = SqliteProfileRepository::open(&path).await?;
    assert_eq!(intact.load().await?.revision, 1);
    intact.close().await;
    Ok(())
}

#[tokio::test]
async fn future_schema_is_not_downgraded() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("future.db");
    let pool = raw_pool(&path).await?;
    query("PRAGMA user_version = 3").execute(&pool).await?;
    pool.close().await;
    assert!(matches!(
        SqliteProfileRepository::open(&path).await,
        Err(StorageError::UnsupportedSchema(3))
    ));
    assert!(backups(temp.path())?.is_empty());
    let pool = raw_pool(&path).await?;
    let version: i64 = query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
    assert_eq!(version, 3);
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn ssh_target_round_trip_and_failed_update_are_atomic() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("targets.db");
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    let record = profile("server", None);
    let target = SshConnectionRecord {
        profile_id: record.id,
        host: "server.example.com".into(),
        port: 2222,
        username: "alice".into(),
    };
    service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(record.clone()),
                CatalogChange::UpsertSshConnection(target.clone()),
            ],
        )
        .await?;
    service.into_repository().close().await;
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    assert_eq!(
        service.load().await?.snapshot().ssh_connections,
        vec![target.clone()]
    );

    let pool = raw_pool(&path).await?;
    query(
        "CREATE TRIGGER reject_target BEFORE INSERT ON profile_ssh_connections
           WHEN NEW.host = 'blocked.example.com'
           BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
    )
    .execute(&pool)
    .await?;
    pool.close().await;
    let mut changed = target.clone();
    changed.host = "blocked.example.com".into();
    assert!(
        service
            .apply_batch(1, &[CatalogChange::UpsertSshConnection(changed)])
            .await
            .is_err()
    );
    let intact = service.load().await?.snapshot();
    assert_eq!(intact.revision, 1);
    assert_eq!(intact.ssh_connections, vec![target]);
    service.into_repository().close().await;
    Ok(())
}

#[tokio::test]
async fn v1_upgrade_preserves_profiles_and_backs_up_old_schema() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("v1.db");
    let id = ProfileId::new();
    let pool = raw_pool(&path).await?;
    query("CREATE TABLE profile_catalog_meta (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL, terminal_type TEXT NOT NULL, theme TEXT NOT NULL, logging INTEGER NOT NULL)").execute(&pool).await?;
    query("CREATE TABLE profile_folders (id TEXT PRIMARY KEY, name TEXT NOT NULL, parent_id TEXT, terminal_type TEXT, theme TEXT, logging INTEGER)").execute(&pool).await?;
    query("CREATE TABLE profile_records (id TEXT PRIMARY KEY, name TEXT NOT NULL, kind INTEGER NOT NULL, folder_id TEXT, favorite INTEGER NOT NULL, terminal_type TEXT, theme TEXT, logging INTEGER)").execute(&pool).await?;
    query("CREATE TABLE profile_tags (profile_id TEXT NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(profile_id, tag))").execute(&pool).await?;
    query("INSERT INTO profile_catalog_meta VALUES (1, 7, 'xterm-256color', 'default', 0)")
        .execute(&pool)
        .await?;
    query("INSERT INTO profile_records (id, name, kind, folder_id, favorite) VALUES (?1, 'legacy', 0, NULL, 0)")
        .bind(id.to_string()).execute(&pool).await?;
    query("PRAGMA user_version = 1").execute(&pool).await?;
    pool.close().await;

    let repository = SqliteProfileRepository::open(&path).await?;
    let snapshot = repository.load().await?;
    assert_eq!(snapshot.revision, 7);
    assert_eq!(snapshot.profiles.len(), 1);
    assert_eq!(snapshot.profiles[0].id, id);
    assert!(snapshot.ssh_connections.is_empty());
    repository.close().await;
    let backup_paths = backups(temp.path())?;
    assert_eq!(backup_paths.len(), 1);
    let backup_pool = raw_pool(&backup_paths[0]).await?;
    let version: i64 = query_scalar("PRAGMA user_version")
        .fetch_one(&backup_pool)
        .await?;
    assert_eq!(version, 1);
    backup_pool.close().await;
    Ok(())
}
