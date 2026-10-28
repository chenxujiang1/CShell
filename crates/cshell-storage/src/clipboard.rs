use crate::SqliteProfileRepository;
use async_trait::async_trait;
use cshell_application::{
    ClipboardPolicyDocument, ClipboardPolicyError, ClipboardPolicyRepository,
    ClipboardPolicySnapshot, MAX_CLIPBOARD_POLICY_BYTES, decode_clipboard_policy,
    validate_clipboard_policy,
};
use sqlx::{Row, query};

#[async_trait]
impl ClipboardPolicyRepository for SqliteProfileRepository {
    async fn load_clipboard_policy(&self) -> Result<ClipboardPolicySnapshot, ClipboardPolicyError> {
        let row = query("SELECT revision, document_json FROM clipboard_policy_state WHERE id = 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| ClipboardPolicyError::Unavailable)?;
        let Some(row) = row else {
            return Ok(ClipboardPolicySnapshot::default());
        };
        let revision: i64 = row
            .try_get("revision")
            .map_err(|_| ClipboardPolicyError::Invalid)?;
        let json: String = row
            .try_get("document_json")
            .map_err(|_| ClipboardPolicyError::Invalid)?;
        Ok(ClipboardPolicySnapshot {
            revision: u64::try_from(revision).map_err(|_| ClipboardPolicyError::Invalid)?,
            document: decode_clipboard_policy(json.as_bytes())?,
        })
    }
    async fn save_clipboard_policy(
        &self,
        expected_revision: u64,
        document: &ClipboardPolicyDocument,
    ) -> Result<ClipboardPolicySnapshot, ClipboardPolicyError> {
        validate_clipboard_policy(document)?;
        let json = serde_json::to_string(document).map_err(|_| ClipboardPolicyError::Invalid)?;
        if json.len() > MAX_CLIPBOARD_POLICY_BYTES {
            return Err(ClipboardPolicyError::Invalid);
        }
        let next = expected_revision
            .checked_add(1)
            .and_then(|r| i64::try_from(r).ok())
            .ok_or(ClipboardPolicyError::Invalid)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| ClipboardPolicyError::Unavailable)?;
        let actual: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM clipboard_policy_state WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| ClipboardPolicyError::Unavailable)?;
        if u64::try_from(actual.unwrap_or(0)).map_err(|_| ClipboardPolicyError::Invalid)?
            != expected_revision
        {
            return Err(ClipboardPolicyError::Conflict);
        }
        query("INSERT INTO clipboard_policy_state (id, revision, document_json) VALUES (1, ?, ?) ON CONFLICT(id) DO UPDATE SET revision = excluded.revision, document_json = excluded.document_json")
            .bind(next).bind(json).execute(&mut *tx).await.map_err(|_| ClipboardPolicyError::Unavailable)?;
        tx.commit()
            .await
            .map_err(|_| ClipboardPolicyError::Unavailable)?;
        Ok(ClipboardPolicySnapshot {
            revision: next as u64,
            document: document.clone(),
        })
    }
}
