//! Internal mixed-task service; client IPC/UI exposure is a separate integration.
use crate::{
    ProfileIpcService, download_executor::SshDownloadExecutor, upload_executor::SshUploadExecutor,
};
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
#[derive(Debug)]
struct Repository(Arc<ProfileIpcService>);
#[async_trait::async_trait]
impl MixedTaskRepository for Repository {
    async fn list_mixed_tasks(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<MixedRunSummary>, TaskRepositoryError> {
        self.0
            .service
            .repository()
            .list_mixed_tasks(after, limit)
            .await
    }
    async fn load_mixed_page(
        &self,
        id: TaskRunId,
        after: Option<HostId>,
        limit: usize,
    ) -> Result<Option<MixedPageData>, TaskRepositoryError> {
        self.0
            .service
            .repository()
            .load_mixed_page(id, after, limit)
            .await
    }
    async fn create_mixed_task(
        &self,
        snapshot: &MixedTaskSnapshot,
    ) -> Result<(), TaskRepositoryError> {
        self.0
            .service
            .repository()
            .create_mixed_task(snapshot)
            .await
    }
    async fn load_mixed_task(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(MixedTaskSnapshot, Vec<MixedTargetRun>)>, TaskRepositoryError> {
        self.0.service.repository().load_mixed_task(id).await
    }
    async fn load_mixed_target(
        &self,
        id: TaskRunId,
        host: HostId,
    ) -> Result<Option<(MixedTaskSnapshot, MixedTargetRun)>, TaskRepositoryError> {
        self.0
            .service
            .repository()
            .load_mixed_target(id, host)
            .await
    }
    async fn commit_mixed_target(
        &self,
        patch: &MixedTargetPatch,
    ) -> Result<u64, TaskRepositoryError> {
        self.0.service.repository().commit_mixed_target(patch).await
    }
}
#[derive(Clone, Debug)]
pub struct MixedTaskService(Arc<Inner>);
#[derive(Debug, thiserror::Error)]
pub enum MixedRetryError {
    #[error(transparent)]
    Storage(#[from] TaskRepositoryError),
    #[error(transparent)]
    Plan(#[from] TaskError),
}
#[derive(Debug)]
struct Inner {
    repository: Arc<dyn MixedTaskRepository>,
    runner: MixedTaskRunner,
    active: Mutex<BTreeMap<TaskRunId, TaskCancellation>>,
    operations: tokio::sync::Mutex<()>,
    repairs: Mutex<BTreeMap<TaskRunId, MixedRepairJob>>,
}
impl MixedTaskService {
    pub async fn repair_document(
        &self,
        id: TaskRunId,
    ) -> Result<MixedRepairDocument, TaskRepositoryError> {
        if self
            .0
            .repository
            .load_mixed_page(id, None, 1)
            .await?
            .is_none()
        {
            return Err(TaskRepositoryError::Missing);
        }
        let active = self
            .0
            .active
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        let repairs = self
            .0
            .repairs
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        let document = MixedRepairDocument {
            format: "cshell.mixed-file-repair".into(),
            version: 1,
            run_id: id,
            active: active.contains_key(&id),
            job: repairs.get(&id).cloned(),
        };
        document
            .validate()
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        Ok(document)
    }
    pub async fn repair(
        &self,
        id: TaskRunId,
        host: HostId,
        step: usize,
        revision: u64,
        operation: MixedFileOperation,
        confirmed: bool,
    ) -> Result<(), TaskRepositoryError> {
        if revision == 0 || operation != MixedFileOperation::Probe && !confirmed {
            return Err(TaskRepositoryError::Conflict);
        }
        let _operation = self.0.operations.lock().await;
        let (snapshot, rows) = self.inspect(id).await?;
        let row = rows
            .iter()
            .find(|row| row.host_id == host)
            .ok_or(TaskRepositoryError::Missing)?;
        if row.revision != revision || !row.file_repair_allowed(step, operation) {
            return Err(TaskRepositoryError::Conflict);
        }
        if operation == MixedFileOperation::Adopt {
            let job = self
                .0
                .repairs
                .lock()
                .map_err(|_| TaskRepositoryError::Unavailable)?
                .get(&id)
                .cloned();
            let document = MixedRepairDocument {
                format: "cshell.mixed-file-repair".into(),
                version: 1,
                run_id: id,
                active: false,
                job,
            };
            if !document.matching_destination(&snapshot, row, step) {
                return Err(TaskRepositoryError::Conflict);
            }
        }
        let mut active = self
            .0
            .active
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        if active.contains_key(&id) || active.len() >= 64 {
            return Err(TaskRepositoryError::Conflict);
        }
        let mut repairs = self
            .0
            .repairs
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        if repairs.len() >= 64 && !repairs.contains_key(&id) {
            let old = repairs
                .iter()
                .find(|(run, job)| {
                    !active.contains_key(*run) && job.status != FileRecoveryStatus::Running
                })
                .map(|(run, _)| *run);
            if let Some(old) = old {
                repairs.remove(&old);
            } else {
                return Err(TaskRepositoryError::Conflict);
            }
        }
        let child = snapshot
            .child(host, step)
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        let child_revision = row.steps[step]
            .receipt
            .file_revision()
            .ok_or(TaskRepositoryError::Conflict)?;
        let cancel = TaskCancellation::detached();
        active.insert(id, cancel.clone());
        repairs.insert(
            id,
            MixedRepairJob {
                host_id: host,
                step,
                parent_revision: revision,
                child_id: child.id(),
                child_revision,
                operation,
                status: FileRecoveryStatus::Running,
                probe: None,
                result_revision: None,
                error: None,
            },
        );
        let inner = self.0.clone();
        tokio::spawn(async move {
            let result = inner
                .runner
                .repair(id, host, step, revision, operation, true, cancel.clone())
                .await;
            let result_revision = inner
                .repository
                .load_mixed_target(id, host)
                .await
                .ok()
                .flatten()
                .map(|(_, row)| row.revision);
            if let Ok(mut active) = inner.active.lock() {
                if let Ok(mut repairs) = inner.repairs.lock()
                    && let Some(job) = repairs.get_mut(&id)
                {
                    job.result_revision = result_revision;
                    match result {
                        Ok(probe) if result_revision.is_some() => {
                            job.status = FileRecoveryStatus::Completed;
                            job.probe = probe;
                        }
                        result => {
                            job.status = FileRecoveryStatus::Failed;
                            job.error = Some(if cancel.reason().is_some() {
                                MixedRepairError::Cancelled
                            } else {
                                match result {
                                    Err(
                                        TaskRepositoryError::Conflict
                                        | TaskRepositoryError::Missing,
                                    ) => MixedRepairError::Conflict,
                                    Err(TaskRepositoryError::Corrupt) => MixedRepairError::Corrupt,
                                    _ => MixedRepairError::Unavailable,
                                }
                            });
                        }
                    }
                }
                active.remove(&id);
            }
        });
        Ok(())
    }
    pub async fn list(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<MixedRunSummary>, TaskRepositoryError> {
        self.0.repository.list_mixed_tasks(after, limit).await
    }
    pub async fn page(
        &self,
        id: TaskRunId,
        after: Option<HostId>,
        limit: usize,
    ) -> Result<MixedStatusPage, TaskRepositoryError> {
        let _operation = self.0.operations.lock().await;
        let mut data = self
            .0
            .repository
            .load_mixed_page(id, after, limit)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        let orphan_control =
            if !self.is_active(id)? && data.snapshot.definition().policy.rollout.is_some() {
                self.0
                    .repository
                    .load_mixed_target(id, data.snapshot.targets().targets()[0].id)
                    .await?
                    .is_some_and(|(_, row)| {
                        row.rollout
                            .as_ref()
                            .is_some_and(|progress| progress.stage == MixedRolloutStage::Running)
                    })
            } else {
                false
            };
        if !self.is_active(id)?
            && (data.counts[MixedTaskState::Running.code() as usize] > 0 || orphan_control)
        {
            self.0.runner.recover(id).await?;
            data = self
                .0
                .repository
                .load_mixed_page(id, after, limit)
                .await?
                .ok_or(TaskRepositoryError::Missing)?;
        }
        MixedStatusPage::from_data(data, self.is_active(id)?)
            .map_err(|_| TaskRepositoryError::Corrupt)
    }
    pub async fn target(
        &self,
        id: TaskRunId,
        host: HostId,
    ) -> Result<(MixedTaskSnapshot, MixedTargetRun), TaskRepositoryError> {
        self.0
            .repository
            .load_mixed_target(id, host)
            .await?
            .ok_or(TaskRepositoryError::Missing)
    }
    pub fn new(
        profiles: Arc<ProfileIpcService>,
        command: Arc<dyn TaskExecutor>,
        budget: TaskResourceBudget,
    ) -> Result<Self, TaskError> {
        let repository: Arc<dyn MixedTaskRepository> = Arc::new(Repository(profiles.clone()));
        let executors = MixedTaskExecutors {
            command,
            upload: Arc::new(SshUploadExecutor(profiles.clone())),
            download: Arc::new(SshDownloadExecutor(profiles)),
        };
        let runner = MixedTaskRunner::new(
            repository.clone(),
            executors,
            budget,
            Arc::new(SystemTaskClock::new()?),
        );
        Ok(Self(Arc::new(Inner {
            repository,
            runner,
            active: Mutex::new(BTreeMap::new()),
            operations: tokio::sync::Mutex::new(()),
            repairs: Mutex::new(BTreeMap::new()),
        })))
    }
    pub async fn create(
        &self,
        snapshot: MixedTaskSnapshot,
    ) -> Result<TaskRunId, TaskRepositoryError> {
        self.0.repository.create_mixed_task(&snapshot).await?;
        Ok(snapshot.id())
    }
    pub fn is_active(&self, id: TaskRunId) -> Result<bool, TaskRepositoryError> {
        Ok(self
            .0
            .active
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?
            .contains_key(&id))
    }
    /// Saves a fresh Pending run; executing it requires a separate reviewed start.
    pub async fn retry(
        &self,
        confirmation: &MixedRetryConfirmation,
        clock: &dyn TaskClock,
    ) -> Result<MixedTaskSnapshot, MixedRetryError> {
        confirmation.validate()?;
        let _operation = self.0.operations.lock().await;
        if self.is_active(confirmation.run_id)? {
            return Err(TaskRepositoryError::Conflict.into());
        }
        let (parent, rows) = self.inspect(confirmation.run_id).await?;
        let now = clock.now_ms()?;
        let next =
            MixedTaskSnapshot::retry(&parent, &rows, confirmation, "local-user".into(), now)?;
        self.0.repository.create_mixed_task(&next).await?;
        Ok(next)
    }
    pub async fn get(
        &self,
        id: TaskRunId,
    ) -> Result<(MixedTaskSnapshot, Vec<MixedTargetRun>), TaskRepositoryError> {
        let _operation = self.0.operations.lock().await;
        self.inspect(id).await
    }
    /// Capture once while excluding new starts and repairs; later mutations cannot alter it.
    pub async fn capture_results(
        &self,
        id: TaskRunId,
        clock: &dyn TaskClock,
    ) -> Result<MixedResultDocument, TaskRepositoryError> {
        let _operation = self.0.operations.lock().await;
        if self.is_active(id)? {
            return Err(TaskRepositoryError::Conflict);
        }
        let (snapshot, rows) = self.inspect(id).await?;
        if rows
            .iter()
            .any(|row| matches!(row.state, MixedTaskState::Pending | MixedTaskState::Running))
        {
            return Err(TaskRepositoryError::Conflict);
        }
        let captured = clock
            .now_ms()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        if captured < snapshot.created_at_ms()
            || rows.iter().any(|row| row.updated_at_ms > captured)
        {
            return Err(TaskRepositoryError::Unavailable);
        }
        tokio::task::spawn_blocking(move || {
            MixedResultDocument::from_rows(snapshot, rows, captured)
        })
        .await
        .map_err(|_| TaskRepositoryError::Unavailable)?
        .map_err(|_| TaskRepositoryError::Corrupt)
    }
    async fn inspect(
        &self,
        id: TaskRunId,
    ) -> Result<(MixedTaskSnapshot, Vec<MixedTargetRun>), TaskRepositoryError> {
        let loaded = self
            .0
            .repository
            .load_mixed_task(id)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        if !self.is_active(id)?
            && loaded.1.iter().any(|row| {
                row.state == MixedTaskState::Running
                    || row
                        .rollout
                        .as_ref()
                        .is_some_and(|progress| progress.stage == MixedRolloutStage::Running)
            })
        {
            self.0.runner.recover(id).await?;
            self.0
                .repository
                .load_mixed_task(id)
                .await?
                .ok_or(TaskRepositoryError::Missing)
        } else {
            Ok(loaded)
        }
    }
    /// Starts exactly the displayed Pending set; terminal/unknown targets are untouched.
    pub async fn rollout_document(
        &self,
        id: TaskRunId,
    ) -> Result<MixedRolloutDocument, TaskRepositoryError> {
        let _operation = self.0.operations.lock().await;
        let (snapshot, rows) = self.inspect(id).await?;
        self.rollout_view(&snapshot, &rows[0])
    }
    fn rollout_view(
        &self,
        snapshot: &MixedTaskSnapshot,
        row: &MixedTargetRun,
    ) -> Result<MixedRolloutDocument, TaskRepositoryError> {
        let document = MixedRolloutDocument {
            format: "cshell.mixed-rollout".into(),
            version: 1,
            run_id: snapshot.id(),
            snapshot_sha256: *snapshot.fingerprint(),
            carrier_host: row.host_id,
            carrier_revision: row.revision,
            target_count: snapshot.targets().targets().len(),
            policy: snapshot
                .definition()
                .policy
                .rollout
                .clone()
                .ok_or(TaskRepositoryError::Conflict)?,
            progress: row.rollout.clone().ok_or(TaskRepositoryError::Corrupt)?,
            updated_at_ms: row.updated_at_ms,
            active: self.is_active(snapshot.id())?,
        };
        document
            .check_snapshot(snapshot)
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        Ok(document)
    }
    pub async fn approve_rollout(
        &self,
        id: TaskRunId,
        revision: u64,
        gate: cshell_domain::TaskGateId,
    ) -> Result<MixedRolloutDocument, TaskRepositoryError> {
        let _operation = self.0.operations.lock().await;
        if self.is_active(id)? {
            return Err(TaskRepositoryError::Conflict);
        }
        let (snapshot, _) = self.inspect(id).await?;
        self.0
            .runner
            .rollout_action(
                &snapshot,
                MixedRolloutAction::Approve { gate },
                Some(revision),
            )
            .await?;
        let (_, row) = self.target(id, snapshot.targets().targets()[0].id).await?;
        self.rollout_view(&snapshot, &row)
    }
    pub async fn start(
        &self,
        id: TaskRunId,
        expected: &BTreeMap<HostId, u64>,
        confirmed: bool,
    ) -> Result<(), TaskRepositoryError> {
        self.start_bound(id, expected, None, None, confirmed).await
    }
    pub async fn start_confirmed(
        &self,
        confirmation: &MixedStartConfirmation,
        confirmed: bool,
    ) -> Result<(), TaskRepositoryError> {
        confirmation
            .validate()
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        let expected = confirmation
            .pending
            .iter()
            .map(|row| (row.host_id, row.revision))
            .collect();
        self.start_bound(
            confirmation.run_id,
            &expected,
            Some(confirmation.snapshot_sha256),
            confirmation.control_revision,
            confirmed,
        )
        .await
    }
    async fn start_bound(
        &self,
        id: TaskRunId,
        expected: &BTreeMap<HostId, u64>,
        fingerprint: Option<[u8; 32]>,
        control_revision: Option<u64>,
        confirmed: bool,
    ) -> Result<(), TaskRepositoryError> {
        if !confirmed || expected.is_empty() {
            return Err(TaskRepositoryError::Conflict);
        }
        let _operation = self.0.operations.lock().await;
        let (snapshot, rows) = self.inspect(id).await?;
        if fingerprint.is_some_and(|fingerprint| snapshot.fingerprint() != &fingerprint) {
            return Err(TaskRepositoryError::Conflict);
        }
        let actual: BTreeMap<_, _> = rows
            .iter()
            .filter(|row| row.state == MixedTaskState::Pending)
            .map(|row| (row.host_id, row.revision))
            .collect();
        if actual != *expected {
            return Err(TaskRepositoryError::Conflict);
        }
        if let Some(policy) = &snapshot.definition().policy.rollout {
            let progress = rows[0]
                .rollout
                .as_ref()
                .ok_or(TaskRepositoryError::Corrupt)?;
            let now = SystemTaskClock::new()
                .and_then(|clock| clock.now_ms())
                .map_err(|_| TaskRepositoryError::Unavailable)?;
            let failures = rows
                .iter()
                .filter(|row| {
                    matches!(
                        row.state,
                        MixedTaskState::Failed | MixedTaskState::Unknown | MixedTaskState::TimedOut
                    )
                })
                .count();
            if control_revision != Some(progress.revision)
                || progress.gate.is_some()
                || progress.stage == MixedRolloutStage::Running
                || progress.deadline_ms.is_some_and(|deadline| now >= deadline)
                || policy
                    .failure_limit
                    .is_some_and(|limit| failures >= usize::from(limit))
            {
                return Err(TaskRepositoryError::Conflict);
            }
            if progress.reason == Some(MixedRolloutStop::CanaryFailed) {
                let batches = mixed_rollout_batches(&snapshot);
                if rows
                    .iter()
                    .filter(|row| batches[0].contains(&row.host_id))
                    .any(|row| {
                        row.state != MixedTaskState::Succeeded
                            && row.state != MixedTaskState::Pending
                    })
                {
                    return Err(TaskRepositoryError::Conflict);
                }
            }
        } else if control_revision.is_some() {
            return Err(TaskRepositoryError::Conflict);
        }
        let mut active = self
            .0
            .active
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        if active.contains_key(&id) || active.len() >= 64 {
            return Err(TaskRepositoryError::Conflict);
        }
        let cancel = TaskCancellation::detached();
        active.insert(id, cancel.clone());
        let inner = self.0.clone();
        tokio::spawn(async move {
            if inner.runner.run(id, true, cancel).await.is_err() {
                tracing::warn!(mixed_run_id = %id, "mixed runner stopped; inspect durable child receipts before another action");
            }
            if let Ok(mut active) = inner.active.lock() {
                active.remove(&id);
            }
        });
        Ok(())
    }
    pub async fn cancel(&self, id: TaskRunId) -> Result<(), TaskRepositoryError> {
        {
            let active = self
                .0
                .active
                .lock()
                .map_err(|_| TaskRepositoryError::Unavailable)?;
            if let Some(cancel) = active.get(&id) {
                cancel.cancel(TaskStopReason::UserCancelled);
                return Ok(());
            }
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.0.runner.cancel_pending(id),
        )
        .await
        .map_err(|_| TaskRepositoryError::Unavailable)?
    }
}
