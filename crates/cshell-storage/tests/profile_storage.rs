use cshell_application::{
    CatalogChange, CatalogSnapshot, ProfileCatalog, ProfileRepository, ProfileService,
};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, SettingSource, SshAuthMethod,
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
    query("PRAGMA user_version = 7").execute(&pool).await?;
    pool.close().await;
    assert!(matches!(
        SqliteProfileRepository::open(&path).await,
        Err(StorageError::UnsupportedSchema(7))
    ));
    assert!(backups(temp.path())?.is_empty());
    let pool = raw_pool(&path).await?;
    let version: i64 = query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
    assert_eq!(version, 7);
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
        auth_method: SshAuthMethod::Certificate,
        private_key_path: Some("/keys/server".into()),
        certificate_path: Some("/keys/server-cert.pub".into()),
        agent_backend: Default::default(),
        agent_identity: None,
        route: cshell_domain::SshRoute::HttpConnect {
            host: "proxy.example.com".into(),
            port: 8080,
        },
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
async fn v2_ssh_target_migrates_with_backup_and_password_default() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("v2.db");
    let id = ProfileId::new();
    let pool = raw_pool(&path).await?;
    query("CREATE TABLE profile_catalog_meta (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL, terminal_type TEXT NOT NULL, theme TEXT NOT NULL, logging INTEGER NOT NULL)").execute(&pool).await?;
    query("CREATE TABLE profile_folders (id TEXT PRIMARY KEY, name TEXT NOT NULL, parent_id TEXT, terminal_type TEXT, theme TEXT, logging INTEGER)").execute(&pool).await?;
    query("CREATE TABLE profile_records (id TEXT PRIMARY KEY, name TEXT NOT NULL, kind INTEGER NOT NULL, folder_id TEXT, favorite INTEGER NOT NULL, terminal_type TEXT, theme TEXT, logging INTEGER)").execute(&pool).await?;
    query("CREATE TABLE profile_tags (profile_id TEXT NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(profile_id, tag))").execute(&pool).await?;
    query("CREATE TABLE profile_ssh_connections (profile_id TEXT PRIMARY KEY, host TEXT NOT NULL, port INTEGER NOT NULL, username TEXT NOT NULL)").execute(&pool).await?;
    query("INSERT INTO profile_catalog_meta VALUES (1, 4, 'xterm-256color', 'default', 0)")
        .execute(&pool)
        .await?;
    query("INSERT INTO profile_records (id, name, kind, folder_id, favorite) VALUES (?1, 'legacy', 0, NULL, 0)").bind(id.to_string()).execute(&pool).await?;
    query("INSERT INTO profile_ssh_connections VALUES (?1, 'example.com', 22, 'alice')")
        .bind(id.to_string())
        .execute(&pool)
        .await?;
    query("PRAGMA user_version = 2").execute(&pool).await?;
    pool.close().await;

    let repository = SqliteProfileRepository::open(&path).await?;
    let snapshot = repository.load().await?;
    assert_eq!(snapshot.revision, 4);
    assert_eq!(snapshot.ssh_connections.len(), 1);
    assert_eq!(
        snapshot.ssh_connections[0].auth_method,
        SshAuthMethod::Password
    );
    assert_eq!(snapshot.ssh_connections[0].private_key_path, None);
    repository.close().await;
    let backup_paths = backups(temp.path())?;
    assert_eq!(backup_paths.len(), 1);
    let backup_pool = raw_pool(&backup_paths[0]).await?;
    let version: i64 = query_scalar("PRAGMA user_version")
        .fetch_one(&backup_pool)
        .await?;
    assert_eq!(version, 2);
    backup_pool.close().await;
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

