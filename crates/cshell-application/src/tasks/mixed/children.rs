//! Leaf repositories are scoped to one frozen parent/host/step and never create runs.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct ChildRepository {
    pub repository: Arc<dyn MixedTaskRepository>,
    pub snapshot: MixedTaskSnapshot,
    pub host: HostId,
    pub step: usize,
    pub clock: Arc<dyn TaskClock>,
    pub readonly: bool,
}
impl ChildRepository {
    pub async fn load(&self) -> Result<MixedTargetRun, TaskRepositoryError> {
        let (snapshot, row) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.repository
                .load_mixed_target(self.snapshot.id(), self.host),
        )
        .await
        .map_err(|_| TaskRepositoryError::Unavailable)??
        .ok_or(TaskRepositoryError::Missing)?;
        if snapshot.fingerprint() != self.snapshot.fingerprint() {
            return Err(TaskRepositoryError::Corrupt);
        }
        if self.readonly {
            if self.step >= row.steps.len()
                || !row.file_repair_allowed(self.step, MixedFileOperation::Probe)
            {
                return Err(TaskRepositoryError::Conflict);
            }
        } else if row.state != MixedTaskState::Running
            || row.next_step != self.step
            || row.steps[self.step].state != MixedTaskState::Running
        {
            return Err(TaskRepositoryError::Conflict);
        }
        Ok(row)
    }
    async fn commit(
        &self,
        row: MixedTargetRun,
        action: MixedTargetAction,
    ) -> Result<(), TaskRepositoryError> {
        if self.readonly {
            return Err(TaskRepositoryError::Conflict);
        }
        let now = self
            .clock
            .now_ms()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        let patch = row
            .prepare(&self.snapshot, action, now)
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        let revision = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.repository.commit_mixed_target(&patch),
        )
        .await
        .map_err(|_| TaskRepositoryError::Unavailable)??;
        if revision != patch.after().revision {
            return Err(TaskRepositoryError::Unavailable);
        }
        Ok(())
    }
    fn child(&self) -> Result<MixedChildSnapshot, TaskRepositoryError> {
        self.snapshot
            .child(self.host, self.step)
            .map_err(|_| TaskRepositoryError::Corrupt)
    }
}
#[async_trait]
impl TaskRepository for ChildRepository {
    async fn create_task_run(&self, _: &TaskRunRecord) -> Result<(), TaskRepositoryError> {
        Err(TaskRepositoryError::Conflict)
    }
    async fn load_task_run(
        &self,
        id: TaskRunId,
    ) -> Result<Option<TaskRunRecord>, TaskRepositoryError> {
        let MixedChildSnapshot::Command(child) = self.child()? else {
            return Err(TaskRepositoryError::Corrupt);
        };
        if child.id() != id {
            return Ok(None);
        }
        let row = self.load().await?;
        let MixedReceipt::Command { meta, target } = &row.steps[self.step].receipt else {
            return Err(TaskRepositoryError::Corrupt);
        };
        TaskRunRecord::restore(child, meta.clone(), vec![target.clone()])
            .map(Some)
            .map_err(|_| TaskRepositoryError::Corrupt)
    }
    async fn commit_task_patch(&self, patch: &TaskRunPatch) -> Result<u64, TaskRepositoryError> {
        let MixedChildSnapshot::Command(child) = self.child()? else {
            return Err(TaskRepositoryError::Corrupt);
        };
        if patch.id() != child.id() || patch.snapshot_fingerprint() != child.fingerprint() {
            return Err(TaskRepositoryError::Conflict);
        }
        let row = self.load().await?;
        self.commit(row, MixedTargetAction::Command(patch.clone()))
            .await?;
        Ok(patch.after().revision)
    }
    async fn list_task_runs(
        &self,
        _: Option<TaskRunId>,
        _: usize,
        _: bool,
    ) -> Result<Vec<TaskRunSummary>, TaskRepositoryError> {
        // This private adapter is an execution port, not a catalog or discovery surface.
        Err(TaskRepositoryError::Corrupt)
    }
}
fn upload_error(error: TaskRepositoryError) -> UploadRepositoryError {
    match error {
        TaskRepositoryError::Conflict => UploadRepositoryError::Conflict,
        TaskRepositoryError::Missing => UploadRepositoryError::Missing,
        TaskRepositoryError::Corrupt => UploadRepositoryError::Corrupt,
        TaskRepositoryError::Unavailable => UploadRepositoryError::Unavailable,
    }
}
#[async_trait]
impl UploadRepository for ChildRepository {
    async fn create_upload(&self, _: &BatchUploadSnapshot) -> Result<(), UploadRepositoryError> {
        Err(UploadRepositoryError::Conflict)
    }
    async fn load_upload(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchUploadSnapshot, Vec<UploadTarget>)>, UploadRepositoryError> {
        let MixedChildSnapshot::Upload(child) = self.child().map_err(upload_error)? else {
            return Err(UploadRepositoryError::Corrupt);
        };
        if child.id != id {
            return Ok(None);
        }
        let row = self.load().await.map_err(upload_error)?;
        let MixedReceipt::Upload { row } = &row.steps[self.step].receipt else {
            return Err(UploadRepositoryError::Corrupt);
        };
        Ok(Some((child, vec![row.clone()])))
    }
    async fn commit_upload_target(
        &self,
        before: &UploadTarget,
        after: &UploadTarget,
    ) -> Result<(), UploadRepositoryError> {
        let child = self.child().map_err(upload_error)?;
        if !matches!(child, MixedChildSnapshot::Upload(_))
            || child.id() != before.run_id
            || before.host_id != self.host
        {
            return Err(UploadRepositoryError::Conflict);
        }
        let row = self.load().await.map_err(upload_error)?;
        self.commit(
            row,
            MixedTargetAction::Upload {
                before: before.clone(),
                after: after.clone(),
            },
        )
        .await
        .map_err(upload_error)
    }
    async fn list_uploads(
        &self,
        _: Option<TaskRunId>,
        _: usize,
    ) -> Result<Vec<UploadRunSummary>, UploadRepositoryError> {
        Err(UploadRepositoryError::Corrupt)
    }
}
#[async_trait]
impl DownloadRepository for ChildRepository {
    async fn create_download(&self, _: &BatchDownloadSnapshot) -> Result<(), TaskRepositoryError> {
        Err(TaskRepositoryError::Conflict)
    }
    async fn load_download(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchDownloadSnapshot, Vec<DownloadTarget>)>, TaskRepositoryError> {
        let MixedChildSnapshot::Download(child) = self.child()? else {
            return Err(TaskRepositoryError::Corrupt);
        };
        if child.id != id {
            return Ok(None);
        }
        let row = self.load().await?;
        let MixedReceipt::Download { row } = &row.steps[self.step].receipt else {
            return Err(TaskRepositoryError::Corrupt);
        };
        Ok(Some((child, vec![row.clone()])))
    }
    async fn commit_download_target(
        &self,
        before: &DownloadTarget,
        after: &DownloadTarget,
    ) -> Result<(), TaskRepositoryError> {
        let child = self.child()?;
        if !matches!(child, MixedChildSnapshot::Download(_))
            || child.id() != before.run_id
            || before.host_id != self.host
        {
            return Err(TaskRepositoryError::Conflict);
        }
        let row = self.load().await?;
        self.commit(
            row,
            MixedTargetAction::Download {
                before: before.clone(),
                after: after.clone(),
            },
        )
        .await
    }
    async fn list_downloads(
        &self,
        _: Option<TaskRunId>,
        _: usize,
    ) -> Result<Vec<DownloadRunSummary>, TaskRepositoryError> {
        Err(TaskRepositoryError::Corrupt)
    }
}
