use crate::SqliteProfileRepository;
use async_trait::async_trait;
use cshell_application::{
    WorkspaceRepository, WorkspaceRepositoryError, WorkspaceSnapshot, decode_workspace,
    validate_workspace,
};
use cshell_domain::WorkspaceDocument;
use sqlx::{Row, query};

#[async_trait]
impl WorkspaceRepository for SqliteProfileRepository {
    async fn load_workspace(&self) -> Result<Option<WorkspaceSnapshot>, WorkspaceRepositoryError> {
        let row = query("SELECT revision, document_json FROM workspace_state WHERE id = 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| WorkspaceRepositoryError::Unavailable)?;
        row.map(|row| {
            let revision: i64 = row
                .try_get("revision")
                .map_err(|_| WorkspaceRepositoryError::Corrupt)?;
            let json: String = row
                .try_get("document_json")
                .map_err(|_| WorkspaceRepositoryError::Corrupt)?;
            Ok(WorkspaceSnapshot {
                revision: u64::try_from(revision).map_err(|_| WorkspaceRepositoryError::Corrupt)?,
                document: decode_workspace(json.as_bytes())
                    .map_err(|_| WorkspaceRepositoryError::Corrupt)?,
            })
        })
        .transpose()
    }

    async fn save_workspace(
        &self,
        expected_revision: u64,
        document: &WorkspaceDocument,
    ) -> Result<WorkspaceSnapshot, WorkspaceRepositoryError> {
        validate_workspace(document).map_err(|_| WorkspaceRepositoryError::Corrupt)?;
        let json =
            serde_json::to_string(document).map_err(|_| WorkspaceRepositoryError::Corrupt)?;
        let next = expected_revision
            .checked_add(1)
            .and_then(|r| i64::try_from(r).ok())
            .ok_or(WorkspaceRepositoryError::Unavailable)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| WorkspaceRepositoryError::Unavailable)?;
        let actual: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM workspace_state WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| WorkspaceRepositoryError::Unavailable)?;
        let actual =
            u64::try_from(actual.unwrap_or(0)).map_err(|_| WorkspaceRepositoryError::Corrupt)?;
        if actual != expected_revision {
            return Err(WorkspaceRepositoryError::Conflict {
                expected: expected_revision,
                actual,
            });
        }
        query("INSERT INTO workspace_state (id, revision, document_json) VALUES (1, ?, ?)
               ON CONFLICT(id) DO UPDATE SET revision = excluded.revision, document_json = excluded.document_json")
            .bind(next).bind(json).execute(&mut *tx).await.map_err(|_| WorkspaceRepositoryError::Unavailable)?;
        tx.commit()
            .await
            .map_err(|_| WorkspaceRepositoryError::Unavailable)?;
        Ok(WorkspaceSnapshot {
            revision: next as u64,
            document: document.clone(),
        })
    }
}
