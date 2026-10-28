use super::*;
use std::{sync::Arc, time::Duration};

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct BatchUploadRunner {
    repository: Arc<dyn UploadRepository>,
    executor: Arc<dyn UploadExecutor>,
    budget: TaskResourceBudget,
    clock: Arc<dyn TaskClock>,
}
enum Fault {
    Transfer(UploadReason),
    Storage(UploadRepositoryError),
}
impl From<UploadReason> for Fault {
    fn from(value: UploadReason) -> Self {
        Self::Transfer(value)
    }
}
impl From<UploadRepositoryError> for Fault {
    fn from(value: UploadRepositoryError) -> Self {
        Self::Storage(value)
    }
}
impl BatchUploadRunner {
    pub fn new(
        repository: Arc<dyn UploadRepository>,
        executor: Arc<dyn UploadExecutor>,
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
    fn now(&self) -> Result<u64, UploadRepositoryError> {
        self.clock
            .now_ms()
            .map_err(|_| UploadRepositoryError::Unavailable)
    }
    async fn checkpoint(
        &self,
        snapshot: &BatchUploadSnapshot,
        row: &mut UploadTarget,
        change: impl FnOnce(&mut UploadTarget),
    ) -> Result<(), UploadRepositoryError> {
        let mut next = row.clone();
        change(&mut next);
        next.revision = row
            .revision
            .checked_add(1)
            .ok_or(UploadRepositoryError::Corrupt)?;
        next.updated_at_ms = self.now()?;
        row.validate_next(&next, snapshot)
            .map_err(|_| UploadRepositoryError::Corrupt)?;
        self.repository.commit_upload_target(row, &next).await?;
        *row = next;
        Ok(())
    }
    async fn load(
        &self,
        id: TaskRunId,
    ) -> Result<(BatchUploadSnapshot, Vec<UploadTarget>), UploadRepositoryError> {
        self.repository
            .load_upload(id)
            .await?
            .ok_or(UploadRepositoryError::Missing)
    }
    /// Called only when no owner exists. It never creates SSH/SFTP connections.
    pub async fn recover(&self, id: TaskRunId) -> Result<(), UploadRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| UploadRepositoryError::Conflict)?;
        let (snapshot, mut rows) = self.load(id).await?;
        for row in &mut rows {
            if row.phase.active() {
                self.checkpoint(&snapshot, row, |next| {
                    next.phase = UploadPhase::Unknown;
                    next.reason = Some(UploadReason::Restarted);
                })
                .await?;
            }
        }
        Ok(())
    }
    /// Explicit start runs Pending targets only; every recovered target remains untouched.
    pub async fn cancel_pending(&self, id: TaskRunId) -> Result<(), UploadRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| UploadRepositoryError::Conflict)?;
        let (snapshot, mut rows) = self.load(id).await?;
        for row in &mut rows {
            if row.phase == UploadPhase::Pending {
                self.checkpoint(&snapshot, row, |next| {
                    next.phase = UploadPhase::Cancelled;
                    next.reason = Some(UploadReason::Cancelled);
                })
                .await?;
            }
        }
        Ok(())
    }
    /// Explicit start runs Pending targets only; every recovered target remains untouched.
    pub async fn run(
        &self,
        id: TaskRunId,
        confirmed: bool,
        cancel: TaskCancellation,
    ) -> Result<(), UploadRepositoryError> {
        if !confirmed {
            return Err(UploadRepositoryError::Conflict);
        }
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| UploadRepositoryError::Conflict)?;
        let (snapshot, rows) = self.load(id).await?;
        // Targets waiting for this run's concurrency slots share the same
        // queue deadline as targets already waiting for a resource lease.
        let queued_at = tokio::time::Instant::now();
        let mut jobs = tokio::task::JoinSet::new();
        let mut pending = rows
            .into_iter()
            .filter(|row| row.phase == UploadPhase::Pending);
        loop {
            while jobs.len() < usize::from(snapshot.definition.concurrency) {
                let Some(row) = pending.next() else {
                    break;
                };
                let runner = self.clone();
                let snapshot = snapshot.clone();
                let cancel = cancel.clone();
                jobs.spawn(async move {
                    runner.target(&snapshot, row, false, cancel, queued_at).await
                });
            }
            if jobs.is_empty() {
                break;
            }
            let Some(result) = jobs.join_next().await else {
                break;
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    cancel.cancel(TaskStopReason::OutputFailure);
                    while jobs.join_next().await.is_some() {}
                    return Err(error);
                }
                Err(_) => {
                    cancel.cancel(TaskStopReason::TransportLost);
                    while jobs.join_next().await.is_some() {}
                    return Err(UploadRepositoryError::Unavailable);
                }
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
    ) -> Result<(), UploadRepositoryError> {
        if !confirmed {
            return Err(UploadRepositoryError::Conflict);
        }
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| UploadRepositoryError::Conflict)?;
        let (snapshot, rows) = self.load(id).await?;
        let row = rows
            .into_iter()
            .find(|row| row.host_id == host)
            .ok_or(UploadRepositoryError::Missing)?;
        if row.revision != revision
            || row.phase != UploadPhase::Unknown
            || !row.temporary_owned
            || row.verified
            || row.intent == UploadIntent::Publish
            || row.committed
        {
            return Err(UploadRepositoryError::Conflict);
        }
        self.target(&snapshot, row, true, cancel, tokio::time::Instant::now())
            .await
    }
    async fn target(
        &self,
        snapshot: &BatchUploadSnapshot,
        mut row: UploadTarget,
        resume: bool,
        cancel: TaskCancellation,
        queued_at: tokio::time::Instant,
    ) -> Result<(), UploadRepositoryError> {
        let lease = loop {
            if cancel.reason().is_some() {
                if row.phase == UploadPhase::Pending {
                    self.checkpoint(snapshot, &mut row, |next| {
                        next.phase = UploadPhase::Cancelled;
                        next.reason = Some(UploadReason::Cancelled);
                    })
                    .await?;
                }
                return Ok(());
            }
            if queued_at.elapsed() >= Duration::from_millis(snapshot.definition.queue_timeout_ms) {
                if row.phase == UploadPhase::Pending {
                    self.checkpoint(snapshot, &mut row, |next| {
                        next.phase = UploadPhase::Failed;
                        next.reason = Some(UploadReason::Timeout);
                    })
                    .await?;
                }
                return Ok(());
            }
            if let Some(lease) = self
                .budget
                .try_acquire_frozen(
                    snapshot.id,
                    &snapshot.targets,
                    snapshot.definition.concurrency,
                    row.host_id,
                )
                .map_err(|_| UploadRepositoryError::Corrupt)?
            {
                break lease;
            }
            tokio::select! { _ = self.budget.changed() => {}, _ = tokio::time::sleep(Duration::from_millis(50)) => {}, _ = cancel.cancelled() => {} }
        };
        let connected = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(UploadReason::Cancelled),
            result = tokio::time::timeout(Duration::from_millis(snapshot.definition.connect_timeout_ms), self.executor.connect(snapshot, row.host_id, cancel.clone())) => result.unwrap_or(Err(UploadReason::Timeout)),
        };
        let mut remote = match connected {
            Ok(remote) => remote,
            Err(reason) => {
                if row.phase == UploadPhase::Pending {
                    self.checkpoint(snapshot, &mut row, |next| {
                        next.phase = UploadPhase::Failed;
                        next.reason = Some(reason);
                    })
                    .await?;
                }
                drop(lease);
                return Ok(());
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Fault::Transfer(UploadReason::Cancelled)),
            result = tokio::time::timeout(Duration::from_millis(snapshot.definition.transfer_timeout_ms),
                self.transfer(snapshot, &mut row, resume, remote.as_mut())) => result.unwrap_or(Err(Fault::Transfer(UploadReason::Timeout))),
        };
        // Only channel/connection cleanup occurs automatically. Never remove an
        // artifact after an uncertain RPC or repository commit.
        let _closed = tokio::time::timeout(Duration::from_secs(3), remote.close()).await;
        drop(remote);
        drop(lease);
        match outcome {
            Ok(()) => Ok(()),
            Err(Fault::Storage(error)) => Err(error),
            Err(Fault::Transfer(reason)) => {
                if row.phase == UploadPhase::Unknown {
                    return self
                        .checkpoint(snapshot, &mut row, |next| {
                            next.reason = Some(reason);
                        })
                        .await;
                }
                self.checkpoint(snapshot, &mut row, |next| {
                    next.reason = Some(reason);
                    next.phase = if next.committed {
                        next.intent = UploadIntent::Cleanup;
                        UploadPhase::CleanupRequired
                    } else if matches!(
                        reason,
                        UploadReason::Transport | UploadReason::Timeout | UploadReason::Cancelled
                    ) && next.phase.active()
                    {
                        UploadPhase::Unknown
                    } else if next.phase == UploadPhase::Pending
                        && reason == UploadReason::Cancelled
                    {
                        UploadPhase::Cancelled
                    } else {
                        UploadPhase::Failed
                    };
                })
                .await
            }
        }
    }
    async fn transfer(
        &self,
        snapshot: &BatchUploadSnapshot,
        row: &mut UploadTarget,
        resume: bool,
        remote: &mut dyn UploadRemote,
    ) -> Result<(), Fault> {
        let temporary = snapshot
            .temporary(row.host_id)
            .map_err(|_| UploadReason::Invalid)?;
        let mut source = self.executor.source(snapshot).await?;
        let mut digest = Sha256::new();
        let mut offset = 0;
        if resume {
            let size = remote.reopen(&temporary).await?;
            if size < row.bytes || size > snapshot.definition.size {
                return Err(UploadReason::Checksum.into());
            }
            while offset < size {
                let bytes = source
                    .read(offset, UPLOAD_CHUNK_BYTES.min((size - offset) as usize))
                    .await?;
                if bytes.is_empty() || bytes.len() > UPLOAD_CHUNK_BYTES {
                    return Err(UploadReason::SourceChanged.into());
                }
                let mut compared = 0;
                while compared < bytes.len() {
                    let previous = remote
                        .read(&temporary, offset + compared as u64, bytes.len() - compared)
                        .await?;
                    if previous.is_empty()
                        || previous.len() > bytes.len() - compared
                        || previous != bytes[compared..compared + previous.len()]
                    {
                        return Err(UploadReason::Checksum.into());
                    }
                    compared += previous.len();
                }
                digest.update(&bytes);
                offset += bytes.len() as u64;
            }
            self.checkpoint(snapshot, row, |next| {
                next.phase = UploadPhase::Writing;
                next.intent = UploadIntent::Write;
                next.bytes = size;
                next.reason = None;
            })
            .await?;
        } else {
            let planned = remote
                .plan_publication(
                    &temporary,
                    &snapshot.definition.remote_destination,
                    &snapshot
                        .backup(row.host_id)
                        .map_err(|_| UploadReason::Invalid)?,
                    snapshot.definition.publication_policy,
                    UploadContent {
                        size: snapshot.definition.size,
                        sha256: snapshot.definition.sha256,
                    },
                )
                .await?;
            self.checkpoint(snapshot, row, |next| {
                next.phase = UploadPhase::Creating;
                next.intent = UploadIntent::Create;
                next.publication = planned;
            })
            .await?;
            remote.create(&temporary).await?;
            self.checkpoint(snapshot, row, |next| {
                next.phase = UploadPhase::Writing;
                next.intent = UploadIntent::Write;
                next.temporary_owned = true;
            })
            .await?;
        }
        while offset < snapshot.definition.size {
            let bytes = source
                .read(
                    offset,
                    UPLOAD_CHUNK_BYTES.min((snapshot.definition.size - offset) as usize),
                )
                .await?;
            if bytes.is_empty()
                || bytes.len() > UPLOAD_CHUNK_BYTES
                || offset + bytes.len() as u64 > snapshot.definition.size
            {
                return Err(UploadReason::SourceChanged.into());
            }
            remote.write(offset, &bytes).await?;
            digest.update(&bytes);
            offset += bytes.len() as u64;
            if offset.saturating_sub(row.bytes) >= 1024 * 1024 {
                self.checkpoint(snapshot, row, |next| {
                    next.bytes = offset;
                })
                .await?;
            }
        }
        if !source.read(offset, 1).await?.is_empty()
            || <[u8; 32]>::from(digest.finalize()) != snapshot.definition.sha256
        {
            return Err(UploadReason::SourceChanged.into());
        }
        self.checkpoint(snapshot, row, |next| {
            next.phase = UploadPhase::Verifying;
            next.intent = UploadIntent::Verify;
            next.bytes = offset;
        })
        .await?;
        let content = remote.verify(&temporary).await?;
        if content
            != (UploadContent {
                size: snapshot.definition.size,
                sha256: snapshot.definition.sha256,
            })
        {
            return Err(UploadReason::Checksum.into());
        }
        self.checkpoint(snapshot, row, |next| {
            next.phase = UploadPhase::PublishIntent;
            next.intent = UploadIntent::Publish;
            next.verified = true;
        })
        .await?;
        let publication = remote
            .publish_planned(
                &temporary,
                &snapshot.definition.remote_destination,
                &snapshot
                    .backup(row.host_id)
                    .map_err(|_| UploadReason::Invalid)?,
                snapshot.definition.publication_policy,
                row.publication,
                UploadContent {
                    size: snapshot.definition.size,
                    sha256: snapshot.definition.sha256,
                },
            )
            .await?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = UploadPhase::Committed;
            next.committed = true;
            next.publication = publication;
        })
        .await?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = UploadPhase::CleanupRequired;
            next.intent = UploadIntent::Cleanup;
        })
        .await?;
        remote.cleanup(&temporary).await?;
        self.checkpoint(snapshot, row, |next| {
            next.phase = UploadPhase::Succeeded;
            next.intent = UploadIntent::Cleanup;
            next.temporary_cleaned = true;
            next.reason = None;
        })
        .await?;
        Ok(())
    }
    pub async fn probe(
        &self,
        id: TaskRunId,
        host: HostId,
    ) -> Result<UploadProbe, UploadRepositoryError> {
        self.probe_controlled(id, host, 0, TaskCancellation::detached())
            .await
    }
    pub async fn probe_controlled(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        cancel: TaskCancellation,
    ) -> Result<UploadProbe, UploadRepositoryError> {
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| UploadRepositoryError::Conflict)?;
        let (snapshot, rows) = self.load(id).await?;
        let row = rows
            .iter()
            .find(|row| row.host_id == host)
            .ok_or(UploadRepositoryError::Missing)?;
        if revision != 0 && row.revision != revision {
            return Err(UploadRepositoryError::Conflict);
        }
        let _lease = self
            .budget
            .try_acquire_frozen(id, &snapshot.targets, snapshot.definition.concurrency, host)
            .map_err(|_| UploadRepositoryError::Corrupt)?
            .ok_or(UploadRepositoryError::Conflict)?;
        let mut remote = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(UploadRepositoryError::Unavailable),
            result = tokio::time::timeout(
            Duration::from_millis(snapshot.definition.connect_timeout_ms),
            self.executor.connect(&snapshot, host, cancel.clone()),
        )
        => result,
        }
        .map_err(|_| UploadRepositoryError::Unavailable)?
        .map_err(|_| UploadRepositoryError::Unavailable)?;
        let temporary = snapshot
            .temporary(host)
            .map_err(|_| UploadRepositoryError::Corrupt)?;
        let result = tokio::select! { biased;
            _ = cancel.cancelled() => Err(UploadRepositoryError::Unavailable),
            result = tokio::time::timeout(
            Duration::from_millis(snapshot.definition.transfer_timeout_ms),
            remote.probe(&temporary, &snapshot.definition.remote_destination),
        )
        => result.map_err(|_| UploadRepositoryError::Unavailable).and_then(|value| value.map_err(|_| UploadRepositoryError::Unavailable)),
        };
        let _closed = tokio::time::timeout(Duration::from_secs(3), remote.close()).await;
        result
    }
    /// Adoption and cleanup are explicit and never replay a publication request.
    pub async fn resolve(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        confirmed: bool,
        adopt: bool,
    ) -> Result<(), UploadRepositoryError> {
        self.resolve_controlled(
            id,
            host,
            revision,
            confirmed,
            adopt,
            TaskCancellation::detached(),
        )
        .await
    }
    pub async fn resolve_controlled(
        &self,
        id: TaskRunId,
        host: HostId,
        revision: u64,
        confirmed: bool,
        adopt: bool,
        cancel: TaskCancellation,
    ) -> Result<(), UploadRepositoryError> {
        if !confirmed {
            return Err(UploadRepositoryError::Conflict);
        }
        let _owner = self
            .budget
            .claim_run(id)
            .map_err(|_| UploadRepositoryError::Conflict)?;
        let (snapshot, rows) = self.load(id).await?;
        let mut row = rows
            .into_iter()
            .find(|row| row.host_id == host)
            .ok_or(UploadRepositoryError::Missing)?;
        if row.revision != revision || !row.temporary_owned || row.temporary_cleaned {
            return Err(UploadRepositoryError::Conflict);
        }
        if adopt {
            if row.phase != UploadPhase::Unknown
                || row.intent != UploadIntent::Publish
                || !row.verified
            {
                return Err(UploadRepositoryError::Conflict);
            }
        } else if !matches!(
            row.phase,
            UploadPhase::Failed
                | UploadPhase::Cancelled
                | UploadPhase::CleanupRequired
                | UploadPhase::Committed
                | UploadPhase::Unknown
        ) || row.phase == UploadPhase::Unknown && row.intent == UploadIntent::Publish
        {
            return Err(UploadRepositoryError::Conflict);
        }
        let _lease = self
            .budget
            .try_acquire_frozen(id, &snapshot.targets, snapshot.definition.concurrency, host)
            .map_err(|_| UploadRepositoryError::Corrupt)?
            .ok_or(UploadRepositoryError::Conflict)?;
        let mut remote = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(UploadRepositoryError::Unavailable),
            result = tokio::time::timeout(
            Duration::from_millis(snapshot.definition.connect_timeout_ms),
            self.executor
                .connect(&snapshot, host, cancel.clone()),
        )
        => result,
        }
        .map_err(|_| UploadRepositoryError::Unavailable)?
        .map_err(|_| UploadRepositoryError::Unavailable)?;
        let result = tokio::select! { biased;
            _ = cancel.cancelled() => Err(UploadRepositoryError::Unavailable),
            result = self.resolve_remote(&snapshot, &mut row, adopt, remote.as_mut()) => result,
        };
        let _closed = tokio::time::timeout(Duration::from_secs(3), remote.close()).await;
        result
    }
    async fn resolve_remote(
        &self,
        snapshot: &BatchUploadSnapshot,
        row: &mut UploadTarget,
        adopt: bool,
        remote: &mut dyn UploadRemote,
    ) -> Result<(), UploadRepositoryError> {
        let temporary = snapshot
            .temporary(row.host_id)
            .map_err(|_| UploadRepositoryError::Corrupt)?;
        if adopt {
            let probe = tokio::time::timeout(
                Duration::from_millis(snapshot.definition.transfer_timeout_ms),
                remote.probe(&temporary, &snapshot.definition.remote_destination),
            )
            .await
            .map_err(|_| UploadRepositoryError::Unavailable)?
            .map_err(|_| UploadRepositoryError::Unavailable)?;
            if probe.destination
                != Some(UploadContent {
                    size: snapshot.definition.size,
                    sha256: snapshot.definition.sha256,
                })
            {
                return Err(UploadRepositoryError::Conflict);
            }
            let publication = remote
                .recover_publication(
                    &snapshot
                        .backup(row.host_id)
                        .map_err(|_| UploadRepositoryError::Corrupt)?,
                    snapshot.definition.publication_policy,
                    row.publication,
                    UploadContent {
                        size: snapshot.definition.size,
                        sha256: snapshot.definition.sha256,
                    },
                )
                .await
                .map_err(|_| UploadRepositoryError::Conflict)?;
            self.checkpoint(snapshot, row, |next| {
                next.phase = UploadPhase::Committed;
                next.committed = true;
                next.adopted = true;
                next.publication = publication;
            })
            .await?;
        }
        if row.phase != UploadPhase::Committed && row.phase != UploadPhase::CleanupRequired {
            self.checkpoint(snapshot, row, |next| {
                next.phase = UploadPhase::CleanupRequired;
                next.intent = UploadIntent::Cleanup;
            })
            .await?;
        }
        match tokio::time::timeout(Duration::from_secs(30), remote.cleanup(&temporary)).await {
            Ok(Ok(())) => {
                self.checkpoint(snapshot, row, |next| {
                    next.phase = if next.committed {
                        UploadPhase::Succeeded
                    } else {
                        UploadPhase::Failed
                    };
                    next.temporary_cleaned = true;
                    next.intent = UploadIntent::Cleanup;
                })
                .await
            }
            _ => {
                if row.phase == UploadPhase::Committed {
                    self.checkpoint(snapshot, row, |next| {
                        next.phase = UploadPhase::CleanupRequired;
                        next.reason = Some(UploadReason::Cleanup);
                    })
                    .await?;
                }
                Err(UploadRepositoryError::Unavailable)
            }
        }
    }
}