#[tokio::test]
async fn v3_route_migration_preserves_identity_and_backup_is_restorable()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("v3.db");
    let record = profile("legacy", None);
    let target = SshConnectionRecord {
        profile_id: record.id,
        host: "target.example.com".into(),
        port: 2222,
        username: "alice".into(),
        auth_method: SshAuthMethod::PrivateKey,
        private_key_path: Some("/keys/legacy".into()),
        certificate_path: None,
        agent_backend: Default::default(),
        agent_identity: None,
        route: Default::default(),
    };
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(record),
                CatalogChange::UpsertSshConnection(target.clone()),
            ],
        )
        .await?;
    service.into_repository().close().await;
    let pool = raw_pool(&path).await?;
    query("ALTER TABLE profile_ssh_connections DROP COLUMN route_json")
        .execute(&pool)
        .await?;
    query("DROP TABLE profile_local_connections")
        .execute(&pool)
        .await?;
    query("PRAGMA user_version = 3").execute(&pool).await?;
    pool.close().await;
    let repository = SqliteProfileRepository::open(&path).await?;
    let snapshot = repository.load().await?;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.ssh_connections, vec![target.clone()]);
    repository.close().await;
    let backups = backups(temp.path())?;
    assert_eq!(backups.len(), 1);
    let backup_pool = raw_pool(&backups[0]).await?;
    let version: i64 = query_scalar("PRAGMA user_version")
        .fetch_one(&backup_pool)
        .await?;
    assert_eq!(version, 3);
    backup_pool.close().await;
    cshell_storage::restore_backup(&path, &backups[0]).await?;
    let repository = SqliteProfileRepository::open(&path).await?;
    assert_eq!(repository.load().await?.ssh_connections, vec![target]);
    repository.close().await;
    Ok(())
}

#[tokio::test]
async fn local_configuration_round_trip_failure_rollback_and_backup_restore()
-> Result<(), Box<dyn Error>> {
    use cshell_domain::{LocalConnectionRecord, LocalWorkingDirectory};
    use std::collections::BTreeMap;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("local.db");
    let mut record = profile("local", None);
    record.kind = ProfileKind::Local;
    let target = LocalConnectionRecord {
        profile_id: record.id,
        close_policy: cshell_domain::LocalClosePolicy::TerminateOnViewClose,
        program: "/custom path/shell".into(),
        args: vec!["".into(), "literal spaces \"quote\" $()".into()],
        cwd: LocalWorkingDirectory::Explicit {
            path: "/directory with spaces".into(),
        },
        env_overrides: BTreeMap::from([
            ("LANG".into(), "zh_CN.UTF-8".into()),
            ("TEST_EMPTY".into(), "".into()),
        ]),
    };
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(record.clone()),
                CatalogChange::UpsertLocalConnection(target.clone()),
            ],
        )
        .await?;
    let repository = service.into_repository();
    let backup = repository.backup().await?;
    repository.close().await;
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    assert_eq!(
        service.load().await?.snapshot().local_connections,
        vec![target.clone()]
    );
    let pool = raw_pool(&path).await?;
    query("CREATE TRIGGER fail_local BEFORE INSERT ON profile_local_connections BEGIN SELECT RAISE(ABORT, 'injected local write failure'); END").execute(&pool).await?;
    pool.close().await;
    let mut changed_record = record.clone();
    changed_record.name = "changed".into();
    let mut changed_target = target.clone();
    changed_target.program = "/changed".into();
    assert!(
        service
            .apply_batch(
                1,
                &[
                    CatalogChange::UpsertProfile(changed_record),
                    CatalogChange::UpsertLocalConnection(changed_target)
                ]
            )
            .await
            .is_err()
    );
    let snapshot = service.load().await?.snapshot();
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.profiles, vec![record.clone()]);
    assert_eq!(snapshot.local_connections, vec![target.clone()]);
    let pool = raw_pool(&path).await?;
    query("DROP TRIGGER fail_local").execute(&pool).await?;
    pool.close().await;
    service
        .apply_batch(1, &[CatalogChange::RemoveProfile(record.id)])
        .await?;
    assert!(
        service
            .load()
            .await?
            .snapshot()
            .local_connections
            .is_empty()
    );
    service.into_repository().close().await;
    restore_backup(&path, &backup).await?;
    let repository = SqliteProfileRepository::open(&path).await?;
    let snapshot = repository.load().await?;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.profiles, vec![record]);
    assert_eq!(snapshot.local_connections, vec![target]);
    repository.close().await;
    Ok(())
}

