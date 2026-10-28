use crate::SqliteProfileRepository;
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use sha2::{Digest, Sha256};
use sqlx::Row;
fn io(_: sqlx::Error) -> TaskRepositoryError {
    TaskRepositoryError::Unavailable
}
fn corrupt(_: TaskError) -> TaskRepositoryError {
    TaskRepositoryError::Corrupt
}
#[async_trait::async_trait]
impl MixedTaskRepository for SqliteProfileRepository {
    async fn list_mixed_tasks(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<MixedRunSummary>, TaskRepositoryError> {
        if limit == 0 || limit > 64 {
            return Err(TaskRepositoryError::Corrupt);
        }
        let rows = sqlx::query("SELECT id,name,created_at_ms,target_count FROM mixed_runs WHERE id>? ORDER BY id LIMIT ?")
            .bind(after.map(|id| id.to_string()).unwrap_or_default()).bind(limit as i64).fetch_all(&self.pool).await.map_err(io)?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id").map_err(io)?;
            let created: i64 = row.try_get("created_at_ms").map_err(io)?;
            let count: i64 = row.try_get("target_count").map_err(io)?;
            if created < 0 || !(1..=1000).contains(&count) {
                return Err(TaskRepositoryError::Corrupt);
            }
            result.push(MixedRunSummary {
                id: serde_json::from_value(serde_json::Value::String(id))
                    .map_err(|_| TaskRepositoryError::Corrupt)?,
                name: row.try_get("name").map_err(io)?,
                created_at_ms: created as u64,
                targets: count as u64,
            });
        }
        decode_mixed_list(&serde_json::to_vec(&result).map_err(|_| TaskRepositoryError::Corrupt)?)
            .map_err(corrupt)
    }
    async fn load_mixed_page(
        &self,
        id: TaskRunId,
        after: Option<HostId>,
        limit: usize,
    ) -> Result<Option<MixedPageData>, TaskRepositoryError> {
        if limit == 0
            || limit > MAX_MIXED_STATUS_TARGETS
            || after.is_some_and(|id| id.as_uuid().is_nil())
        {
            return Err(TaskRepositoryError::Corrupt);
        }
        let mut tx = self.pool.begin().await.map_err(io)?;
        let Some(snapshot) = load_snapshot(&mut tx, id).await? else {
            return Ok(None);
        };
        let groups = sqlx::query(
            "SELECT state,count(*) AS count FROM mixed_targets WHERE run_id=? GROUP BY state",
        )
        .bind(id.to_string())
        .fetch_all(&mut *tx)
        .await
        .map_err(io)?;
        let mut counts = [0_u64; 7];
        for row in groups {
            let state: i32 = row.try_get("state").map_err(io)?;
            let count: i64 = row.try_get("count").map_err(io)?;
            if !(0..=6).contains(&state) || !(1..=1000).contains(&count) {
                return Err(TaskRepositoryError::Corrupt);
            }
            counts[state as usize] = count as u64;
        }
        if counts.iter().sum::<u64>() != snapshot.targets().targets().len() as u64 {
            return Err(TaskRepositoryError::Corrupt);
        }
        let pending_rows = sqlx::query("SELECT host_id,revision FROM mixed_targets WHERE run_id=? AND state=0 ORDER BY host_id LIMIT 1001").bind(id.to_string()).fetch_all(&mut *tx).await.map_err(io)?;
        let mut pending = Vec::with_capacity(pending_rows.len());
        for row in pending_rows {
            let host: String = row.try_get("host_id").map_err(io)?;
            let revision: i64 = row.try_get("revision").map_err(io)?;
            let host_id: HostId = serde_json::from_value(serde_json::Value::String(host))
                .map_err(|_| TaskRepositoryError::Corrupt)?;
            if revision <= 0
                || snapshot
                    .targets()
                    .targets()
                    .binary_search_by_key(&host_id, |host| host.id)
                    .is_err()
            {
                return Err(TaskRepositoryError::Corrupt);
            }
            pending.push(MixedTargetRevision {
                host_id,
                revision: revision as u64,
            });
        }
        if pending.len() as u64 != counts[0] {
            return Err(TaskRepositoryError::Corrupt);
        }
        let saved = sqlx::query("SELECT host_id,revision,state,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM mixed_targets WHERE run_id=? AND host_id>? ORDER BY host_id LIMIT ?")
            .bind(MAX_MIXED_TARGET_BYTES as i64).bind(id.to_string()).bind(after.map(|id| id.to_string()).unwrap_or_default()).bind((limit + 1) as i64).fetch_all(&mut *tx).await.map_err(io)?;
        let more = saved.len() > limit;
        let mut rows = Vec::with_capacity(limit);
        for saved in saved.into_iter().take(limit) {
            let host: String = saved.try_get("host_id").map_err(io)?;
            let host: HostId = serde_json::from_value(serde_json::Value::String(host))
                .map_err(|_| TaskRepositoryError::Corrupt)?;
            rows.push(read_target(&saved, &snapshot, host)?);
        }
        let next = more.then(|| rows.last().map(|row| row.host_id)).flatten();
        tx.commit().await.map_err(io)?;
        Ok(Some(MixedPageData {
            snapshot,
            rows,
            counts,
            pending,
            after,
            next,
            limit,
        }))
    }
    async fn create_mixed_task(
        &self,
        snapshot: &MixedTaskSnapshot,
    ) -> Result<(), TaskRepositoryError> {
        let bytes = encode_mixed_snapshot(snapshot).map_err(corrupt)?;
        let mut tx = self.pool.begin().await.map_err(io)?;
        let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM mixed_runs WHERE id=?")
            .bind(snapshot.id().to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(io)?;
        if exists != 0 {
            return Err(TaskRepositoryError::Conflict);
        }
        if let Some(proof) = snapshot.retry_provenance() {
            let parent = load_snapshot(&mut tx, proof.parent_id)
                .await?
                .ok_or(TaskRepositoryError::Missing)?;
            snapshot.validate_retry_parent(&parent).map_err(corrupt)?;
            let mut source_rows = Vec::with_capacity(proof.sources.len());
            for source in &proof.sources {
                let saved=sqlx::query("SELECT host_id,revision,state,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM mixed_targets WHERE run_id=? AND host_id=?")
                    .bind(MAX_MIXED_TARGET_BYTES as i64).bind(parent.id().to_string()).bind(source.host_id.to_string()).fetch_optional(&mut *tx).await.map_err(io)?.ok_or(TaskRepositoryError::Missing)?;
                let row = read_target(&saved, &parent, source.host_id)?;
                if row.revision != source.revision {
                    return Err(TaskRepositoryError::Conflict);
                }
                source_rows.push(row);
            }
            snapshot
                .validate_retry_rows(&parent, &source_rows)
                .map_err(|_| TaskRepositoryError::Conflict)?;
        }
        sqlx::query("INSERT INTO mixed_runs(id,name,created_at_ms,snapshot_json,snapshot_sha256,target_count) VALUES(?,?,?,?,?,?)")
            .bind(snapshot.id().to_string()).bind(&snapshot.definition().name).bind(snapshot.created_at_ms() as i64).bind(&bytes)
            .bind(Sha256::digest(&bytes).as_slice()).bind(snapshot.targets().targets().len() as i64).execute(&mut *tx).await.map_err(io)?;
        for host in snapshot.targets().targets() {
            let row = MixedTargetRun::initial(snapshot, host.id).map_err(corrupt)?;
            sqlx::query("INSERT INTO mixed_targets(run_id,host_id,revision,state,row_json) VALUES(?,?,1,0,?)")
                .bind(snapshot.id().to_string()).bind(host.id.to_string()).bind(encode_mixed_target(&row, snapshot).map_err(corrupt)?).execute(&mut *tx).await.map_err(io)?;
            event(&mut tx, &row).await?;
        }
        tx.commit().await.map_err(io)
    }
    async fn load_mixed_task(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(MixedTaskSnapshot, Vec<MixedTargetRun>)>, TaskRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let Some(snapshot) = load_snapshot(&mut tx, id).await? else {
            return Ok(None);
        };
        let rows = sqlx::query("SELECT host_id,revision,state,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM mixed_targets WHERE run_id=? ORDER BY host_id LIMIT 1001")
            .bind(MAX_MIXED_TARGET_BYTES as i64).bind(id.to_string()).fetch_all(&mut *tx).await.map_err(io)?;
        if rows.len() != snapshot.targets().targets().len() {
            return Err(TaskRepositoryError::Corrupt);
        }
        let mut result = Vec::with_capacity(rows.len());
        for (saved, host) in rows.iter().zip(snapshot.targets().targets()) {
            result.push(read_target(saved, &snapshot, host.id)?);
        }
        tx.commit().await.map_err(io)?;
        Ok(Some((snapshot, result)))
    }
    async fn load_mixed_target(
        &self,
        id: TaskRunId,
        host: HostId,
    ) -> Result<Option<(MixedTaskSnapshot, MixedTargetRun)>, TaskRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(io)?;
        let Some(snapshot) = load_snapshot(&mut tx, id).await? else {
            return Ok(None);
        };
        let saved = sqlx::query("SELECT host_id,revision,state,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM mixed_targets WHERE run_id=? AND host_id=?")
            .bind(MAX_MIXED_TARGET_BYTES as i64).bind(id.to_string()).bind(host.to_string()).fetch_optional(&mut *tx).await.map_err(io)?;
        let Some(saved) = saved else {
            return Ok(None);
        };
        let row = read_target(&saved, &snapshot, host)?;
        tx.commit().await.map_err(io)?;
        Ok(Some((snapshot, row)))
    }
    async fn commit_mixed_target(
        &self,
        patch: &MixedTargetPatch,
    ) -> Result<u64, TaskRepositoryError> {
        let before = patch.before();
        let after = patch.after();
        let mut tx = self.pool.begin().await.map_err(io)?;
        let snapshot = load_snapshot(&mut tx, before.run_id)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        if snapshot.fingerprint() != patch.fingerprint() {
            return Err(TaskRepositoryError::Corrupt);
        }
        let saved = sqlx::query("SELECT host_id,revision,state,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM mixed_targets WHERE run_id=? AND host_id=?")
            .bind(MAX_MIXED_TARGET_BYTES as i64).bind(before.run_id.to_string()).bind(before.host_id.to_string()).fetch_optional(&mut *tx).await.map_err(io)?.ok_or(TaskRepositoryError::Missing)?;
        if read_target(&saved, &snapshot, before.host_id)? != *before {
            return Err(TaskRepositoryError::Conflict);
        }
        let result = sqlx::query("UPDATE mixed_targets SET revision=?,state=?,row_json=? WHERE run_id=? AND host_id=? AND revision=?")
            .bind(after.revision as i64).bind(after.state.code()).bind(encode_mixed_target(after, &snapshot).map_err(corrupt)?)
            .bind(before.run_id.to_string()).bind(before.host_id.to_string()).bind(before.revision as i64).execute(&mut *tx).await.map_err(io)?;
        if result.rows_affected() != 1 {
            return Err(TaskRepositoryError::Conflict);
        }
        event(&mut tx, after).await?;
        tx.commit().await.map_err(io)?;
        Ok(after.revision)
    }
}
fn read_target(
    saved: &sqlx::sqlite::SqliteRow,
    snapshot: &MixedTaskSnapshot,
    host: HostId,
) -> Result<MixedTargetRun, TaskRepositoryError> {
    let bytes: Option<Vec<u8>> = saved.try_get("row_json").map_err(io)?;
    let row = decode_mixed_target(&bytes.ok_or(TaskRepositoryError::Corrupt)?, snapshot)
        .map_err(corrupt)?;
    let host_id: String = saved.try_get("host_id").map_err(io)?;
    let revision: i64 = saved.try_get("revision").map_err(io)?;
    let state: i32 = saved.try_get("state").map_err(io)?;
    if row.host_id != host
        || host_id != host.to_string()
        || revision <= 0
        || revision as u64 != row.revision
        || state != row.state.code()
    {
        return Err(TaskRepositoryError::Corrupt);
    }
    Ok(row)
}
async fn load_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: TaskRunId,
) -> Result<Option<MixedTaskSnapshot>, TaskRepositoryError> {
    let saved = sqlx::query("SELECT name,created_at_ms,snapshot_sha256,target_count,CASE WHEN length(snapshot_json)<=? THEN snapshot_json ELSE NULL END AS snapshot_json FROM mixed_runs WHERE id=?")
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
    let snapshot = decode_mixed_snapshot(&bytes).map_err(corrupt)?;
    let name: String = saved.try_get("name").map_err(io)?;
    let created: i64 = saved.try_get("created_at_ms").map_err(io)?;
    let count: i64 = saved.try_get("target_count").map_err(io)?;
    if snapshot.id() != id
        || snapshot.fingerprint().as_slice() != hash.as_slice()
        || name != snapshot.definition().name
        || created < 0
        || created as u64 != snapshot.created_at_ms()
        || count != snapshot.targets().targets().len() as i64
    {
        return Err(TaskRepositoryError::Corrupt);
    }
    Ok(Some(snapshot))
}
async fn event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &MixedTargetRun,
) -> Result<(), TaskRepositoryError> {
    // Events carry metadata and receipt kinds, never definitions, commands or paths.
    let steps: Vec<_> = row.steps.iter().map(|step| match &step.receipt {
        MixedReceipt::Command { meta, target } => serde_json::json!({"kind":"command","state":step.state,"revision":meta.revision,"target_state":target.state,"exit_code":target.steps[0].exit_code,"reason":target.reason}),
        MixedReceipt::Upload { row } => serde_json::json!({"kind":"upload","state":step.state,"revision":row.revision,"phase":row.phase,"bytes":row.bytes,"committed":row.committed,"adopted":row.adopted,"cleaned":row.temporary_cleaned}),
        MixedReceipt::Download { row } => serde_json::json!({"kind":"download","state":step.state,"revision":row.revision,"phase":row.phase,"bytes":row.bytes,"committed":row.committed,"adopted":row.adopted,"cleaned":row.temporary_cleaned}),
    }).collect();
    let bytes = serde_json::to_vec(&serde_json::json!({"state":row.state,"next_step":row.next_step,"reason":row.reason,"time":row.updated_at_ms,"steps":steps})).map_err(|_| TaskRepositoryError::Corrupt)?;
    sqlx::query("INSERT INTO mixed_events(run_id,host_id,revision,event_json) VALUES(?,?,?,?)")
        .bind(row.run_id.to_string())
        .bind(row.host_id.to_string())
        .bind(row.revision as i64)
        .bind(bytes)
        .execute(&mut **tx)
        .await
        .map_err(io)?;
    Ok(())
}
