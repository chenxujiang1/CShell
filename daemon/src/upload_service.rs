use crate::{ProfileIpcService, upload_executor::SshUploadExecutor};
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
struct Repository(Arc<ProfileIpcService>);
#[async_trait::async_trait]
impl UploadRepository for Repository {
    async fn list_uploads(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<UploadRunSummary>, UploadRepositoryError> {
        self.0.service.repository().list_uploads(after, limit).await
    }
    async fn create_upload(
        &self,
        snapshot: &BatchUploadSnapshot,
    ) -> Result<(), UploadRepositoryError> {
        self.0.service.repository().create_upload(snapshot).await
    }
    async fn load_upload(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchUploadSnapshot, Vec<UploadTarget>)>, UploadRepositoryError> {
        self.0.service.repository().load_upload(id).await
    }
    async fn commit_upload_target(
        &self,
        expected: &UploadTarget,
        next: &UploadTarget,
    ) -> Result<(), UploadRepositoryError> {
        self.0
            .service
            .repository()
            .commit_upload_target(expected, next)
            .await
    }
}
#[derive(Clone, Debug)]
pub struct BatchUploadService(Arc<Inner>);
#[derive(Debug)]
struct Inner {
    repository: Arc<dyn UploadRepository>,
    runner: BatchUploadRunner,
    active: Mutex<BTreeMap<TaskRunId, TaskCancellation>>,
    recovery: Mutex<BTreeMap<TaskRunId, UploadRecoveryJob>>,
}
impl BatchUploadService {
    pub fn new(
        profiles: Arc<ProfileIpcService>,
        budget: TaskResourceBudget,
    ) -> Result<Self, TaskError> {
        let repository: Arc<dyn UploadRepository> = Arc::new(Repository(profiles.clone()));
        let runner = BatchUploadRunner::new(
            repository.clone(),
            Arc::new(SshUploadExecutor(profiles)),
            budget,
            Arc::new(SystemTaskClock::new()?),
        );
        Ok(Self(Arc::new(Inner {
            repository,
            runner,
            active: Mutex::new(BTreeMap::new()),
            recovery: Mutex::new(BTreeMap::new()),
        })))
    }
    pub async fn create(
        &self,
        snapshot: BatchUploadSnapshot,
    ) -> Result<TaskRunId, UploadRepositoryError> {
        snapshot
            .validate()
            .map_err(|_| UploadRepositoryError::Corrupt)?;
        self.0.repository.create_upload(&snapshot).await?;
        Ok(snapshot.id)
    }
    pub async fn list(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<UploadRunSummary>, UploadRepositoryError> {
        self.0.repository.list_uploads(after, limit).await
    }
    pub fn recovery_job(
        &self,
        id: TaskRunId,
    ) -> Result<Option<UploadRecoveryJob>, UploadRepositoryError> {
        Ok(self
            .0
            .recovery
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?
            .get(&id)
            .cloned())
    }
    pub fn activity(
        &self,
        id: TaskRunId,
    ) -> Result<(bool, Option<UploadRecoveryJob>), UploadRepositoryError> {
        let active = self
            .0
            .active
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?;
        let recovery = self
            .0
            .recovery
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?;
        Ok((active.contains_key(&id), recovery.get(&id).cloned()))
    }
    pub async fn start_recovery(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        confirmed: bool,
        operation: UploadRecoveryOperation,
    ) -> Result<(), UploadRepositoryError> {
        if operation != UploadRecoveryOperation::Probe && !confirmed {
            return Err(UploadRepositoryError::Conflict);
        }
        let (_, rows) = self.get(id).await?;
        let row = rows
            .iter()
            .find(|row| row.host_id == host)
            .ok_or(UploadRepositoryError::Missing)?;
        if revision != 0 && revision != row.revision
            || operation != UploadRecoveryOperation::Probe && revision == 0
        {
            return Err(UploadRepositoryError::Conflict);
        }
        if operation != UploadRecoveryOperation::Probe
            && (!row.temporary_owned || row.temporary_cleaned)
        {
            return Err(UploadRepositoryError::Conflict);
        }
        if operation == UploadRecoveryOperation::Adopt
            && (row.phase != UploadPhase::Unknown
                || row.intent != UploadIntent::Publish
                || !row.verified)
        {
            return Err(UploadRepositoryError::Conflict);
        }
        if operation == UploadRecoveryOperation::Cleanup
            && (row.phase.active()
                || row.phase == UploadPhase::Succeeded
                || row.phase == UploadPhase::Pending
                || row.phase == UploadPhase::Unknown && row.intent == UploadIntent::Publish)
        {
            return Err(UploadRepositoryError::Conflict);
        }
        let mut active = self
            .0
            .active
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?;
        if active.contains_key(&id) || active.len() >= 64 {
            return Err(UploadRepositoryError::Conflict);
        }
        let mut recovery = self
            .0
            .recovery
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?;
        if recovery.len() >= 64 && !recovery.contains_key(&id) {
            let old = recovery
                .iter()
                .find(|(run, job)| {
                    !active.contains_key(*run) && job.status != UploadRecoveryStatus::Running
                })
                .map(|(id, _)| *id);
            if let Some(old) = old {
                recovery.remove(&old);
            } else {
                return Err(UploadRepositoryError::Conflict);
            }
        }
        let revision = row.revision;
        let cancel = TaskCancellation::detached();
        active.insert(id, cancel.clone());
        recovery.insert(
            id,
            UploadRecoveryJob {
                host_id: host,
                row_revision: revision,
                operation,
                status: UploadRecoveryStatus::Running,
                probe: None,
                error: None,
            },
        );
        let inner = self.0.clone();
        tokio::spawn(async move {
            let result = if operation == UploadRecoveryOperation::Probe {
                inner
                    .runner
                    .probe_controlled(id, host, revision, cancel.clone())
                    .await
                    .map(Some)
            } else {
                inner
                    .runner
                    .resolve_controlled(
                        id,
                        host,
                        revision,
                        true,
                        operation == UploadRecoveryOperation::Adopt,
                        cancel.clone(),
                    )
                    .await
                    .map(|()| None)
            }
            .map_err(|error| {
                if cancel.reason().is_some() {
                    UploadReason::Cancelled
                } else {
                    match error {
                        UploadRepositoryError::Conflict | UploadRepositoryError::Missing => {
                            UploadReason::Invalid
                        }
                        UploadRepositoryError::Unavailable | UploadRepositoryError::Corrupt => {
                            UploadReason::Storage
                        }
                    }
                }
            });
            if let Ok(mut active) = inner.active.lock() {
                if let Ok(mut recovery) = inner.recovery.lock()
                    && let Some(job) = recovery.get_mut(&id)
                {
                    match result {
                        Ok(probe) => {
                            job.status = UploadRecoveryStatus::Completed;
                            job.probe = probe;
                        }
                        Err(error) => {
                            job.status = UploadRecoveryStatus::Failed;
                            job.error = Some(error);
                        }
                    }
                }
                active.remove(&id);
            }
        });
        Ok(())
    }
    pub async fn get(
        &self,
        id: TaskRunId,
    ) -> Result<(BatchUploadSnapshot, Vec<UploadTarget>), UploadRepositoryError> {
        let loaded = self
            .0
            .repository
            .load_upload(id)
            .await?
            .ok_or(UploadRepositoryError::Missing)?;
        let active = self
            .0
            .active
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?
            .contains_key(&id);
        if !active && loaded.1.iter().any(|row| row.phase.active()) {
            self.0.runner.recover(id).await?;
            self.0
                .repository
                .load_upload(id)
                .await?
                .ok_or(UploadRepositoryError::Missing)
        } else {
            Ok(loaded)
        }
    }
    pub fn is_active(&self, id: TaskRunId) -> Result<bool, UploadRepositoryError> {
        Ok(self
            .0
            .active
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?
            .contains_key(&id))
    }
    pub fn start(&self, id: TaskRunId, confirmed: bool) -> Result<(), UploadRepositoryError> {
        if !confirmed {
            return Err(UploadRepositoryError::Conflict);
        }
        let mut active = self
            .0
            .active
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?;
        if active.contains_key(&id) || active.len() >= 64 {
            return Err(UploadRepositoryError::Conflict);
        }
        let cancel = TaskCancellation::detached();
        active.insert(id, cancel.clone());
        let inner = self.0.clone();
        tokio::spawn(async move {
            if inner.runner.run(id, true, cancel).await.is_err() {
                tracing::warn!(upload_run_id = %id, "upload runner stopped; inspect persisted state before another action");
            }
            if let Ok(mut active) = inner.active.lock() {
                active.remove(&id);
            }
        });
        Ok(())
    }
    pub fn cancel(&self, id: TaskRunId) -> Result<(), UploadRepositoryError> {
        let active = self
            .0
            .active
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?;
        let cancel = active.get(&id).ok_or(UploadRepositoryError::Conflict)?;
        cancel.cancel(TaskStopReason::UserCancelled);
        Ok(())
    }
    pub async fn cancel_pending(&self, id: TaskRunId) -> Result<(), UploadRepositoryError> {
        self.0.runner.cancel_pending(id).await
    }
    pub async fn resume(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        confirmed: bool,
    ) -> Result<(), UploadRepositoryError> {
        if !confirmed {
            return Err(UploadRepositoryError::Conflict);
        }
        let (_, rows) = self.get(id).await?;
        let row = rows
            .iter()
            .find(|row| row.host_id == host)
            .ok_or(UploadRepositoryError::Missing)?;
        if row.revision != revision
            || row.phase != UploadPhase::Unknown
            || !row.temporary_owned
            || row.verified
            || row.committed
            || row.intent == UploadIntent::Publish
        {
            return Err(UploadRepositoryError::Conflict);
        }
        let mut active = self
            .0
            .active
            .lock()
            .map_err(|_| UploadRepositoryError::Unavailable)?;
        if active.contains_key(&id) || active.len() >= 64 {
            return Err(UploadRepositoryError::Conflict);
        }
        let cancel = TaskCancellation::detached();
        active.insert(id, cancel.clone());
        let inner = self.0.clone();
        tokio::spawn(async move {
            if inner
                .runner
                .resume(id, host, revision, true, cancel)
                .await
                .is_err()
            {
                tracing::warn!(upload_run_id = %id, "upload resume stopped; inspect saved state");
            }
            if let Ok(mut active) = inner.active.lock() {
                active.remove(&id);
            }
        });
        Ok(())
    }
    pub async fn probe(
        &self,
        id: TaskRunId,
        host: HostId,
    ) -> Result<UploadProbe, UploadRepositoryError> {
        self.0.runner.probe(id, host).await
    }
    pub async fn resolve(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        confirmed: bool,
        adopt: bool,
    ) -> Result<(), UploadRepositoryError> {
        self.0
            .runner
            .resolve(id, host, revision, confirmed, adopt)
            .await
    }
}
