use crate::SqliteProfileRepository;
use cshell_application::*;
use cshell_domain::TaskRunId;
use sha2::{Digest, Sha256};
use sqlx::Row;
fn io(_: sqlx::Error) -> TaskRepositoryError {
    TaskRepositoryError::Unavailable
}
fn corrupt(_: TaskError) -> TaskRepositoryError {
    TaskRepositoryError::Corrupt
}
#[async_trait::async_trait]
impl DownloadRepository for SqliteProfileRepository {
    async fn create_download(
        &self,
        snapshot: &BatchDownloadSnapshot,
    ) -> Result<(), TaskRepositoryError> {
        let bytes = encode_download_snapshot(snapshot).map_err(corrupt)?;
        let mut tx = self.pool.begin().await.map_err(io)?;
        let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM download_runs WHERE id=?")
            .bind(snapshot.id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(io)?;
        if exists != 0 {
            return Err(TaskRepositoryError::Conflict);
        }
        sqlx::query("INSERT INTO download_runs(id,name,created_at_ms,snapshot_json,snapshot_sha256,target_count) VALUES(?,?,?,?,?,?)")
            .bind(snapshot.id.to_string()).bind(&snapshot.definition.name).bind(snapshot.created_at_ms as i64).bind(&bytes)
            .bind(Sha256::digest(&bytes).as_slice()).bind(snapshot.targets.targets().len() as i64).execute(&mut *tx).await.map_err(io)?;
        for host in snapshot.targets.targets() {
            let row = DownloadTarget::initial(snapshot, host.id);
            sqlx::query("INSERT INTO download_targets(run_id,host_id,revision,phase,row_json) VALUES(?,?,1,0,?)")
                .bind(snapshot.id.to_string()).bind(host.id.to_string()).bind(encode_download_target(&row, snapshot).map_err(corrupt)?).execute(&mut *tx).await.map_err(io)?;
            insert_event(&mut tx, &row).await?;
        }
        tx.commit().await.map_err(io)
    }
    async fn load_download(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchDownloadSnapshot, Vec<DownloadTarget>)>, TaskRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let Some(snapshot) = load_snapshot(&mut tx, id).await? else {
            return Ok(None);
        };
        let rows = sqlx::query("SELECT host_id,revision,phase,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM download_targets WHERE run_id=? ORDER BY host_id LIMIT 1001")
            .bind(MAX_UPLOAD_ROW_BYTES as i64).bind(id.to_string()).fetch_all(&mut *tx).await.map_err(io)?;
        if rows.len() != snapshot.targets.targets().len() {
            return Err(TaskRepositoryError::Corrupt);
        }
        let mut result = Vec::with_capacity(rows.len());
        for (saved, host) in rows.iter().zip(snapshot.targets.targets()) {
            let bytes: Option<Vec<u8>> = saved.try_get("row_json").map_err(io)?;
            let row =
                decode_download_target(&bytes.ok_or(TaskRepositoryError::Corrupt)?, &snapshot)
                    .map_err(corrupt)?;
            let host_id: String = saved.try_get("host_id").map_err(io)?;
            let revision: i64 = saved.try_get("revision").map_err(io)?;
            let phase: i32 = saved.try_get("phase").map_err(io)?;
            if row.host_id != host.id
                || host_id != host.id.to_string()
                || revision <= 0
                || revision as u64 != row.revision
                || phase != row.phase.code()
            {
                return Err(TaskRepositoryError::Corrupt);
            }
            result.push(row);
        }
        tx.commit().await.map_err(io)?;
        Ok(Some((snapshot, result)))
    }
    async fn commit_download_target(
        &self,
        expected: &DownloadTarget,
        next: &DownloadTarget,
    ) -> Result<(), TaskRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let snapshot = load_snapshot(&mut tx, expected.run_id)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        expected.validate_next(next, &snapshot).map_err(corrupt)?;
        let saved = sqlx::query("SELECT revision,phase,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM download_targets WHERE run_id=? AND host_id=?")
            .bind(MAX_UPLOAD_ROW_BYTES as i64).bind(expected.run_id.to_string()).bind(expected.host_id.to_string()).fetch_optional(&mut *tx).await.map_err(io)?.ok_or(TaskRepositoryError::Missing)?;
        let bytes: Option<Vec<u8>> = saved.try_get("row_json").map_err(io)?;
        let actual = decode_download_target(&bytes.ok_or(TaskRepositoryError::Corrupt)?, &snapshot)
            .map_err(corrupt)?;
        let revision: i64 = saved.try_get("revision").map_err(io)?;
        let phase: i32 = saved.try_get("phase").map_err(io)?;
        if revision <= 0 || revision as u64 != actual.revision || phase != actual.phase.code() {
            return Err(TaskRepositoryError::Corrupt);
        }
        if actual != *expected {
            return Err(TaskRepositoryError::Conflict);
        }
        let updated = sqlx::query("UPDATE download_targets SET revision=?,phase=?,row_json=? WHERE run_id=? AND host_id=? AND revision=?")
            .bind(next.revision as i64).bind(next.phase.code()).bind(encode_download_target(next, &snapshot).map_err(corrupt)?)
            .bind(next.run_id.to_string()).bind(next.host_id.to_string()).bind(expected.revision as i64).execute(&mut *tx).await.map_err(io)?;
        if updated.rows_affected() != 1 {
            return Err(TaskRepositoryError::Conflict);
        }
        insert_event(&mut tx, next).await?;
        tx.commit().await.map_err(io)
    }
    async fn list_downloads(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<DownloadRunSummary>, TaskRepositoryError> {
        if limit == 0 || limit > 64 {
            return Err(TaskRepositoryError::Corrupt);
        }
        let rows = sqlx::query("SELECT id,name,created_at_ms,target_count FROM download_runs WHERE id>? ORDER BY id LIMIT ?")
            .bind(after.map(|id| id.to_string()).unwrap_or_default()).bind(limit as i64).fetch_all(&self.pool).await.map_err(io)?;
        let mut result = Vec::with_capacity(rows.len());
        for saved in rows {
            let id: String = saved.try_get("id").map_err(io)?;
            let created: i64 = saved.try_get("created_at_ms").map_err(io)?;
            let count: i64 = saved.try_get("target_count").map_err(io)?;
            if created < 0 || !(1..=1000).contains(&count) {
                return Err(TaskRepositoryError::Corrupt);
            }
            result.push(DownloadRunSummary {
                id: serde_json::from_value(serde_json::Value::String(id))
                    .map_err(|_| TaskRepositoryError::Corrupt)?,
                name: saved.try_get("name").map_err(io)?,
                created_at_ms: created as u64,
                targets: count as u64,
            });
        }
        decode_download_list(
            &serde_json::to_vec(&result).map_err(|_| TaskRepositoryError::Corrupt)?,
        )
        .map_err(corrupt)
    }
}
async fn load_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: TaskRunId,
) -> Result<Option<BatchDownloadSnapshot>, TaskRepositoryError> {
    let saved = sqlx::query("SELECT name,created_at_ms,snapshot_sha256,target_count,CASE WHEN length(snapshot_json)<=? THEN snapshot_json ELSE NULL END AS snapshot_json FROM download_runs WHERE id=?")
        .bind(MAX_TASK_SNAPSHOT_BYTES as i64).bind(id.to_string()).fetch_optional(&mut **tx).await.map_err(io)?;
    let Some(saved) = saved else {
        return Ok(None);
    };
    let bytes: Option<Vec<u8>> = saved.try_get("snapshot_json").map_err(io)?;
    let bytes = bytes.ok_or(TaskRepositoryError::Corrupt)?;
    let hash: Vec<u8> = saved.try_get("snapshot_sha256").map_err(io)?;
    if hash.as_slice() != Sha256::digest(&bytes).as_slice() {
        return Err(TaskRepositoryError::Corrupt);
    }
    let snapshot = decode_download_snapshot(&bytes).map_err(corrupt)?;
    let name: String = saved.try_get("name").map_err(io)?;
    let created: i64 = saved.try_get("created_at_ms").map_err(io)?;
    let count: i64 = saved.try_get("target_count").map_err(io)?;
    if snapshot.id != id
        || name != snapshot.definition.name
        || created < 0
        || created as u64 != snapshot.created_at_ms
        || count != snapshot.targets.targets().len() as i64
    {
        return Err(TaskRepositoryError::Corrupt);
    }
    Ok(Some(snapshot))
}
async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &DownloadTarget,
) -> Result<(), TaskRepositoryError> {
    let event = serde_json::to_vec(&serde_json::json!({"phase":row.phase,"intent":row.intent,"bytes":row.bytes,"content":row.content,"owned":row.temporary_owned,
        "verified":row.verified,"committed":row.committed,"adopted":row.adopted,"cleaned":row.temporary_cleaned,"reason":row.reason,"time":row.updated_at_ms})).map_err(|_| TaskRepositoryError::Corrupt)?;
    sqlx::query("INSERT INTO download_events(run_id,host_id,revision,event_json) VALUES(?,?,?,?)")
        .bind(row.run_id.to_string())
        .bind(row.host_id.to_string())
        .bind(row.revision as i64)
        .bind(event)
        .execute(&mut **tx)
        .await
        .map_err(io)?;
    Ok(())
}
