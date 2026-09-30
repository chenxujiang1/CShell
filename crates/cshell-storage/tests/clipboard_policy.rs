use cshell_application::{
    ClipboardPolicyDocument, ClipboardPolicyError, ClipboardPolicyRepository, ProfileRepository,
    WorkspaceRepository,
};
use cshell_domain::ClipboardHost;
use cshell_storage::{SqliteProfileRepository, restore_backup};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::error::Error;
use std::path::Path;

async fn raw_pool(path: &Path) -> Result<sqlx::SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
}

#[tokio::test]
async fn clipboard_policy_cas_reopen_and_failed_writes_leave_other_revisions_untouched()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("policies.sqlite");
    let repository = SqliteProfileRepository::open(&database).await?;
    let initial = repository.load_clipboard_policy().await?;
    assert_eq!(initial.revision, 0);
    let profile_before = repository.load().await?;
    let host = ClipboardHost::new("EXAMPLE.test.", 22).ok_or("invalid host")?;
    let document = ClipboardPolicyDocument {
        blocked_hosts: vec![host],
        ..Default::default()
    };
    let saved = repository.save_clipboard_policy(0, &document).await?;
    assert_eq!(saved.revision, 1);
    assert_eq!(
        repository
            .save_clipboard_policy(0, &ClipboardPolicyDocument::default())
            .await
            .err()
            .ok_or("expected policy rejection")?,
        ClipboardPolicyError::Conflict
    );
    assert_eq!(repository.load().await?, profile_before);
    assert!(repository.load_workspace().await?.is_none());
    let raw = raw_pool(&database).await?;
    sqlx::query("CREATE TRIGGER deny_policy_update BEFORE UPDATE ON clipboard_policy_state BEGIN SELECT RAISE(FAIL, 'injected failure'); END").execute(&raw).await?;
    assert_eq!(
        repository
            .save_clipboard_policy(1, &ClipboardPolicyDocument::default())
            .await
            .err()
            .ok_or("expected policy rejection")?,
        ClipboardPolicyError::Unavailable
    );
    assert_eq!(repository.load_clipboard_policy().await?, saved);
    raw.close().await;
    repository.close().await;
    let reopened = SqliteProfileRepository::open(&database).await?;
    assert_eq!(reopened.load_clipboard_policy().await?, saved);
    reopened.close().await;
    Ok(())
}

#[tokio::test]
async fn clipboard_policy_v7_upgrade_keeps_backup_and_restores_without_grants()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("v7.sqlite");
    let repository = SqliteProfileRepository::open(&database).await?;
    let before = repository.load().await?;
    repository.close().await;
    let raw = raw_pool(&database).await?;
    sqlx::query("DROP TABLE clipboard_policy_state")
        .execute(&raw)
        .await?;
    sqlx::query("PRAGMA user_version = 7").execute(&raw).await?;
    raw.close().await;
    let repository = SqliteProfileRepository::open(&database).await?;
    assert_eq!(repository.load().await?, before);
    assert!(
        repository
            .load_clipboard_policy()
            .await?
            .document
            .blocked_hosts
            .is_empty()
    );
    let mut files = tokio::fs::read_dir(directory.path()).await?;
    let mut backup = None;
    while let Some(entry) = files.next_entry().await? {
        if entry.file_name().to_string_lossy().contains(".backup-")
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "sqlite")
        {
            backup = Some(entry.path());
        }
    }
    let backup = backup.ok_or("missing migration backup")?;
    let backup_pool = raw_pool(&backup).await?;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&backup_pool)
        .await?;
    assert_eq!(version, 7);
    backup_pool.close().await;
    repository
        .save_clipboard_policy(
            0,
            &ClipboardPolicyDocument {
                blocked_hosts: vec![ClipboardHost::new("example.test", 22).ok_or("invalid host")?],
                ..Default::default()
            },
        )
        .await?;
    repository.close().await;
    restore_backup(&database, &backup).await?;
    let restored = SqliteProfileRepository::open(&database).await?;
    assert_eq!(restored.load().await?, before);
    assert_eq!(restored.load_clipboard_policy().await?.revision, 0);
    restored.close().await;
    Ok(())
}
