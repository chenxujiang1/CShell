use cshell_application::{ProfileRepository, WorkspaceRepository, WorkspaceRepositoryError};
use cshell_domain::{ProfileId, TabId, WorkspaceBinding, WorkspaceDocument};
use cshell_storage::{SqliteProfileRepository, restore_backup};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{SqlitePool, query, query_scalar};
use std::{error::Error, path::Path};

async fn raw(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
        )
        .await
}

fn document(title: &str) -> WorkspaceDocument {
    let mut doc = WorkspaceDocument::default();
    let tab = TabId::new();
    doc.tab_groups[0].tabs.push(tab);
    doc.tab_groups[0].active_tab = Some(tab);
    doc.bindings.push(WorkspaceBinding {
        tab_id: tab,
        profile_id: Some(ProfileId::new()),
        title: title.into(),
    });
    doc
}

#[tokio::test]
async fn workspace_cas_failure_backup_and_restore_preserve_committed_layout()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("workspace.db");
    let first = SqliteProfileRepository::open(&path).await?;
    let second = SqliteProfileRepository::open(&path).await?;
    assert!(first.load_workspace().await?.is_none());
    let saved = first.save_workspace(0, &document("first")).await?;
    assert_eq!(saved.revision, 1);
    let backup = first.backup().await?;
    assert!(matches!(
        second.save_workspace(0, &document("stale")).await,
        Err(WorkspaceRepositoryError::Conflict {
            expected: 0,
            actual: 1
        })
    ));
    assert_eq!(second.load_workspace().await?, Some(saved.clone()));
    let pool = raw(&path).await?;
    query("CREATE TRIGGER reject_workspace BEFORE UPDATE ON workspace_state BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&pool).await?;
    assert_eq!(
        first.save_workspace(1, &document("blocked")).await,
        Err(WorkspaceRepositoryError::Unavailable)
    );
    assert_eq!(first.load_workspace().await?, Some(saved.clone()));
    query("DROP TRIGGER reject_workspace")
        .execute(&pool)
        .await?;
    pool.close().await;
    first.save_workspace(1, &document("newer")).await?;
    assert_eq!(first.load().await?.revision, 0); // layout has an independent revision.
    first.close().await;
    second.close().await;
    restore_backup(&path, &backup).await?;
    let restored = SqliteProfileRepository::open(&path).await?;
    assert_eq!(restored.load_workspace().await?, Some(saved));
    restored.close().await;
    Ok(())
}

#[tokio::test]
async fn v6_upgrade_backs_up_original_and_corruption_is_not_replaced() -> Result<(), Box<dyn Error>>
{
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("v6.db");
    let repository = SqliteProfileRepository::open(&path).await?;
    let catalog = repository.load().await?;
    repository.close().await;
    let pool = raw(&path).await?;
    query("DROP TABLE workspace_state").execute(&pool).await?;
    query("PRAGMA user_version = 6").execute(&pool).await?;
    pool.close().await;
    let repository = SqliteProfileRepository::open(&path).await?;
    assert_eq!(repository.load().await?, catalog);
    assert!(repository.load_workspace().await?.is_none());
    let backup = std::fs::read_dir(temp.path())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.contains(".backup-"))
        })
        .ok_or("missing migration backup")?;
    let pool = raw(&backup).await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await?,
        6
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master WHERE name = 'workspace_state'")
            .fetch_one(&pool)
            .await?,
        0
    );
    pool.close().await;
    let pool = raw(&path).await?;
    query("INSERT INTO workspace_state VALUES (1, 1, '{\"format\":\"future\"}')")
        .execute(&pool)
        .await?;
    assert_eq!(
        repository.load_workspace().await,
        Err(WorkspaceRepositoryError::Corrupt)
    );
    assert!(matches!(
        repository.save_workspace(0, &document("overwrite")).await,
        Err(WorkspaceRepositoryError::Conflict { .. })
    ));
    assert_eq!(
        query_scalar::<_, String>("SELECT document_json FROM workspace_state")
            .fetch_one(&pool)
            .await?,
        "{\"format\":\"future\"}"
    );
    pool.close().await;
    repository.close().await;
    Ok(())
}
