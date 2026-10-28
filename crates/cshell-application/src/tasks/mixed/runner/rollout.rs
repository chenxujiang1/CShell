use super::*;
pub(crate) fn failure_count(rows: &[MixedTargetRun]) -> usize {
    rows.iter()
        .filter(|row| {
            matches!(
                row.state,
                MixedTaskState::Failed | MixedTaskState::Unknown | MixedTaskState::TimedOut
            )
        })
        .count()
}
fn stopped(reason: TaskStopReason) -> MixedRolloutStop {
    match reason {
        TaskStopReason::TotalTimeout => MixedRolloutStop::TotalTimeout,
        TaskStopReason::FailureThreshold => MixedRolloutStop::FailureThreshold,
        TaskStopReason::OutputFailure | TaskStopReason::TransportLost => {
            MixedRolloutStop::StorageFailure
        }
        _ => MixedRolloutStop::UserCancelled,
    }
}
impl MixedTaskRunner {
    async fn stop_rollout_pending(
        &self,
        snapshot: &MixedTaskSnapshot,
        reason: MixedRolloutStop,
    ) -> Result<(), TaskRepositoryError> {
        let (_, rows) = self
            .repository
            .load_mixed_task(snapshot.id())
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        let target_reason = match reason {
            MixedRolloutStop::TotalTimeout => MixedStopReason::TotalTimeout,
            MixedRolloutStop::FailureThreshold => MixedStopReason::FailureThreshold,
            MixedRolloutStop::CanaryFailed => MixedStopReason::CanaryFailed,
            MixedRolloutStop::StorageFailure => MixedStopReason::StorageFailure,
            _ => MixedStopReason::UserCancelled,
        };
        for row in rows {
            if row.state == MixedTaskState::Pending {
                self.commit(snapshot, &row, MixedTargetAction::Stop(target_reason))
                    .await?;
            }
        }
        self.rollout_action(snapshot, MixedRolloutAction::Stop(reason), None)
            .await?;
        Ok(())
    }
    pub async fn rollout_action(
        &self,
        snapshot: &MixedTaskSnapshot,
        action: MixedRolloutAction,
        expected: Option<u64>,
    ) -> Result<MixedRolloutProgress, TaskRepositoryError> {
        let row = self
            .load(snapshot, snapshot.targets().targets()[0].id)
            .await?;
        if let MixedRolloutAction::Approve { gate } = &action {
            let progress = row.rollout.as_ref().ok_or(TaskRepositoryError::Corrupt)?;
            let now = self
                .clock
                .now_ms()
                .map_err(|_| TaskRepositoryError::Unavailable)?;
            if progress.stage != MixedRolloutStage::Paused
                || progress
                    .gate
                    .as_ref()
                    .is_none_or(|current| current.id != *gate)
                || progress.deadline_ms.is_some_and(|deadline| now >= deadline)
            {
                return Err(TaskRepositoryError::Conflict);
            }
        }
        if expected.is_some_and(|revision| {
            row.rollout
                .as_ref()
                .is_none_or(|progress| progress.revision != revision)
        }) {
            return Err(TaskRepositoryError::Conflict);
        }
        let row = self
            .commit(snapshot, &row, MixedTargetAction::Rollout(action))
            .await?;
        row.rollout.ok_or(TaskRepositoryError::Corrupt)
    }
    pub(super) async fn run_rollout(
        &self,
        snapshot: MixedTaskSnapshot,
        cancel: TaskCancellation,
    ) -> Result<(), TaskRepositoryError> {
        let policy = snapshot
            .definition()
            .policy
            .rollout
            .as_ref()
            .ok_or(TaskRepositoryError::Corrupt)?
            .clone();
        let batches = mixed_rollout_batches(&snapshot);
        let (_, rows) = self
            .repository
            .load_mixed_task(snapshot.id())
            .await?
            .ok_or(TaskRepositoryError::Missing)?;
        let current = rows[0]
            .rollout
            .as_ref()
            .ok_or(TaskRepositoryError::Corrupt)?;
        if policy
            .failure_limit
            .is_some_and(|limit| failure_count(&rows) >= usize::from(limit))
        {
            return Err(TaskRepositoryError::Conflict);
        }
        let pending_batch = batches
            .iter()
            .position(|batch| {
                rows.iter()
                    .any(|row| row.state == MixedTaskState::Pending && batch.contains(&row.host_id))
            })
            .ok_or(TaskRepositoryError::Conflict)?;
        let first = pending_batch.min(current.next_batch);
        let mut progress = self
            .rollout_action(
                &snapshot,
                MixedRolloutAction::Start { batch: first },
                Some(current.revision),
            )
            .await?;
        while progress.next_batch < batches.len() {
            let now = self
                .clock
                .now_ms()
                .map_err(|_| TaskRepositoryError::Unavailable)?;
            let deadline = progress.deadline_ms.ok_or(TaskRepositoryError::Corrupt)?;
            if now >= deadline {
                cancel.cancel(TaskStopReason::TotalTimeout);
            }
            if let Some(reason) = cancel.reason() {
                self.stop_rollout_pending(&snapshot, stopped(reason))
                    .await?;
                return Ok(());
            }
            if progress.not_before_ms > now {
                tokio::select! {biased;_ = cancel.cancelled()=>{},_ = tokio::time::sleep(Duration::from_millis((progress.not_before_ms-now).min(deadline-now)))=>{}}
                continue;
            }
            let batch_index = progress.next_batch;
            let batch = &batches[batch_index];
            let (_, rows) = self
                .repository
                .load_mixed_task(snapshot.id())
                .await?
                .ok_or(TaskRepositoryError::Missing)?;
            if policy
                .failure_limit
                .is_some_and(|limit| failure_count(&rows) >= usize::from(limit))
            {
                cancel.cancel(TaskStopReason::FailureThreshold);
                continue;
            }
            let mut pending = rows
                .into_iter()
                .filter(|row| row.state == MixedTaskState::Pending && batch.contains(&row.host_id));
            let queued_at = tokio::time::Instant::now();
            let mut jobs = tokio::task::JoinSet::new();
            let timer = tokio::time::sleep(Duration::from_millis(deadline - now));
            tokio::pin!(timer);
            let mut error = None;
            loop {
                while cancel.reason().is_none()
                    && jobs.len() < usize::from(snapshot.definition().policy.concurrency)
                {
                    let now = match self.clock.now_ms() {
                        Ok(now) => now,
                        Err(_) => {
                            error = Some(TaskRepositoryError::Unavailable);
                            cancel.cancel(TaskStopReason::OutputFailure);
                            break;
                        }
                    };
                    if now >= deadline {
                        cancel.cancel(TaskStopReason::TotalTimeout);
                        break;
                    }
                    let Some(row) = pending.next() else {
                        break;
                    };
                    let runner = self.clone();
                    let snapshot = snapshot.clone();
                    let cancel = cancel.clone();
                    jobs.spawn(
                        async move { runner.target(snapshot, row, cancel, queued_at).await },
                    );
                }
                if jobs.is_empty() {
                    break;
                }
                let result = tokio::select! {biased;_=&mut timer,if cancel.reason().is_none()=>{cancel.cancel(TaskStopReason::TotalTimeout);continue;},result=jobs.join_next()=>result};
                match result {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(failure))) => {
                        error = Some(failure);
                        cancel.cancel(TaskStopReason::OutputFailure);
                    }
                    Some(Err(_)) => {
                        error = Some(TaskRepositoryError::Unavailable);
                        cancel.cancel(TaskStopReason::OutputFailure);
                    }
                    None => break,
                }
                if error.is_none() {
                    match self.repository.load_mixed_task(snapshot.id()).await {
                        Ok(Some((_, rows))) => {
                            if policy
                                .failure_limit
                                .is_some_and(|limit| failure_count(&rows) >= usize::from(limit))
                            {
                                cancel.cancel(TaskStopReason::FailureThreshold);
                            }
                        }
                        Ok(None) => {
                            error = Some(TaskRepositoryError::Missing);
                            cancel.cancel(TaskStopReason::OutputFailure);
                        }
                        Err(failure) => {
                            error = Some(failure);
                            cancel.cancel(TaskStopReason::OutputFailure);
                        }
                    }
                }
            }
            if let Some(error) = error {
                let _stopped = self
                    .rollout_action(
                        &snapshot,
                        MixedRolloutAction::Stop(MixedRolloutStop::StorageFailure),
                        None,
                    )
                    .await;
                return Err(error);
            }
            if let Some(reason) = cancel.reason() {
                self.stop_rollout_pending(&snapshot, stopped(reason))
                    .await?;
                return Ok(());
            }
            let (_, rows) = self
                .repository
                .load_mixed_task(snapshot.id())
                .await?
                .ok_or(TaskRepositoryError::Missing)?;
            if batch_index == 0
                && policy.canary_count != 0
                && rows
                    .iter()
                    .filter(|row| batch.contains(&row.host_id))
                    .any(|row| row.state != MixedTaskState::Succeeded)
            {
                self.stop_rollout_pending(&snapshot, MixedRolloutStop::CanaryFailed)
                    .await?;
                return Ok(());
            }
            progress = self
                .rollout_action(
                    &snapshot,
                    MixedRolloutAction::Advance { batch: batch_index },
                    None,
                )
                .await?;
            if progress.stage == MixedRolloutStage::Paused {
                return Ok(());
            }
        }
        Ok(())
    }
}
