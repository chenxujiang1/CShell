use super::*;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct BatchDownloadRunner {
    repository: Arc<dyn DownloadRepository>,
    executor: Arc<dyn DownloadExecutor>,
    budget: TaskResourceBudget,
    clock: Arc<dyn TaskClock>,
}
enum Fault {
    Transfer(DownloadReason),
    Storage(TaskRepositoryError),
}
impl From<DownloadReason> for Fault {
    fn from(value: DownloadReason) -> Self {
        Self::Transfer(value)
    }
}
impl From<TaskRepositoryError> for Fault {
    fn from(value: TaskRepositoryError) -> Self {
        Self::Storage(value)
    }
}
impl BatchDownloadRunner {
    pub fn new(
        repository: Arc<dyn DownloadRepository>,
        executor: Arc<dyn DownloadExecutor>,
        budget: TaskResourceBudget,
        clock: Arc<dyn TaskClock>,
    ) -> Self {
        Self {
            repository,
            executor,
            budget,
            clock,
        }
    }
    async fn load(
        &self,
        id: TaskRunId,
    ) -> Result<(BatchDownloadSnapshot, Vec<DownloadTarget>), TaskRepositoryError> {
        self.repository
            .load_download(id)
            .await?
            .ok_or(TaskRepositoryError::Missing)
    }
    async fn checkpoint(
        &self,
        snapshot: &BatchDownloadSnapshot,
        row: &mut DownloadTarget,
        change: impl FnOnce(&mut DownloadTarget),
    ) -> Result<(), TaskRepositoryError> {
        let mut next = row.clone();
        change(&mut next);
        next.revision = row
            .revision
            .checked_add(1)
            .ok_or(TaskRepositoryError::Corrupt)?;
        next.updated_at_ms = self
            .clock
            .now_ms()
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        row.validate_next(&next, snapshot)
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        self.repository.commit_download_target(row, &next).await?;
        *row = next;
        Ok(())
    }
    pub async fn recover(&self, id: TaskRunId) -> Result<(), TaskRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, mut rows) = self.load(id).await?;
        for row in &mut rows {
            if row.phase.active() {
                self.checkpoint(&snapshot, row, |next| {
                    next.phase = DownloadPhase::Unknown;
                    next.reason = Some(DownloadReason::Restarted);
                })
                .await?;
            }
        }
        Ok(())
    }
    pub async fn cancel_pending(&self, id: TaskRunId) -> Result<(), TaskRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, mut rows) = self.load(id).await?;
        for row in &mut rows {
            if row.phase == DownloadPhase::Pending {
                self.checkpoint(&snapshot, row, |next| {
                    next.phase = DownloadPhase::Cancelled;
                    next.reason = Some(DownloadReason::Cancelled);
                })
                .await?;
            }
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
        let (snapshot, rows) = self.load(id).await?;
        let snapshot = Arc::new(snapshot);
        let queued_at = tokio::time::Instant::now();
        let mut pending = rows
            .into_iter()
            .filter(|row| row.phase == DownloadPhase::Pending);
        let mut jobs = tokio::task::JoinSet::new();
        loop {
            while jobs.len() < usize::from(snapshot.definition.concurrency) {
                let Some(row) = pending.next() else {
                    break;
                };
                let runner = self.clone();
                let snapshot = snapshot.clone();
                let cancel = cancel.clone();
                jobs.spawn(async move {
                    runner
                        .target(&snapshot, row, false, cancel, queued_at)
                        .await
                });
            }
            let Some(result) = jobs.join_next().await else {
                break;
            };
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some(TaskRepositoryError::Unavailable),
            };
            if let Some(error) = error {
                cancel.cancel(TaskStopReason::TransportLost);
                while jobs.join_next().await.is_some() {}
                return Err(error);
            }
        }
        Ok(())
    }
    pub async fn resume(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
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
        let (snapshot, rows) = self.load(id).await?;
        let row = rows
            .into_iter()
            .find(|row| row.host_id == host)
            .ok_or(TaskRepositoryError::Missing)?;
        if row.revision != revision || !row.can_resume() {
            return Err(TaskRepositoryError::Conflict);
        }
        self.target(&snapshot, row, true, cancel, tokio::time::Instant::now())
            .await
    }
    async fn target(
        &self,
        snapshot: &BatchDownloadSnapshot,
        mut row: DownloadTarget,
        resume: bool,
        cancel: TaskCancellation,
        queued_at: tokio::time::Instant,
    ) -> Result<(), TaskRepositoryError> {
        let lease = loop {
            let reason = if cancel.reason().is_some() {
                Some(DownloadReason::Cancelled)
            } else if queued_at.elapsed()
                >= Duration::from_millis(snapshot.definition.queue_timeout_ms)
            {
                Some(DownloadReason::Timeout)
            } else {
                None
            };
            if let Some(reason) = reason {
                return self.fail(snapshot, &mut row, reason).await;
            }
            if let Some(lease) = self
                .budget
                .try_acquire_frozen(
                    snapshot.id,
                    &snapshot.targets,
                    snapshot.definition.concurrency,
                    row.host_id,
                )
                .map_err(|_| TaskRepositoryError::Corrupt)?
            {
                break lease;
            }
            tokio::select! { _ = self.budget.changed() => {}, _ = tokio::time::sleep(Duration::from_millis(50)) => {}, _ = cancel.cancelled() => {} }
        };
        let connected = tokio::select! { biased;
            _ = cancel.cancelled() => Err(DownloadReason::Cancelled),
            result = tokio::time::timeout(Duration::from_millis(snapshot.definition.connect_timeout_ms), self.executor.connect(snapshot, row.host_id, cancel.clone())) => result.unwrap_or(Err(DownloadReason::Timeout)),
        };
        let mut remote = match connected {
            Ok(remote) => remote,
            Err(reason) => return self.fail(snapshot, &mut row, reason).await,
        };
        let mut local = match self.executor.local(snapshot, row.host_id).await {
            Ok(local) => local,
            Err(reason) => {
                let _closed = tokio::time::timeout(Duration::from_secs(3), remote.close()).await;
                return self.fail(snapshot, &mut row, reason).await;
            }
        };
        let outcome = tokio::select! { biased;
            _ = cancel.cancelled() => Err(Fault::Transfer(DownloadReason::Cancelled)),
            result = tokio::time::timeout(Duration::from_millis(snapshot.definition.transfer_timeout_ms), self.transfer(snapshot, &mut row, resume, remote.as_mut(), local.as_mut())) => result.unwrap_or(Err(Fault::Transfer(DownloadReason::Timeout))),
        };
        let _closed = tokio::time::timeout(Duration::from_secs(3), async {
            local.close().await;
            remote.close().await;
        })
        .await;
        drop(local);
        drop(remote);
        drop(lease);
        match outcome {
            Ok(()) => Ok(()),
            Err(Fault::Storage(error)) => Err(error),
            Err(Fault::Transfer(reason)) => self.fail(snapshot, &mut row, reason).await,
        }
    }
    async fn fail(
        &self,
        snapshot: &BatchDownloadSnapshot,
        row: &mut DownloadTarget,
        reason: DownloadReason,
    ) -> Result<(), TaskRepositoryError> {
        self.checkpoint(snapshot, row, |next| {
            next.reason = Some(reason);
            next.phase = if next.committed {
                next.intent = DownloadIntent::Cleanup;
                DownloadPhase::CleanupRequired
            } else if next.phase == DownloadPhase::Unknown
                || next.phase.active()
                    && matches!(
                        reason,
                        DownloadReason::Cancelled
                            | DownloadReason::Timeout
                            | DownloadReason::Transport
                            | DownloadReason::LocalIo
                    )
            {
                DownloadPhase::Unknown
            } else if next.phase == DownloadPhase::Pending && reason == DownloadReason::Cancelled {
                DownloadPhase::Cancelled
            } else {
                DownloadPhase::Failed
            };
        })
        .await
    }
    async fn transfer(
        &self,
        snapshot: &BatchDownloadSnapshot,
        row: &mut DownloadTarget,
        resume: bool,
        remote: &mut dyn DownloadRemote,
        local: &mut dyn DownloadLocal,
    ) -> Result<(), Fault> {
        if !resume {
            self.checkpoint(snapshot, row, |next| {
                next.phase = DownloadPhase::Inspecting;
                next.intent = DownloadIntent::Inspect;
            })
            .await?;
        }
        let content = remote
            .inspect(
                &snapshot.definition.remote_source,
                snapshot.definition.maximum_bytes,
            )
            .await?;
        let mut hash = Sha256::new();
        let mut offset = 0;
        if resume {
            if row.content != Some(content) {
                return Err(DownloadReason::SourceChanged.into());
            }
            let size = local.reopen().await?;
            if size < row.bytes || size > content.size {
                return Err(DownloadReason::Checksum.into());
            }
            while offset < size {
                let limit = UPLOAD_CHUNK_BYTES.min((size - offset) as usize);
                let bytes = remote.read(offset, limit).await?;
                if bytes.is_empty()
                    || bytes.len() > limit
                    || local.read(offset, bytes.len()).await? != bytes
                {
                    return Err(DownloadReason::Checksum.into());
                }
                hash.update(&bytes);
                offset += bytes.len() as u64;
            }
            local.flush().await?;
            self.checkpoint(snapshot, row, |next| {
                next.phase = DownloadPhase::Writing;
                next.intent = DownloadIntent::Write;
                next.bytes = offset;
                next.reason = None;
            })
            .await?;
        } else {
            let planned = local
                .plan_publication(snapshot.definition.publication_policy, content)
                .await?;
            self.checkpoint(snapshot, row, |next| {
                next.phase = DownloadPhase::Creating;
                next.intent = DownloadIntent::Create;
                next.content = Some(content);
                next.publication = planned;
            })
            .await?;
            local.create().await?;
            self.checkpoint(snapshot, row, |next| {
                next.phase = DownloadPhase::Writing;
                next.intent = DownloadIntent::Write;
                next.temporary_owned = true;
            })
            .await?;
        }
        while offset < content.size {
            let limit = UPLOAD_CHUNK_BYTES.min((content.size - offset) as usize);
            let bytes = remote.read(offset, limit).await?;
            if bytes.is_empty() || bytes.len() > limit {
                return Err(DownloadReason::SourceChanged.into());
            }
            local.write(offset, &bytes).await?;
            hash.update(&bytes);
            offset += bytes.len() as u64;
            if offset.saturating_sub(row.bytes) >= 1024 * 1024 {
                local.flush().await?;
                self.checkpoint(snapshot, row, |next| {
                    next.bytes = offset;
                })
                .await?;
            }
        }
        if !remote.read(offset, 1).await?.is_empty()
            || <[u8; 32]>::from(hash.finalize()) != content.sha256
        {
            return Err(DownloadReason::SourceChanged.into());
        }
        local.flush().await?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = DownloadPhase::Verifying;
            next.intent = DownloadIntent::Verify;
            next.bytes = offset;
        })
        .await?;
        if local.verify(snapshot.definition.maximum_bytes).await? != content {
            return Err(DownloadReason::Checksum.into());
        }
        if remote.recheck().await? != content {
            return Err(DownloadReason::SourceChanged.into());
        }
        self.checkpoint(snapshot, row, |next| {
            next.phase = DownloadPhase::PublishIntent;
            next.intent = DownloadIntent::Publish;
            next.verified = true;
        })
        .await?;
        let publication = local
            .publish_planned(
                snapshot.definition.publication_policy,
                row.publication,
                content,
            )
            .await?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = DownloadPhase::Committed;
            next.committed = true;
            next.publication = publication;
        })
        .await?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = DownloadPhase::CleanupRequired;
            next.intent = DownloadIntent::Cleanup;
        })
        .await?;
        local.cleanup().await?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = DownloadPhase::Succeeded;
            next.temporary_cleaned = true;
            next.reason = None;
        })
        .await?;
        Ok(())
    }
    /// Inspect only local artifacts, bound to the stored row under a run owner.
    pub async fn probe(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        cancel: TaskCancellation,
    ) -> Result<DownloadProbe, TaskRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, rows) = self.load(id).await?;
        let row = rows
            .iter()
            .find(|row| row.host_id == host)
            .ok_or(TaskRepositoryError::Missing)?;
        if row.revision != revision {
            return Err(TaskRepositoryError::Conflict);
        }
        let mut local = self
            .executor
            .local(&snapshot, host)
            .await
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        let result = tokio::select! { biased;
            _ = cancel.cancelled() => Err(TaskRepositoryError::Unavailable),
            result = tokio::time::timeout(Duration::from_millis(snapshot.definition.transfer_timeout_ms), local.probe(snapshot.definition.maximum_bytes)) => result.map_err(|_| TaskRepositoryError::Unavailable).and_then(|value| value.map_err(|_| TaskRepositoryError::Unavailable)),
        };
        let _closed = tokio::time::timeout(Duration::from_secs(3), local.close()).await;
        result
    }
    pub async fn resolve(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        confirmed: bool,
        adopt: bool,
        cancel: TaskCancellation,
    ) -> Result<(), TaskRepositoryError> {
        if !confirmed {
            return Err(TaskRepositoryError::Conflict);
        }
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        let (snapshot, rows) = self.load(id).await?;
        let mut row = rows
            .into_iter()
            .find(|row| row.host_id == host)
            .ok_or(TaskRepositoryError::Missing)?;
        if row.revision != revision
            || if adopt {
                !row.can_adopt()
            } else {
                !row.can_cleanup()
            }
        {
            return Err(TaskRepositoryError::Conflict);
        }
        let mut local = self
            .executor
            .local(&snapshot, host)
            .await
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        let result = tokio::select! { biased;
            _ = cancel.cancelled() => Err(TaskRepositoryError::Unavailable),
            result = tokio::time::timeout(Duration::from_millis(snapshot.definition.transfer_timeout_ms), self.resolve_local(&snapshot, &mut row, adopt, local.as_mut())) => result.unwrap_or(Err(TaskRepositoryError::Unavailable)),
        };
        let _closed = tokio::time::timeout(Duration::from_secs(3), local.close()).await;
        result
    }
    async fn resolve_local(
        &self,
        snapshot: &BatchDownloadSnapshot,
        row: &mut DownloadTarget,
        adopt: bool,
        local: &mut dyn DownloadLocal,
    ) -> Result<(), TaskRepositoryError> {
        if adopt {
            let probe = local
                .probe(snapshot.definition.maximum_bytes)
                .await
                .map_err(|_| TaskRepositoryError::Unavailable)?;
            if probe.destination != row.content {
                return Err(TaskRepositoryError::Conflict);
            }
            let publication = local
                .recover_publication(
                    snapshot.definition.publication_policy,
                    row.publication,
                    row.content.ok_or(TaskRepositoryError::Corrupt)?,
                )
                .await
                .map_err(|_| TaskRepositoryError::Conflict)?;
            self.checkpoint(snapshot, row, |next| {
                next.phase = DownloadPhase::Committed;
                next.committed = true;
                next.adopted = true;
                next.publication = publication;
            })
            .await?;
        }
        if row.phase != DownloadPhase::CleanupRequired {
            self.checkpoint(snapshot, row, |next| {
                next.phase = DownloadPhase::CleanupRequired;
                next.intent = DownloadIntent::Cleanup;
            })
            .await?;
        }
        local
            .cleanup()
            .await
            .map_err(|_| TaskRepositoryError::Unavailable)?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = if next.committed {
                DownloadPhase::Succeeded
            } else {
                DownloadPhase::Failed
            };
            next.temporary_cleaned = true;
        })
        .await
    }
}
