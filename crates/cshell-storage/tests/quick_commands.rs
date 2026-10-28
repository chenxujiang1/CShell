mod support;

use cshell_application::{QuickCommandRepository, QuickCommandRepositoryError};
use cshell_domain::{
    QuickCommandAction, QuickCommandCatalog, QuickCommandId, QuickCommandRecord, QuickCommandTarget,
};
use cshell_storage::SqliteProfileRepository;
use sqlx::{query, query_scalar, sqlite::SqlitePoolOptions};
use std::{collections::BTreeSet, error::Error};

#[tokio::test]
async fn quick_command_cas_reopen_backup_and_insert_failure_preserve_committed_catalog()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("commands.sqlite");
    let first = SqliteProfileRepository::open(&path).await?;
    let second = SqliteProfileRepository::open(&path).await?;
    let catalog = QuickCommandCatalog {
        commands: vec![QuickCommandRecord {
            id: QuickCommandId::new(),
            name: "health".into(),
            folder_id: None,
            tags: BTreeSet::from(["ops".into()]),
            systems: BTreeSet::new(),
            action: QuickCommandAction::SendText {
                text: "echo ready".into(),
                append_enter: true,
                target: QuickCommandTarget::Current,
            },
        }],
        ..Default::default()
    };
    assert!(first.load_quick_commands().await?.is_none());
    let saved = first.save_quick_commands(0, &catalog).await?;
    assert_eq!(saved.revision, 1);
    assert_eq!(
        second
            .save_quick_commands(0, &QuickCommandCatalog::default())
            .await,
        Err(QuickCommandRepositoryError::Conflict {
            expected: 0,
            actual: 1
        })
    );
    let backup = first.backup().await?;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("CREATE TRIGGER reject_quick BEFORE UPDATE ON quick_command_state BEGIN SELECT RAISE(ABORT,'injected failure'); END").execute(&raw).await?;
    assert!(
        first
            .save_quick_commands(1, &QuickCommandCatalog::default())
            .await
            .is_err()
    );
    assert_eq!(second.load_quick_commands().await?, Some(saved.clone()));
    raw.close().await;
    first.close().await;
    second.close().await;
    let reopened = SqliteProfileRepository::open(&path).await?;
    assert_eq!(reopened.load_quick_commands().await?, Some(saved.clone()));
    reopened.close().await;
    cshell_storage::restore_backup(&path, &backup).await?;
    let restored = SqliteProfileRepository::open(&path).await?;
    assert_eq!(restored.load_quick_commands().await?, Some(saved));
    restored.close().await;
    Ok(())
}

#[tokio::test]
async fn v9_quick_command_migration_keeps_backup_and_rolls_back_failed_schema_change()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("v9.sqlite");
    SqliteProfileRepository::open(&path).await?.close().await;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("DROP TABLE task_run_events").execute(&raw).await?;
    query("DROP TABLE task_targets").execute(&raw).await?;
    query("DROP TABLE task_runs").execute(&raw).await?;
    query("DROP TABLE host_catalog_state").execute(&raw).await?;
    query("DROP TABLE quick_command_state")
        .execute(&raw)
        .await?;
    support::strip_newer_fixture_schema(&raw).await?;
    query("PRAGMA user_version = 9").execute(&raw).await?;
    raw.close().await;
    let upgraded = SqliteProfileRepository::open(&path).await?;
    assert!(upgraded.load_quick_commands().await?.is_none());
    upgraded.close().await;
    let backup = std::fs::read_dir(directory.path())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().contains(".backup-"))
        .ok_or("missing backup")?;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", backup.display()))
        .await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&raw)
            .await?,
        9
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM sqlite_master WHERE name='quick_command_state'"
        )
        .fetch_one(&raw)
        .await?,
        0
    );
    raw.close().await;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("PRAGMA user_version = 9").execute(&raw).await?;
    raw.close().await;
    assert!(SqliteProfileRepository::open(&path).await.is_err());
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&raw)
            .await?,
        9
    );
    raw.close().await;
    Ok(())
}