#[tokio::test]
async fn v4_local_migration_preserves_metadata_and_backup() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("v4.db");
    let mut record = profile("legacy local", None);
    record.kind = ProfileKind::Local;
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    service
        .apply_batch(0, &[CatalogChange::UpsertProfile(record.clone())])
        .await?;
    service.into_repository().close().await;
    let pool = raw_pool(&path).await?;
    query("DROP TABLE profile_local_connections")
        .execute(&pool)
        .await?;
    query("PRAGMA user_version = 4").execute(&pool).await?;
    pool.close().await;
    let repository = SqliteProfileRepository::open(&path).await?;
    let snapshot = repository.load().await?;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.profiles, vec![record.clone()]);
    assert!(snapshot.local_connections.is_empty());
    repository.close().await;
    let backup_paths = backups(temp.path())?;
    assert_eq!(backup_paths.len(), 1);
    let pool = raw_pool(&backup_paths[0]).await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await?,
        4
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM sqlite_master WHERE name = 'profile_local_connections'"
        )
        .fetch_one(&pool)
        .await?,
        0
    );
    pool.close().await;
    restore_backup(&path, &backup_paths[0]).await?;
    let repository = SqliteProfileRepository::open(&path).await?;
    assert_eq!(repository.load().await?.profiles, vec![record]);
    repository.close().await;
    Ok(())
}

#[tokio::test]
async fn existing_v5_local_json_defaults_to_keep_alive_without_rewriting_catalog()
-> Result<(), Box<dyn Error>> {
    use cshell_domain::{LocalClosePolicy, LocalConnectionRecord, LocalWorkingDirectory};
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("legacy-v5.db");
    let mut record = profile("legacy", None);
    record.kind = ProfileKind::Local;
    let target = LocalConnectionRecord {
        profile_id: record.id,
        program: "/legacy/shell".into(),
        args: vec![],
        cwd: LocalWorkingDirectory::Home,
        env_overrides: Default::default(),
        close_policy: LocalClosePolicy::KeepAlive,
    };
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(record),
                CatalogChange::UpsertLocalConnection(target.clone()),
            ],
        )
        .await?;
    service.into_repository().close().await;
    let mut legacy = serde_json::to_value(&target)?;
    legacy
        .as_object_mut()
        .ok_or("not an object")?
        .remove("close_policy");
    let original_json = serde_json::to_string(&legacy)?;
    let pool = raw_pool(&path).await?;
    query("UPDATE profile_local_connections SET configuration_json = ?")
        .bind(&original_json)
        .execute(&pool)
        .await?;
    query("PRAGMA user_version = 5").execute(&pool).await?;
    pool.close().await;
    let repository = SqliteProfileRepository::open(&path).await?;
    let snapshot = repository.load().await?;
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.local_connections, vec![target]);
    repository.close().await;
    let pool = raw_pool(&path).await?;
    assert_eq!(
        query_scalar::<_, String>("SELECT configuration_json FROM profile_local_connections")
            .fetch_one(&pool)
            .await?,
        original_json
    );
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await?,
        6
    );
    pool.close().await;
    let backup_paths = backups(temp.path())?;
    assert_eq!(backup_paths.len(), 1);
    let pool = raw_pool(&backup_paths[0]).await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await?,
        5
    );
    assert_eq!(
        query_scalar::<_, String>("SELECT configuration_json FROM profile_local_connections")
            .fetch_one(&pool)
            .await?,
        original_json
    );
    pool.close().await;
    restore_backup(&path, &backup_paths[0]).await?;
    let repository = SqliteProfileRepository::open(&path).await?;
    let restored = repository.load().await?;
    assert_eq!(restored.revision, snapshot.revision);
    assert_eq!(restored.local_connections, snapshot.local_connections);
    repository.close().await;
    Ok(())
}
