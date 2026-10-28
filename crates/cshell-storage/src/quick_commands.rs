use crate::SqliteProfileRepository;
use cshell_application::{
    QuickCommandRepository, QuickCommandRepositoryError, QuickCommandSnapshot,
    decode_quick_commands, encode_quick_commands,
};
use cshell_domain::QuickCommandCatalog;
use sqlx::Row;

#[async_trait::async_trait]
impl QuickCommandRepository for SqliteProfileRepository {
    async fn load_quick_commands(
        &self,
    ) -> Result<Option<QuickCommandSnapshot>, QuickCommandRepositoryError> {
        let row =
            sqlx::query("SELECT revision, document_json FROM quick_command_state WHERE id = 1")
                .fetch_optional(&self.pool)
                .await
                .map_err(|_| QuickCommandRepositoryError::Unavailable)?;
        row.map(|row| {
            let revision: i64 = row
                .try_get("revision")
                .map_err(|_| QuickCommandRepositoryError::Corrupt)?;
            let bytes: Vec<u8> = row
                .try_get("document_json")
                .map_err(|_| QuickCommandRepositoryError::Corrupt)?;
            Ok(QuickCommandSnapshot {
                revision: u64::try_from(revision)
                    .map_err(|_| QuickCommandRepositoryError::Corrupt)?,
                catalog: decode_quick_commands(&bytes)
                    .map_err(|_| QuickCommandRepositoryError::Corrupt)?,
            })
        })
        .transpose()
    }
    async fn save_quick_commands(
        &self,
        expected_revision: u64,
        catalog: &QuickCommandCatalog,
    ) -> Result<QuickCommandSnapshot, QuickCommandRepositoryError> {
        let bytes =
            encode_quick_commands(catalog).map_err(|_| QuickCommandRepositoryError::Corrupt)?;
        let next = expected_revision
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(QuickCommandRepositoryError::Unavailable)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| QuickCommandRepositoryError::Unavailable)?;
        let actual: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM quick_command_state WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| QuickCommandRepositoryError::Unavailable)?;
        let actual =
            u64::try_from(actual.unwrap_or(0)).map_err(|_| QuickCommandRepositoryError::Corrupt)?;
        if actual != expected_revision {
            return Err(QuickCommandRepositoryError::Conflict {
                expected: expected_revision,
                actual,
            });
        }
        sqlx::query("INSERT INTO quick_command_state (id,revision,document_json) VALUES (1,?,?) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision, document_json=excluded.document_json").bind(next).bind(bytes).execute(&mut *tx).await.map_err(|_|QuickCommandRepositoryError::Unavailable)?;
        tx.commit()
            .await
            .map_err(|_| QuickCommandRepositoryError::Unavailable)?;
        Ok(QuickCommandSnapshot {
            revision: next as u64,
            catalog: catalog.clone(),
        })
    }
}
