use crate::SqliteProfileRepository;
use cshell_application::*;
use cshell_domain::TaskRunId;
use sqlx::Row;

fn unavailable(_: sqlx::Error) -> TaskRepositoryError {
    TaskRepositoryError::Unavailable
}
fn corrupt(_: TaskError) -> TaskRepositoryError {
    TaskRepositoryError::Corrupt
}

#[async_trait::async_trait]
impl TaskRepository for SqliteProfileRepository {
    async fn create_task_run(&self, record: &TaskRunRecord) -> Result<(), TaskRepositoryError> {
        if TaskRunRecord::new(record.snapshot().clone()).map_err(corrupt)? != *record {
            return Err(TaskRepositoryError::Corrupt);
        }
        let snapshot = encode_task_snapshot(record.snapshot()).map_err(corrupt)?;
        let meta = encode_task_meta(record.meta()).map_err(corrupt)?;
        if let Some(parent) = record.snapshot().retry_of() {
            let parent = self
                .load_task_run(parent)
                .await?
                .ok_or(TaskRepositoryError::Missing)?;
            record
                .snapshot()
                .validate_retry_parent(&parent)
                .map_err(corrupt)?;
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM task_runs WHERE id=?")
            .bind(record.id().to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?
            != 0
        {
            return Err(TaskRepositoryError::Conflict);
        }
        if let Some(parent) = record.snapshot().retry_of() {
            let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM task_runs WHERE id=?")
                .bind(parent.to_string())
                .fetch_one(&mut *tx)
                .await
                .map_err(unavailable)?;
            if exists != 1 {
                return Err(TaskRepositoryError::Missing);
            }
        }
        sqlx::query("INSERT INTO task_runs(id,revision,state,snapshot_json,snapshot_sha256,meta_json,name,operator,target_count) VALUES(?,1,0,?,?,?,?,?,?)")
            .bind(record.id().to_string()).bind(snapshot).bind(record.snapshot().fingerprint().as_slice()).bind(meta)
            .bind(&record.snapshot().definition().name).bind(record.snapshot().operator()).bind(record.targets().len() as i64)
            .execute(&mut *tx).await.map_err(unavailable)?;
        for target in record.targets() {
            sqlx::query("INSERT INTO task_targets(run_id,host_id,state,row_json) VALUES(?,?,0,?)")
                .bind(record.id().to_string())
                .bind(target.host_id.to_string())
                .bind(encode_task_target(target).map_err(corrupt)?)
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
        }
        sqlx::query("INSERT INTO task_run_events(run_id,revision,event_json) VALUES(?,1,?)")
            .bind(record.id().to_string())
            .bind(encode_task_event(&record.created_event()).map_err(corrupt)?)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)
    }
    async fn load_task_run(
        &self,
        id: TaskRunId,
    ) -> Result<Option<TaskRunRecord>, TaskRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let row=sqlx::query("SELECT revision,state,name,operator,target_count,snapshot_sha256, CASE WHEN length(snapshot_json)<=? THEN snapshot_json ELSE NULL END AS snapshot_json, CASE WHEN length(meta_json)<=? THEN meta_json ELSE NULL END AS meta_json FROM task_runs WHERE id=?")
            .bind(MAX_TASK_SNAPSHOT_BYTES as i64).bind(MAX_TASK_META_BYTES as i64).bind(id.to_string())
            .fetch_optional(&mut *tx).await.map_err(unavailable)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let snapshot: Option<Vec<u8>> = row.try_get("snapshot_json").map_err(unavailable)?;
        let meta: Option<Vec<u8>> = row.try_get("meta_json").map_err(unavailable)?;
        let snapshot = decode_task_snapshot(&snapshot.ok_or(TaskRepositoryError::Corrupt)?)
            .map_err(corrupt)?;
        let meta = decode_task_meta(&meta.ok_or(TaskRepositoryError::Corrupt)?).map_err(corrupt)?;
        let fingerprint: Vec<u8> = row.try_get("snapshot_sha256").map_err(unavailable)?;
        if snapshot.id() != id
            || meta.revision as i64 != row.try_get::<i64, _>("revision").map_err(unavailable)?
            || task_state_code(meta.state) != row.try_get::<i64, _>("state").map_err(unavailable)?
            || fingerprint.as_slice() != snapshot.fingerprint()
            || snapshot.definition().name
                != row.try_get::<String, _>("name").map_err(unavailable)?
            || snapshot.operator() != row.try_get::<String, _>("operator").map_err(unavailable)?
            || snapshot.targets().targets().len() as i64
                != row.try_get::<i64, _>("target_count").map_err(unavailable)?
        {
            return Err(TaskRepositoryError::Corrupt);
        }
        let rows=sqlx::query("SELECT host_id,state,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM task_targets WHERE run_id=? ORDER BY host_id LIMIT 1001")
            .bind(MAX_TASK_TARGET_BYTES as i64).bind(id.to_string()).fetch_all(&mut *tx).await.map_err(unavailable)?;
        let mut targets = Vec::new();
        for row in rows {
            let bytes: Option<Vec<u8>> = row.try_get("row_json").map_err(unavailable)?;
            let target =
                decode_task_target(&bytes.ok_or(TaskRepositoryError::Corrupt)?).map_err(corrupt)?;
            if target.host_id.to_string()
                != row.try_get::<String, _>("host_id").map_err(unavailable)?
                || task_target_code(target.state)
                    != row.try_get::<i64, _>("state").map_err(unavailable)?
            {
                return Err(TaskRepositoryError::Corrupt);
            }
            targets.push(target);
        }
        let (count, last): (i64, i64) = sqlx::query_as(
            "SELECT count(*),coalesce(max(revision),0) FROM task_run_events WHERE run_id=?",
        )
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;
        if count != meta.revision as i64 || last != count {
            return Err(TaskRepositoryError::Corrupt);
        }
        let record = TaskRunRecord::restore(snapshot, meta, targets).map_err(corrupt)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Some(record))
    }
    async fn commit_task_patch(&self, patch: &TaskRunPatch) -> Result<u64, TaskRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let row=sqlx::query("SELECT revision,state,snapshot_sha256,CASE WHEN length(meta_json)<=? THEN meta_json ELSE NULL END AS meta_json FROM task_runs WHERE id=?")
            .bind(MAX_TASK_META_BYTES as i64).bind(patch.id().to_string()).fetch_optional(&mut *tx).await.map_err(unavailable)?;
        let row = row.ok_or(TaskRepositoryError::Missing)?;
        if row.try_get::<i64, _>("revision").map_err(unavailable)? != patch.before().revision as i64
        {
            return Err(TaskRepositoryError::Conflict);
        }
        let bytes: Option<Vec<u8>> = row.try_get("meta_json").map_err(unavailable)?;
        let before =
            decode_task_meta(&bytes.ok_or(TaskRepositoryError::Corrupt)?).map_err(corrupt)?;
        let fingerprint: Vec<u8> = row.try_get("snapshot_sha256").map_err(unavailable)?;
        if &before != patch.before()
            || fingerprint.as_slice() != patch.snapshot_fingerprint()
            || task_state_code(before.state)
                != row.try_get::<i64, _>("state").map_err(unavailable)?
        {
            return Err(TaskRepositoryError::Corrupt);
        }
        for change in patch.changes() {
            let row=sqlx::query("SELECT state,CASE WHEN length(row_json)<=? THEN row_json ELSE NULL END AS row_json FROM task_targets WHERE run_id=? AND host_id=?")
                .bind(MAX_TASK_TARGET_BYTES as i64).bind(patch.id().to_string()).bind(change.before().host_id.to_string())
                .fetch_optional(&mut *tx).await.map_err(unavailable)?.ok_or(TaskRepositoryError::Corrupt)?;
            let bytes: Option<Vec<u8>> = row.try_get("row_json").map_err(unavailable)?;
            if decode_task_target(&bytes.ok_or(TaskRepositoryError::Corrupt)?).map_err(corrupt)?
                != *change.before()
                || row.try_get::<i64, _>("state").map_err(unavailable)?
                    != task_target_code(change.before().state)
            {
                return Err(TaskRepositoryError::Corrupt);
            }
            sqlx::query("UPDATE task_targets SET state=?,row_json=? WHERE run_id=? AND host_id=?")
                .bind(task_target_code(change.after().state))
                .bind(encode_task_target(change.after()).map_err(corrupt)?)
                .bind(patch.id().to_string())
                .bind(change.after().host_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
        }
        sqlx::query(
            "UPDATE task_runs SET revision=?,state=?,meta_json=? WHERE id=? AND revision=?",
        )
        .bind(patch.after().revision as i64)
        .bind(task_state_code(patch.after().state))
        .bind(encode_task_meta(patch.after()).map_err(corrupt)?)
        .bind(patch.id().to_string())
        .bind(patch.before().revision as i64)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        sqlx::query("INSERT INTO task_run_events(run_id,revision,event_json) VALUES(?,?,?)")
            .bind(patch.id().to_string())
            .bind(patch.after().revision as i64)
            .bind(encode_task_event(patch.event()).map_err(corrupt)?)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(patch.after().revision)
    }
    async fn list_task_runs(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
        unfinished_only: bool,
    ) -> Result<Vec<TaskRunSummary>, TaskRepositoryError> {
        if limit == 0 || limit > MAX_TASK_LIST_RESULTS {
            return Err(TaskRepositoryError::Corrupt);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let rows=sqlx::query("SELECT id,revision,state,name,operator,target_count,CASE WHEN length(meta_json)<=? THEN meta_json ELSE NULL END AS meta_json FROM task_runs WHERE id>? AND (?=0 OR state<4) ORDER BY id LIMIT ?")
            .bind(MAX_TASK_META_BYTES as i64).bind(after.map_or_else(String::new,|id|id.to_string()))
            .bind(unfinished_only).bind(limit as i64).fetch_all(&mut *tx).await.map_err(unavailable)?;
        let mut result = Vec::new();
        for row in rows {
            let id: String = row.try_get("id").map_err(unavailable)?;
            let bytes: Option<Vec<u8>> = row.try_get("meta_json").map_err(unavailable)?;
            let meta =
                decode_task_meta(&bytes.ok_or(TaskRepositoryError::Corrupt)?).map_err(corrupt)?;
            if meta.revision == 0
                || meta.revision as i64 != row.try_get::<i64, _>("revision").map_err(unavailable)?
                || task_state_code(meta.state)
                    != row.try_get::<i64, _>("state").map_err(unavailable)?
            {
                return Err(TaskRepositoryError::Corrupt);
            }
            let mut counts = [0; 8];
            let groups = sqlx::query(
                "SELECT state,count(*) AS count FROM task_targets WHERE run_id=? GROUP BY state",
            )
            .bind(&id)
            .fetch_all(&mut *tx)
            .await
            .map_err(unavailable)?;
            for group in groups {
                let state: i64 = group.try_get("state").map_err(unavailable)?;
                let count: i64 = group.try_get("count").map_err(unavailable)?;
                let index = usize::try_from(state)
                    .ok()
                    .filter(|index| *index < 8)
                    .ok_or(TaskRepositoryError::Corrupt)?;
                counts[index] = u64::try_from(count).map_err(|_| TaskRepositoryError::Corrupt)?;
            }
            let total: i64 = row.try_get("target_count").map_err(unavailable)?;
            if !(1..=1000).contains(&total) || counts.iter().sum::<u64>() != total as u64 {
                return Err(TaskRepositoryError::Corrupt);
            }
            result.push(TaskRunSummary {
                id: TaskRunId::from_uuid(id.parse().map_err(|_| TaskRepositoryError::Corrupt)?),
                name: row.try_get("name").map_err(unavailable)?,
                operator: row.try_get("operator").map_err(unavailable)?,
                meta,
                targets: counts,
            });
        }
        tx.commit().await.map_err(unavailable)?;
        Ok(result)
    }
}
