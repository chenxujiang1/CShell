use crate::SqliteProfileRepository;
use cshell_application::*;
use cshell_domain::TaskRunId;
use sha2::{Digest, Sha256};
use sqlx::Row;
fn io(_: sqlx::Error) -> UploadRepositoryError {
    UploadRepositoryError::Unavailable
}
fn corrupt(_: TaskError) -> UploadRepositoryError {
    UploadRepositoryError::Corrupt
}
#[async_trait::async_trait]
impl UploadRepository for SqliteProfileRepository {
    async fn list_uploads(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<UploadRunSummary>, UploadRepositoryError> {
        if limit == 0 || limit > 64 {
            return Err(UploadRepositoryError::Corrupt);
        }
        let rows = sqlx::query("SELECT id,name,created_at_ms,target_count FROM upload_runs WHERE id>? ORDER BY id LIMIT ?")
            .bind(after.map(|id| id.to_string()).unwrap_or_default()).bind(limit as i64).fetch_all(&self.pool).await.map_err(io)?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id").map_err(io)?;
            let id: TaskRunId = serde_json::from_value(serde_json::Value::String(id))
                .map_err(|_| UploadRepositoryError::Corrupt)?;
            let created: i64 = row.try_get("created_at_ms").map_err(io)?;
            let count: i64 = row.try_get("target_count").map_err(io)?;
            if created < 0 || !(1..=1000).contains(&count) {
                return Err(UploadRepositoryError::Corrupt);
            }
            result.push(UploadRunSummary {
                id,
                name: row.try_get("name").map_err(io)?,
                created_at_ms: created as u64,
                targets: count as u64,
            });
        }
        let bytes = serde_json::to_vec(&result).map_err(|_| UploadRepositoryError::Corrupt)?;
        decode_upload_list(&bytes).map_err(corrupt)?;
        Ok(result)
    }
    async fn create_upload(
        &self,
        snapshot: &BatchUploadSnapshot,
    ) -> Result<(), UploadRepositoryError> {
        let bytes = encode_upload_snapshot(snapshot).map_err(corrupt)?;
        let mut tx = self.pool.begin().await.map_err(io)?;
        let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM upload_runs WHERE id=?")
            .bind(snapshot.id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(io)?;
        if exists != 0 {
            return Err(UploadRepositoryError::Conflict);
        }
        sqlx::query("INSERT INTO upload_runs(id,snapshot_json,snapshot_sha256,target_count,name,created_at_ms) VALUES(?,?,?,?,?,?)")
            .bind(snapshot.id.to_string()).bind(&bytes).bind(Sha256::digest(&bytes).as_slice()).bind(snapshot.targets.targets().len() as i64).bind(&snapshot.definition.name).bind(snapshot.created_at_ms as i64)
            .execute(&mut *tx).await.map_err(io)?;
        for host in snapshot.targets.targets() {
            let row = UploadTarget::initial(snapshot, host.id);
            sqlx::query("INSERT INTO upload_targets(run_id,host_id,revision,phase,row_json) VALUES(?,?,1,0,?)")
                .bind(snapshot.id.to_string()).bind(host.id.to_string()).bind(encode_upload_target(&row).map_err(corrupt)?)
                .execute(&mut *tx).await.map_err(io)?;
            insert_event(&mut tx, &row).await?;
        }
        tx.commit().await.map_err(io)
    }
    async fn load_upload(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchUploadSnapshot, Vec<UploadTarget>)>, UploadRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let Some(snapshot) = load_snapshot(&mut tx, id).await? else {
            return Ok(None);
        };
        let rows = sqlx::query("SELECT host_id,revision,phase,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM upload_targets WHERE run_id=? ORDER BY host_id LIMIT 1001")
            .bind(MAX_UPLOAD_ROW_BYTES as i64).bind(id.to_string()).fetch_all(&mut *tx).await.map_err(io)?;
        if rows.len() != snapshot.targets.targets().len() {
            return Err(UploadRepositoryError::Corrupt);
        }
        let mut targets = Vec::with_capacity(rows.len());
        for (row, host) in rows.iter().zip(snapshot.targets.targets()) {
            let bytes: Option<Vec<u8>> = row.try_get("row_json").map_err(io)?;
            let target =
                decode_upload_target(&bytes.ok_or(UploadRepositoryError::Corrupt)?, &snapshot)
                    .map_err(corrupt)?;
            let host_id: String = row.try_get("host_id").map_err(io)?;
            let revision: i64 = row.try_get("revision").map_err(io)?;
            let phase: i32 = row.try_get("phase").map_err(io)?;
            if target.host_id != host.id
                || host_id != host.id.to_string()
                || revision <= 0
                || revision as u64 != target.revision
                || phase != target.phase.code()
            {
                return Err(UploadRepositoryError::Corrupt);
            }
            targets.push(target);
        }
        tx.commit().await.map_err(io)?;
        Ok(Some((snapshot, targets)))
    }
    async fn commit_upload_target(
        &self,
        expected: &UploadTarget,
        next: &UploadTarget,
    ) -> Result<(), UploadRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let snapshot = load_snapshot(&mut tx, expected.run_id)
            .await?
            .ok_or(UploadRepositoryError::Missing)?;
        expected.validate_next(next, &snapshot).map_err(corrupt)?;
        let saved = sqlx::query("SELECT revision,phase,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM upload_targets WHERE run_id=? AND host_id=?")
            .bind(MAX_UPLOAD_ROW_BYTES as i64).bind(expected.run_id.to_string()).bind(expected.host_id.to_string())
            .fetch_optional(&mut *tx).await.map_err(io)?.ok_or(UploadRepositoryError::Missing)?;
        let revision: i64 = saved.try_get("revision").map_err(io)?;
        let phase: i32 = saved.try_get("phase").map_err(io)?;
        let bytes: Option<Vec<u8>> = saved.try_get("row_json").map_err(io)?;
        let actual = decode_upload_target(&bytes.ok_or(UploadRepositoryError::Corrupt)?, &snapshot)
            .map_err(corrupt)?;
        if revision <= 0 || revision as u64 != actual.revision || phase != actual.phase.code() {
            return Err(UploadRepositoryError::Corrupt);
        }
        if actual != *expected {
            return Err(UploadRepositoryError::Conflict);
        }
        let updated = sqlx::query("UPDATE upload_targets SET revision=?,phase=?,row_json=? WHERE run_id=? AND host_id=? AND revision=?")
            .bind(next.revision as i64).bind(next.phase.code()).bind(encode_upload_target(next).map_err(corrupt)?)
            .bind(next.run_id.to_string()).bind(next.host_id.to_string()).bind(expected.revision as i64)
            .execute(&mut *tx).await.map_err(io)?;
        if updated.rows_affected() != 1 {
            return Err(UploadRepositoryError::Conflict);
        }
        insert_event(&mut tx, next).await?;
        tx.commit().await.map_err(io)
    }
}
async fn load_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: TaskRunId,
) -> Result<Option<BatchUploadSnapshot>, UploadRepositoryError> {
    let row = sqlx::query("SELECT CASE WHEN length(snapshot_json)<=? THEN snapshot_json ELSE NULL END AS snapshot_json,snapshot_sha256,target_count,name,created_at_ms FROM upload_runs WHERE id=?")
        .bind(MAX_TASK_SNAPSHOT_BYTES as i64).bind(id.to_string()).fetch_optional(&mut **tx).await.map_err(io)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let bytes: Option<Vec<u8>> = row.try_get("snapshot_json").map_err(io)?;
    let bytes = bytes.ok_or(UploadRepositoryError::Corrupt)?;
    let hash: Vec<u8> = row.try_get("snapshot_sha256").map_err(io)?;
    if hash.as_slice() != Sha256::digest(&bytes).as_slice() {
        return Err(UploadRepositoryError::Corrupt);
    }
    let snapshot = decode_upload_snapshot(&bytes).map_err(corrupt)?;
    let count: i64 = row.try_get("target_count").map_err(io)?;
    let name: String = row.try_get("name").map_err(io)?;
    let created: i64 = row.try_get("created_at_ms").map_err(io)?;
    if snapshot.id != id
        || count != snapshot.targets.targets().len() as i64
        || name != snapshot.definition.name
        || created < 0
        || created as u64 != snapshot.created_at_ms
    {
        return Err(UploadRepositoryError::Corrupt);
    }
    Ok(Some(snapshot))
}
async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &UploadTarget,
) -> Result<(), UploadRepositoryError> {
    let event = serde_json::to_vec(&serde_json::json!({"phase": row.phase, "intent": row.intent, "bytes": row.bytes,
        "owned": row.temporary_owned, "verified": row.verified, "committed": row.committed, "adopted": row.adopted,
        "cleaned": row.temporary_cleaned, "reason": row.reason, "time": row.updated_at_ms})).map_err(|_| UploadRepositoryError::Corrupt)?;
    sqlx::query("INSERT INTO upload_events(run_id,host_id,revision,event_json) VALUES(?,?,?,?)")
        .bind(row.run_id.to_string())
        .bind(row.host_id.to_string())
        .bind(row.revision as i64)
        .bind(event)
        .execute(&mut **tx)
        .await
        .map_err(io)?;
    Ok(())
}
