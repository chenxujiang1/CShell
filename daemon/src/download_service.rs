use crate::{ProfileIpcService, download_executor::SshDownloadExecutor};
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
#[derive(Debug)]
struct Repository(Arc<ProfileIpcService>);
#[async_trait::async_trait]
impl DownloadRepository for Repository {
    async fn create_download(
        &self,
        snapshot: &BatchDownloadSnapshot,
    ) -> Result<(), TaskRepositoryError> {
        self.0.service.repository().create_download(snapshot).await
    }
    async fn load_download(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchDownloadSnapshot, Vec<DownloadTarget>)>, TaskRepositoryError> {
        self.0.service.repository().load_download(id).await
    }
    async fn commit_download_target(
        &self,
        expected: &DownloadTarget,
        next: &DownloadTarget,
    ) -> Result<(), TaskRepositoryError> {
        self.0
            .service
            .repository()
            .commit_download_target(expected, next)
            .await
    }
    async fn list_downloads(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<DownloadRunSummary>, TaskRepositoryError> {
        self.0
            .service
            .repository()
            .list_downloads(after, limit)
            .await
    }
}
#[derive(Clone, Debug)]
pub struct BatchDownloadService(Arc<Inner>);
#[derive(Debug)]
struct Inner {
    repository: Arc<dyn DownloadRepository>,
    runner: BatchDownloadRunner,
    active: Mutex<BTreeMap<TaskRunId, TaskCancellation>>,
    recovery: Mutex<BTreeMap<TaskRunId, DownloadRecoveryJob>>,
}
impl BatchDownloadService {
    pub fn new(
        profiles: Arc<ProfileIpcService>,
        budget: TaskResourceBudget,
    ) -> Result<Self, TaskError> {
        let repository: Arc<dyn DownloadRepository> = Arc::new(Repository(profiles.clone()));
        let runner = BatchDownloadRunner::new(
            repository.clone(),
            Arc::new(SshDownloadExecutor(profiles)),
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
        snapshot: BatchDownloadSnapshot,
    ) -> Result<TaskRunId, TaskRepositoryError> {
        snapshot
            .validate()
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        self.0.repository.create_download(&snapshot).await?;
        Ok(snapshot.id)
    }
    pub async fn list(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<DownloadRunSummary>, TaskRepositoryError> {
        self.0.repository.list_downloads(after, limit).await
    }
    pub async fn get(
        &self,
        id: TaskRunId,
    ) -> Result<(BatchDownloadSnapshot, Vec<DownloadTarget>), TaskRepositoryError> {
        let loaded = self
            .0
            .repository
            .load_download(id)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        let active = self
            .0
            .active
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?
            .contains_key(&id);
        if !active && loaded.1.iter().any(|row| row.phase.active()) {
            self.0.runner.recover(id).await?;
            self.0
                .repository
                .load_download(id)
                .await?
                .ok_or(TaskRepositoryError::Missing)
        } else {
            Ok(loaded)
        }
    }
    pub fn activity(
        &self,
        id: TaskRunId,
    ) -> Result<(bool, Option<DownloadRecoveryJob>), TaskRepositoryError> {
        let active = self
            .0
            .active
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        let recovery = self
            .0
            .recovery
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        Ok((active.contains_key(&id), recovery.get(&id).cloned()))
    }
    pub async fn launch(
        &self,
        id: TaskRunId,
        confirmed: bool,
        target: Option<(HostId, u64)>,
    ) -> Result<(), TaskRepositoryError> {
        if !confirmed {
            return Err(TaskRepositoryError::Conflict);
        }
        let (_, rows) = self.get(id).await?;
        if let Some((host, revision)) = target {
            let row = rows
                .iter()
                .find(|row| row.host_id == host)
                .ok_or(TaskRepositoryError::Missing)?;
            if row.revision != revision || !row.can_resume() {
                return Err(TaskRepositoryError::Conflict);
            }
        } else if !rows.iter().any(|row| row.phase == DownloadPhase::Pending) {
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
            let result = if let Some((host, revision)) = target {
                inner.runner.resume(id, host, revision, true, cancel).await
            } else {
                inner.runner.run(id, true, cancel).await
            };
            if result.is_err() {
                tracing::warn!(download_run_id = %id, "download runner stopped; inspect saved state before another action");
            }
            if let Ok(mut active) = inner.active.lock() {
                active.remove(&id);
            }
        });
        Ok(())
    }
    pub fn cancel(&self, id: TaskRunId) -> Result<(), TaskRepositoryError> {
        let active = self
            .0
            .active
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        active
            .get(&id)
            .ok_or(TaskRepositoryError::Conflict)?
            .cancel(TaskStopReason::UserCancelled);
        Ok(())
    }
    pub async fn cancel_pending(&self, id: TaskRunId) -> Result<(), TaskRepositoryError> {
        self.0.runner.cancel_pending(id).await
    }
    pub async fn recover_target(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        confirmed: bool,
        operation: DownloadRecoveryOperation,
    ) -> Result<(), TaskRepositoryError> {
        if operation != DownloadRecoveryOperation::Probe && (!confirmed || revision == 0) {
            return Err(TaskRepositoryError::Conflict);
        }
        let (_, rows) = self.get(id).await?;
        let row = rows
            .iter()
            .find(|row| row.host_id == host)
            .ok_or(TaskRepositoryError::Missing)?;
        if revision != 0 && revision != row.revision {
            return Err(TaskRepositoryError::Conflict);
        }
        if operation == DownloadRecoveryOperation::Adopt && !row.can_adopt()
            || operation == DownloadRecoveryOperation::Cleanup && !row.can_cleanup()
        {
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
        let mut recovery = self
            .0
            .recovery
            .lock()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        if recovery.len() >= 64 && !recovery.contains_key(&id) {
            let old = recovery
                .iter()
                .find(|(run, job)| {
                    !active.contains_key(*run) && job.status != FileRecoveryStatus::Running
                })
                .map(|(id, _)| *id);
            if let Some(old) = old {
                recovery.remove(&old);
            } else {
                return Err(TaskRepositoryError::Conflict);
            }
        }
        let revision = row.revision;
        let cancel = TaskCancellation::detached();
        active.insert(id, cancel.clone());
        recovery.insert(
            id,
            DownloadRecoveryJob {
                host_id: host,
                row_revision: revision,
                operation,
                status: FileRecoveryStatus::Running,
                probe: None,
                error: None,
            },
        );
        let inner = self.0.clone();
        tokio::spawn(async move {
            let result = if operation == DownloadRecoveryOperation::Probe {
                inner
                    .runner
                    .probe(id, host, revision, cancel.clone())
                    .await
                    .map(Some)
            } else {
                inner
                    .runner
                    .resolve(
                        id,
                        host,
                        revision,
                        true,
                        operation == DownloadRecoveryOperation::Adopt,
                        cancel.clone(),
                    )
                    .await
                    .map(|()| None)
            };
            if let Ok(mut active) = inner.active.lock() {
                if let Ok(mut recovery) = inner.recovery.lock()
                    && let Some(job) = recovery.get_mut(&id)
                {
                    match result {
                        Ok(probe) => {
                            job.status = FileRecoveryStatus::Completed;
                            job.probe = probe;
                        }
                        Err(_) => {
                            job.status = FileRecoveryStatus::Failed;
                            job.error = Some(if cancel.reason().is_some() {
                                DownloadReason::Cancelled
                            } else {
                                DownloadReason::Storage
                            });
                        }
                    }
                }
                active.remove(&id);
            }
        });
        Ok(())
    }
}
