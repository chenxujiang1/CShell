mod support;

use cshell_application::{HostRepository, HostRepositoryError, MAX_HOST_DOCUMENT_BYTES};
use cshell_domain::{HostCatalog, HostId, HostPlatformHint, HostRecord, ProfileId};
use cshell_storage::SqliteProfileRepository;
use sqlx::{query, query_scalar, sqlite::SqlitePoolOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
};

fn catalog() -> HostCatalog {
    HostCatalog {
        hosts: vec![HostRecord {
            id: HostId::new(),
            name: "web".into(),
            address: "web.test".into(),
            port: 2222,
            profile_id: ProfileId::new(),
            tags: BTreeSet::from(["production".into()]),
            labels: BTreeMap::from([("env".into(), "prod".into())]),
            platform_hint: HostPlatformHint::Linux,
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn host_cas_reopen_backup_and_failed_write_preserve_the_committed_inventory()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("hosts.sqlite");
    let first = SqliteProfileRepository::open(&path).await?;
    let second = SqliteProfileRepository::open(&path).await?;
    assert!(first.load_hosts().await?.is_none());
    let saved = first.save_hosts(0, &catalog()).await?;
    assert_eq!(saved.revision, 1);
    assert_eq!(
        second.save_hosts(0, &HostCatalog::default()).await,
        Err(HostRepositoryError::Conflict {
            expected: 0,
            actual: 1
        })
    );
    assert_eq!(second.load_hosts().await?, Some(saved.clone()));
    assert!(
        first
            .save_hosts(u64::MAX, &HostCatalog::default())
            .await
            .is_err()
    );
    let backup = first.backup().await?;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("CREATE TRIGGER reject_host BEFORE UPDATE ON host_catalog_state BEGIN SELECT RAISE(ABORT,'injected failure'); END")
        .execute(&raw).await?;
    assert!(first.save_hosts(1, &HostCatalog::default()).await.is_err());
    assert_eq!(second.load_hosts().await?, Some(saved.clone()));
    raw.close().await;
    first.close().await;
    second.close().await;
    let reopened = SqliteProfileRepository::open(&path).await?;
    assert_eq!(reopened.load_hosts().await?, Some(saved.clone()));
    reopened.close().await;
    cshell_storage::restore_backup(&path, &backup).await?;
    let restored = SqliteProfileRepository::open(&path).await?;
    assert_eq!(restored.load_hosts().await?, Some(saved));
    restored.close().await;
    Ok(())
}

#[tokio::test]
async fn corrupt_or_oversized_stored_inventory_is_rejected_before_decoding()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("corrupt.sqlite");
    let repository = SqliteProfileRepository::open(&path).await?;
    repository.save_hosts(0, &catalog()).await?;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("UPDATE host_catalog_state SET document_json = ? WHERE id=1")
        .bind(b"invalid".as_slice())
        .execute(&raw)
        .await?;
    assert_eq!(
        repository.load_hosts().await,
        Err(HostRepositoryError::Corrupt)
    );
    query("UPDATE host_catalog_state SET document_json = zeroblob(?) WHERE id=1")
        .bind((MAX_HOST_DOCUMENT_BYTES + 1) as i64)
        .execute(&raw)
        .await?;
    assert_eq!(
        repository.load_hosts().await,
        Err(HostRepositoryError::Corrupt)
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT revision FROM host_catalog_state WHERE id=1")
            .fetch_one(&raw)
            .await?,
        1
    );
    raw.close().await;
    repository.close().await;
    Ok(())
}

#[tokio::test]
async fn v10_host_migration_backs_up_and_keeps_the_old_schema_on_failure()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("v10.sqlite");
    SqliteProfileRepository::open(&path).await?.close().await;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("DROP TABLE task_run_events").execute(&raw).await?;
    query("DROP TABLE task_targets").execute(&raw).await?;
    query("DROP TABLE task_runs").execute(&raw).await?;
    query("DROP TABLE host_catalog_state").execute(&raw).await?;
    support::strip_newer_fixture_schema(&raw).await?;
    query("PRAGMA user_version = 10").execute(&raw).await?;
    raw.close().await;
    let upgraded = SqliteProfileRepository::open(&path).await?;
    assert!(upgraded.load_hosts().await?.is_none());
    let saved = upgraded.save_hosts(0, &catalog()).await?;
    upgraded.close().await;
    let backup = std::fs::read_dir(directory.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().contains(".backup-"))
        .ok_or("missing migration backup")?;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", backup.display()))
        .await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&raw)
            .await?,
        10
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM sqlite_master WHERE name='host_catalog_state'"
        )
        .fetch_one(&raw)
        .await?,
        0
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM sqlite_master WHERE name='quick_command_state'"
        )
        .fetch_one(&raw)
        .await?,
        1
    );
    raw.close().await;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    // An existing conflicting object makes CREATE fail after the backup.
    query("PRAGMA user_version = 10").execute(&raw).await?;
    raw.close().await;
    assert!(SqliteProfileRepository::open(&path).await.is_err());
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&raw)
            .await?,
        10
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT revision FROM host_catalog_state WHERE id=1")
            .fetch_one(&raw)
            .await?,
        saved.revision as i64
    );
    assert_eq!(
        query_scalar::<_, Vec<u8>>("SELECT document_json FROM host_catalog_state WHERE id=1")
            .fetch_one(&raw)
            .await?,
        cshell_application::encode_hosts(&saved.catalog)?
    );
    raw.close().await;
    Ok(())
}
