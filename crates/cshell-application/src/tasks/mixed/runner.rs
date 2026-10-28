use super::*;
use std::time::Duration;
mod rollout;

#[derive(Clone, Debug)]
pub struct MixedTaskExecutors {
    pub command: Arc<dyn TaskExecutor>,
    pub upload: Arc<dyn UploadExecutor>,
    pub download: Arc<dyn DownloadExecutor>,
}
#[derive(Clone, Debug)]
pub struct MixedTaskRunner {
    repository: Arc<dyn MixedTaskRepository>,
    executors: MixedTaskExecutors,
    budget: TaskResourceBudget,
    clock: Arc<dyn TaskClock>,
}
impl MixedTaskRunner {
    pub async fn repair(
        &self,
        id: TaskRunId,
        host: HostId,
        step: usize,
        revision: u64,
        operation: MixedFileOperation,
        confirmed: bool,
        cancel: TaskCancellation,
    ) -> Result<Option<MixedFileProbe>, TaskRepositoryError> {
        if revision == 0 || operation != MixedFileOperation::Probe && !confirmed {
            return Err(TaskRepositoryError::Conflict);
        }
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, row) = self
            .repository
            .load_mixed_target(id, host)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        if row.revision != revision || !row.file_repair_allowed(step, operation) {
            return Err(TaskRepositoryError::Conflict);
        }
        let child = snapshot
            .child(host, step)
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        let child_revision = row.steps[step]
            .receipt
            .file_revision()
            .ok_or(TaskRepositoryError::Conflict)?;
        if operation != MixedFileOperation::Probe {
            self.commit(
                &snapshot,
                &row,
                MixedTargetAction::BeginFileRepair { step, operation },
            )
            .await?;
        }
        let repository = Arc::new(children::ChildRepository {
            repository: self.repository.clone(),
            snapshot: snapshot.clone(),
            host,
            step,
            clock: self.clock.clone(),
            readonly: operation == MixedFileOperation::Probe,
        });
        let probe = match child {
            MixedChildSnapshot::Upload(child) => {
                let runner = BatchUploadRunner::new(
                    repository,
                    self.executors.upload.clone(),
                    self.budget.clone(),
                    self.clock.clone(),
                );
                match operation {
                    MixedFileOperation::Probe => {
                        let probe = runner
                            .probe_controlled(child.id, host, child_revision, cancel)
                            .await
                            .map_err(upload_failure)?;
                        Some(MixedFileProbe {
                            temporary: probe.temporary,
                            destination: probe.destination,
                        })
                    }
                    MixedFileOperation::Resume => {
                        runner
                            .resume(child.id, host, child_revision, true, cancel)
                            .await
                            .map_err(upload_failure)?;
                        None
                    }
                    MixedFileOperation::Cleanup | MixedFileOperation::Adopt => {
                        runner
                            .resolve_controlled(
                                child.id,
                                host,
                                child_revision,
                                true,
                                operation == MixedFileOperation::Adopt,
                                cancel,
                            )
                            .await
                            .map_err(upload_failure)?;
                        None
                    }
                }
            }
            MixedChildSnapshot::Download(child) => {
                let runner = BatchDownloadRunner::new(
                    repository,
                    self.executors.download.clone(),
                    self.budget.clone(),
                    self.clock.clone(),
                );
                match operation {
                    MixedFileOperation::Probe => {
                        let probe = runner.probe(child.id, host, child_revision, cancel).await?;
                        Some(MixedFileProbe {
                            temporary: probe.temporary.map(upload_content),
                            destination: probe.destination.map(upload_content),
                        })
                    }
                    MixedFileOperation::Resume => {
                        runner
                            .resume(child.id, host, child_revision, true, cancel)
                            .await?;
                        None
                    }
                    MixedFileOperation::Cleanup | MixedFileOperation::Adopt => {
                        runner
                            .resolve(
                                child.id,
                                host,
                                child_revision,
                                true,
                                operation == MixedFileOperation::Adopt,
                                cancel,
                            )
                            .await?;
                        None
                    }
                }
            }
            _ => return Err(TaskRepositoryError::Conflict),
        };
        if operation != MixedFileOperation::Probe {
            let row = self.load(&snapshot, host).await?;
            self.commit(&snapshot, &row, MixedTargetAction::FinishFileRepair)
                .await?;
        }
        Ok(probe)
    }
    pub fn new(
        repository: Arc<dyn MixedTaskRepository>,
        executors: MixedTaskExecutors,
        budget: TaskResourceBudget,
        clock: Arc<dyn TaskClock>,
    ) -> Self {
        Self {
            repository,
            executors,
            budget,
            clock,
        }
    }
    async fn commit(
        &self,
        snapshot: &MixedTaskSnapshot,
        row: &MixedTargetRun,
        action: MixedTargetAction,
    ) -> Result<MixedTargetRun, TaskRepositoryError> {
        let patch = row
            .prepare(
                snapshot,
                action,
                self.clock
                    .now_ms()
                    .map_err(|_| TaskRepositoryError::Unavailable)?,
            )
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        let revision = tokio::time::timeout(
            Duration::from_secs(5),
            self.repository.commit_mixed_target(&patch),
        )
        .await
        .map_err(|_| TaskRepositoryError::Unavailable)??;
        if revision != patch.after().revision {
            return Err(TaskRepositoryError::Unavailable);
        }
        Ok(patch.after().clone())
    }
    async fn load(
        &self,
        snapshot: &MixedTaskSnapshot,
        host: HostId,
    ) -> Result<MixedTargetRun, TaskRepositoryError> {
        let (saved, row) = self
            .repository
            .load_mixed_target(snapshot.id(), host)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        if saved.fingerprint() != snapshot.fingerprint() {
            return Err(TaskRepositoryError::Corrupt);
        }
        Ok(row)
    }
    pub async fn recover(&self, id: TaskRunId) -> Result<(), TaskRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, rows) = self
            .repository
            .load_mixed_task(id)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        for row in rows {
            if row.state == MixedTaskState::Running {
                self.commit(&snapshot, &row, MixedTargetAction::Recover)
                    .await?;
            }
        }
        if snapshot.definition().policy.rollout.is_some() {
            self.rollout_action(
                &snapshot,
                MixedRolloutAction::Stop(MixedRolloutStop::Restarted),
                None,
            )
            .await?;
        }
        Ok(())
    }
    pub async fn cancel_pending(&self, id: TaskRunId) -> Result<(), TaskRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, rows) = self
            .repository
            .load_mixed_task(id)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        for row in rows {
            if row.state == MixedTaskState::Pending {
                self.commit(
                    &snapshot,
                    &row,
                    MixedTargetAction::Stop(MixedStopReason::UserCancelled),
                )
                .await?;
            }
        }
        if snapshot.definition().policy.rollout.is_some() {
            self.rollout_action(
                &snapshot,
                MixedRolloutAction::Stop(MixedRolloutStop::UserCancelled),
                None,
            )
            .await?;
        }
        Ok(())
    }
    pub async fn run(
        &self,
        id: TaskRunId,
        confirmed: bool,
        cancel: TaskCancellation,
    ) -> Result<(), TaskRepositoryError> {
        if !confirmed {
            return Err(TaskRepositoryError::Conflict);
        }
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, rows) = self
            .repository
            .load_mixed_task(id)
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        if rows.iter().any(|row| row.state == MixedTaskState::Running) {
            return Err(TaskRepositoryError::Conflict);
        }
        if snapshot.definition().policy.rollout.is_some() {
            return self.run_rollout(snapshot, cancel).await;
        }
        let queued_at = tokio::time::Instant::now();
        let mut pending = rows
            .into_iter()
            .filter(|row| row.state == MixedTaskState::Pending);
        let mut jobs = tokio::task::JoinSet::new();
        let deadline = tokio::time::sleep(Duration::from_millis(
            snapshot.definition().policy.total_timeout_ms,
        ));
        tokio::pin!(deadline);
        loop {
            while jobs.len() < usize::from(snapshot.definition().policy.concurrency) {
                let Some(row) = pending.next() else {
                    break;
                };
                let runner = self.clone();
                let snapshot = snapshot.clone();
                let cancel = cancel.clone();
                jobs.spawn(async move { runner.target(snapshot, row, cancel, queued_at).await });
            }
            if jobs.is_empty() {
                break;
            }
            let result = tokio::select! { biased;
                _ = &mut deadline, if cancel.reason().is_none() => { cancel.cancel(TaskStopReason::TotalTimeout); continue; }
                result = jobs.join_next() => result,
            };
            let error = match result {
                Some(Ok(Ok(()))) => None,
                Some(Ok(Err(error))) => Some(error),
                Some(Err(_)) => Some(TaskRepositoryError::Unavailable),
                None => break,
            };
            if let Some(error) = error {
                cancel.cancel(TaskStopReason::OutputFailure);
                while jobs.join_next().await.is_some() {}
                return Err(error);
            }
        }
        Ok(())
    }
    async fn target(
        &self,
        snapshot: MixedTaskSnapshot,
        mut row: MixedTargetRun,
        cancel: TaskCancellation,
        queued_at: tokio::time::Instant,
    ) -> Result<(), TaskRepositoryError> {
        if let Some(reason) = cancel.reason() {
            self.commit(
                &snapshot,
                &row,
                MixedTargetAction::Stop(stop_reason(reason)),
            )
            .await?;
            return Ok(());
        }
        if queued_at.elapsed()
            >= Duration::from_millis(snapshot.definition().policy.queue_timeout_ms)
        {
            self.commit(
                &snapshot,
                &row,
                MixedTargetAction::Stop(MixedStopReason::QueueTimeout),
            )
            .await?;
            return Ok(());
        }
        row = self
            .commit(&snapshot, &row, MixedTargetAction::Start)
            .await?;
        loop {
            if let Some(reason) = cancel.reason() {
                self.commit(
                    &snapshot,
                    &row,
                    MixedTargetAction::Stop(stop_reason(reason)),
                )
                .await?;
                return Ok(());
            }
            row = self
                .commit(&snapshot, &row, MixedTargetAction::EnterStep)
                .await?;
            let child = snapshot
                .child(row.host_id, row.next_step)
                .map_err(|_| TaskRepositoryError::Corrupt)?;
            let repository = Arc::new(children::ChildRepository {
                readonly: false,
                repository: self.repository.clone(),
                snapshot: snapshot.clone(),
                host: row.host_id,
                step: row.next_step,
                clock: self.clock.clone(),
            });
            // Leaf runners acquire their own shared budget. No outer connection lease
            // is held while a file child waits, avoiding recursive budget acquisition.
            match child {
                MixedChildSnapshot::Command(child) => {
                    let scheduler = TaskScheduler::new(
                        repository,
                        self.executors.command.clone(),
                        self.budget.clone(),
                        self.clock.clone(),
                    );
                    let (control, signals) = task_run_controls();
                    let execution = scheduler.run(child.id(), signals);
                    tokio::pin!(execution);
                    let result = tokio::select! { biased;
                        reason = cancel.cancelled() => { control.cancel_for(reason); execution.await }
                        result = &mut execution => result,
                    };
                    result.map_err(|error| match error {
                        TaskSchedulerError::Storage(error) => error,
                        _ => TaskRepositoryError::Unavailable,
                    })?;
                }
                MixedChildSnapshot::Upload(child) => {
                    BatchUploadRunner::new(
                        repository,
                        self.executors.upload.clone(),
                        self.budget.clone(),
                        self.clock.clone(),
                    )
                    .run(child.id, true, cancel.clone())
                    .await
                    .map_err(|error| match error {
                        UploadRepositoryError::Conflict => TaskRepositoryError::Conflict,
                        UploadRepositoryError::Missing => TaskRepositoryError::Missing,
                        UploadRepositoryError::Corrupt => TaskRepositoryError::Corrupt,
                        UploadRepositoryError::Unavailable => TaskRepositoryError::Unavailable,
                    })?;
                }
                MixedChildSnapshot::Download(child) => {
                    BatchDownloadRunner::new(
                        repository,
                        self.executors.download.clone(),
                        self.budget.clone(),
                        self.clock.clone(),
                    )
                    .run(child.id, true, cancel.clone())
                    .await?;
                }
            }
            row = self.load(&snapshot, row.host_id).await?;
            row = self
                .commit(
                    &snapshot,
                    &row,
                    MixedTargetAction::CompleteStep(cancel.reason().map(stop_reason)),
                )
                .await?;
            if row.state != MixedTaskState::Running {
                return Ok(());
            }
        }
    }
}
fn upload_failure(error: UploadRepositoryError) -> TaskRepositoryError {
    match error {
        UploadRepositoryError::Conflict => TaskRepositoryError::Conflict,
        UploadRepositoryError::Missing => TaskRepositoryError::Missing,
        UploadRepositoryError::Unavailable => TaskRepositoryError::Unavailable,
        UploadRepositoryError::Corrupt => TaskRepositoryError::Corrupt,
    }
}
fn upload_content(content: DownloadContent) -> UploadContent {
    UploadContent {
        size: content.size,
        sha256: content.sha256,
    }
}
fn stop_reason(reason: TaskStopReason) -> MixedStopReason {
    match reason {
        TaskStopReason::TotalTimeout => MixedStopReason::TotalTimeout,
        TaskStopReason::FailureThreshold => MixedStopReason::FailureThreshold,
        TaskStopReason::OutputFailure | TaskStopReason::TransportLost => {
            MixedStopReason::StorageFailure
        }
        _ => MixedStopReason::UserCancelled,
    }
}
