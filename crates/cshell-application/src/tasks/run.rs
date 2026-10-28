use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStopReason {
    UserCancelled,
    ConnectionFailure,
    ConnectionTimeout,
    QueueTimeout,
    StepTimeout,
    TotalTimeout,
    TransportLost,
    OutputFailure,
    ExecRejected,
    DaemonRestarted,
    FailureThreshold,
    CanaryFailed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGateKind {
    Canary,
    BetweenBatches,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGate {
    pub id: TaskGateId,
    pub kind: TaskGateKind,
    pub next_batch: usize,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRunMeta {
    pub revision: u64,
    pub state: TaskState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub next_batch: usize,
    #[serde(default)]
    pub batch_not_before_ms: u64,
    pub gate: Option<TaskGate>,
    pub stop_reason: Option<TaskStopReason>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStepRun {
    pub state: TargetState,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub exit_code: Option<u32>,
    pub reason: Option<TaskStopReason>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTargetRun {
    pub host_id: HostId,
    pub state: TargetState,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub reason: Option<TaskStopReason>,
    pub steps: Vec<TaskStepRun>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRunRecord {
    snapshot: TaskSnapshot,
    meta: TaskRunMeta,
    targets: Vec<TaskTargetRun>,
}
#[must_use]
pub const fn target_terminal(state: TargetState) -> bool {
    matches!(
        state,
        TargetState::Succeeded
            | TargetState::Failed
            | TargetState::TimedOut
            | TargetState::Cancelled
            | TargetState::Unknown
    )
}
#[must_use]
pub const fn task_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::PartialFailed | TaskState::Failed | TaskState::Cancelled
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskRunAction {
    Start,
    Connect {
        host: HostId,
    },
    /// Commit this intent before calling SSH exec. A crash afterwards is Unknown.
    StepIntent {
        host: HostId,
        step: usize,
    },
    StepExited {
        host: HostId,
        step: usize,
        exit_code: u32,
    },
    AbortTarget {
        host: HostId,
        reason: TaskStopReason,
    },
    AdvanceBatch,
    ApproveGate(TaskGateId),
    Stop(TaskStopReason),
    Recover,
    Finish,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunEventKind {
    Created,
    Started,
    Connecting,
    StepIntent,
    StepExited,
    TargetStopped,
    BatchFinished,
    GateApproved,
    StopRequested,
    Recovered,
    Finished,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRunEvent {
    pub revision: u64,
    pub at_ms: u64,
    pub kind: TaskRunEventKind,
    pub target: Option<HostId>,
    pub step: Option<usize>,
    pub affected_targets: usize,
    pub reason: Option<TaskStopReason>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTargetChange {
    before: TaskTargetRun,
    after: TaskTargetRun,
}
impl TaskTargetChange {
    #[must_use]
    pub fn before(&self) -> &TaskTargetRun {
        &self.before
    }
    #[must_use]
    pub fn after(&self) -> &TaskTargetRun {
        &self.after
    }
}
/// Opaque patch. Only validated transitions can construct one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRunPatch {
    id: TaskRunId,
    fingerprint: [u8; 32],
    before: TaskRunMeta,
    after: TaskRunMeta,
    changes: Vec<TaskTargetChange>,
    event: TaskRunEvent,
}
impl TaskRunPatch {
    #[must_use]
    pub fn id(&self) -> TaskRunId {
        self.id
    }
    #[must_use]
    pub fn snapshot_fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
    #[must_use]
    pub fn before(&self) -> &TaskRunMeta {
        &self.before
    }
    #[must_use]
    pub fn after(&self) -> &TaskRunMeta {
        &self.after
    }
    #[must_use]
    pub fn changes(&self) -> &[TaskTargetChange] {
        &self.changes
    }
    #[must_use]
    pub fn event(&self) -> &TaskRunEvent {
        &self.event
    }
}
fn pending_step() -> TaskStepRun {
    TaskStepRun {
        state: TargetState::Pending,
        started_at_ms: None,
        finished_at_ms: None,
        exit_code: None,
        reason: None,
    }
}
fn cancelled_steps(target: &mut TaskTargetRun, now: u64, reason: TaskStopReason) {
    for step in &mut target.steps {
        if step.state == TargetState::Pending {
            step.state = TargetState::Cancelled;
            step.finished_at_ms = Some(now);
            step.reason = Some(reason);
        }
    }
}
fn final_state(targets: &[TaskTargetRun], reason: Option<TaskStopReason>) -> TaskState {
    if targets.iter().all(|t| t.state == TargetState::Succeeded) {
        TaskState::Completed
    } else if targets
        .iter()
        .any(|t| matches!(t.state, TargetState::Succeeded | TargetState::Unknown))
    {
        TaskState::PartialFailed
    } else if reason == Some(TaskStopReason::UserCancelled) {
        if targets.iter().all(|t| t.state == TargetState::Cancelled) {
            TaskState::Cancelled
        } else if targets.iter().any(|t| t.state == TargetState::Cancelled) {
            TaskState::PartialFailed
        } else {
            TaskState::Failed
        }
    } else {
        TaskState::Failed
    }
}
fn abort(target: &mut TaskTargetRun, reason: TaskStopReason, now: u64) -> Result<(), TaskError> {
    if target_terminal(target.state) {
        return Err(TaskError::Transition);
    }
    let active = target
        .steps
        .iter()
        .position(|step| step.state == TargetState::Running);
    if matches!(
        reason,
        TaskStopReason::OutputFailure | TaskStopReason::ExecRejected
    ) && active.is_none()
        || reason == TaskStopReason::StepTimeout && target.state != TargetState::Running
        || matches!(
            reason,
            TaskStopReason::ConnectionFailure | TaskStopReason::ConnectionTimeout
        ) && target.state != TargetState::Connecting
        || reason == TaskStopReason::QueueTimeout && target.state != TargetState::Pending
    {
        return Err(TaskError::Transition);
    }
    target.state = if reason == TaskStopReason::DaemonRestarted
        || active.is_some() && reason != TaskStopReason::ExecRejected
    {
        TargetState::Unknown
    } else {
        match reason {
            TaskStopReason::QueueTimeout
            | TaskStopReason::ConnectionTimeout
            | TaskStopReason::StepTimeout
            | TaskStopReason::TotalTimeout => TargetState::TimedOut,
            TaskStopReason::UserCancelled
            | TaskStopReason::FailureThreshold
            | TaskStopReason::CanaryFailed => TargetState::Cancelled,
            _ => TargetState::Failed,
        }
    };
    if let Some(index) = active {
        let step = &mut target.steps[index];
        step.state = target.state;
        step.reason = Some(reason);
        step.finished_at_ms = Some(now);
    }
    target.reason = Some(reason);
    target.finished_at_ms = Some(now);
    cancelled_steps(target, now, reason);
    Ok(())
}
impl TaskRunRecord {
    pub fn new(snapshot: TaskSnapshot) -> Result<Self, TaskError> {
        let targets = snapshot
            .targets()
            .targets()
            .iter()
            .map(|host| TaskTargetRun {
                host_id: host.id,
                state: TargetState::Pending,
                started_at_ms: None,
                finished_at_ms: None,
                reason: None,
                steps: vec![pending_step(); snapshot.definition().steps.len()],
            })
            .collect();
        let meta = TaskRunMeta {
            revision: 1,
            state: TaskState::Queued,
            created_at_ms: snapshot.created_at_ms(),
            updated_at_ms: snapshot.created_at_ms(),
            next_batch: 0,
            batch_not_before_ms: 0,
            gate: None,
            stop_reason: None,
        };
        Self::restore(snapshot, meta, targets)
    }
    pub fn restore(
        snapshot: TaskSnapshot,
        meta: TaskRunMeta,
        targets: Vec<TaskTargetRun>,
    ) -> Result<Self, TaskError> {
        let record = Self {
            snapshot,
            meta,
            targets,
        };
        record.validate()?;
        Ok(record)
    }
    #[must_use]
    pub fn id(&self) -> TaskRunId {
        self.snapshot.id()
    }
    #[must_use]
    pub fn snapshot(&self) -> &TaskSnapshot {
        &self.snapshot
    }
    #[must_use]
    pub fn meta(&self) -> &TaskRunMeta {
        &self.meta
    }
    #[must_use]
    pub fn targets(&self) -> &[TaskTargetRun] {
        &self.targets
    }
    #[must_use]
    pub fn target(&self, host: HostId) -> Option<&TaskTargetRun> {
        self.targets
            .binary_search_by_key(&host, |target| target.host_id)
            .ok()
            .map(|index| &self.targets[index])
    }
    pub fn created_event(&self) -> TaskRunEvent {
        TaskRunEvent {
            revision: 1,
            at_ms: self.meta.created_at_ms,
            kind: TaskRunEventKind::Created,
            target: None,
            step: None,
            affected_targets: self.targets.len(),
            reason: None,
        }
    }
    pub fn prepare(&self, action: TaskRunAction, now: u64) -> Result<TaskRunPatch, TaskError> {
        if now < self.meta.updated_at_ms || now > i64::MAX as u64 {
            return Err(TaskError::Clock);
        }
        if task_terminal(self.meta.state) {
            return Err(TaskError::Transition);
        }
        let mut next = self.clone();
        let mut target_id = None;
        let mut step_index = None;
        let mut reason = None;
        let kind = match action {
            TaskRunAction::Start if self.meta.state == TaskState::Queued => {
                next.meta.state = TaskState::Running;
                TaskRunEventKind::Started
            }
            TaskRunAction::Connect { host } if self.meta.state == TaskState::Running => {
                if self
                    .targets
                    .iter()
                    .filter(|t| matches!(t.state, TargetState::Connecting | TargetState::Running))
                    .count()
                    >= usize::from(self.snapshot.definition().policy.concurrency)
                {
                    return Err(TaskError::Transition);
                }
                if !self
                    .snapshot
                    .batches()
                    .get(self.meta.next_batch)
                    .is_some_and(|ids| ids.contains(&host))
                {
                    return Err(TaskError::Transition);
                }
                if self
                    .snapshot
                    .definition()
                    .policy
                    .failure_limit
                    .is_some_and(|limit| {
                        self.targets
                            .iter()
                            .filter(|t| {
                                matches!(
                                    t.state,
                                    TargetState::Failed
                                        | TargetState::TimedOut
                                        | TargetState::Unknown
                                )
                            })
                            .count()
                            >= usize::from(limit)
                    })
                {
                    return Err(TaskError::Transition);
                }
                let target = next.target_mut(host)?;
                if target.state != TargetState::Pending {
                    return Err(TaskError::Transition);
                }
                target.state = TargetState::Connecting;
                target.started_at_ms = Some(now);
                target_id = Some(host);
                TaskRunEventKind::Connecting
            }
            TaskRunAction::StepIntent { host, step } if self.meta.state == TaskState::Running => {
                let target = next.target_mut(host)?;
                if !matches!(target.state, TargetState::Connecting | TargetState::Running)
                    || target
                        .steps
                        .get(step)
                        .is_none_or(|s| s.state != TargetState::Pending)
                    || target.steps[..step]
                        .iter()
                        .any(|s| s.state != TargetState::Succeeded)
                {
                    return Err(TaskError::Transition);
                }
                target.steps[step].state = TargetState::Running;
                target.steps[step].started_at_ms = Some(now);
                target.state = TargetState::Running;
                target_id = Some(host);
                step_index = Some(step);
                TaskRunEventKind::StepIntent
            }
            TaskRunAction::StepExited {
                host,
                step,
                exit_code,
            } if matches!(self.meta.state, TaskState::Running | TaskState::Cancelling) => {
                let success = self
                    .snapshot
                    .definition()
                    .steps
                    .get(step)
                    .ok_or(TaskError::Transition)?
                    .success_codes
                    .contains(&exit_code);
                let target = next.target_mut(host)?;
                if target.state != TargetState::Running
                    || target
                        .steps
                        .get(step)
                        .is_none_or(|s| s.state != TargetState::Running)
                {
                    return Err(TaskError::Transition);
                }
                let completed = &mut target.steps[step];
                completed.state = if success {
                    TargetState::Succeeded
                } else {
                    TargetState::Failed
                };
                completed.exit_code = Some(exit_code);
                completed.finished_at_ms = Some(now);
                if !success {
                    target.state = TargetState::Failed;
                    target.finished_at_ms = Some(now);
                    cancelled_steps(target, now, TaskStopReason::FailureThreshold);
                } else if step + 1 == target.steps.len() {
                    target.state = TargetState::Succeeded;
                    target.finished_at_ms = Some(now);
                }
                target_id = Some(host);
                step_index = Some(step);
                TaskRunEventKind::StepExited
            }
            TaskRunAction::AbortTarget { host, reason: why } => {
                abort(next.target_mut(host)?, why, now)?;
                target_id = Some(host);
                reason = Some(why);
                TaskRunEventKind::TargetStopped
            }
            TaskRunAction::AdvanceBatch if self.meta.state == TaskState::Running => {
                let batches = self.snapshot.batches();
                let batch = batches
                    .get(self.meta.next_batch)
                    .ok_or(TaskError::Transition)?;
                if batch
                    .iter()
                    .any(|id| self.target(*id).is_none_or(|t| !target_terminal(t.state)))
                {
                    return Err(TaskError::Transition);
                }
                let policy = &self.snapshot.definition().policy;
                if self.meta.next_batch == 0
                    && policy.canary_count > 0
                    && batch.iter().any(|id| {
                        self.target(*id)
                            .is_none_or(|t| t.state != TargetState::Succeeded)
                    })
                {
                    next.stop(TaskStopReason::CanaryFailed, now)?;
                    reason = Some(TaskStopReason::CanaryFailed);
                } else {
                    next.meta.next_batch += 1;
                    next.meta.batch_not_before_ms = now
                        .checked_add(policy.batch_delay_ms)
                        .filter(|v| *v <= i64::MAX as u64)
                        .ok_or(TaskError::Clock)?;
                    if next.meta.next_batch < batches.len()
                        && (self.meta.next_batch == 0 && policy.approve_after_canary
                            || policy.pause_between_batches)
                    {
                        next.meta.state = TaskState::Paused;
                        next.meta.gate = Some(TaskGate {
                            id: TaskGateId::new(),
                            kind: if self.meta.next_batch == 0 && policy.approve_after_canary {
                                TaskGateKind::Canary
                            } else {
                                TaskGateKind::BetweenBatches
                            },
                            next_batch: next.meta.next_batch,
                        });
                    }
                }
                TaskRunEventKind::BatchFinished
            }
            TaskRunAction::ApproveGate(id)
                if self.meta.state == TaskState::Paused
                    && self.meta.gate.as_ref().is_some_and(|gate| gate.id == id) =>
            {
                next.meta.state = TaskState::Running;
                next.meta.gate = None;
                next.meta.batch_not_before_ms =
                    if next.meta.batch_not_before_ms == 0 && next.meta.next_batch > 0 {
                        now.checked_add(self.snapshot.definition().policy.batch_delay_ms)
                            .filter(|v| *v <= i64::MAX as u64)
                            .ok_or(TaskError::Clock)?
                    } else {
                        next.meta.batch_not_before_ms.max(now)
                    };
                TaskRunEventKind::GateApproved
            }
            TaskRunAction::Stop(why)
                if matches!(
                    why,
                    TaskStopReason::UserCancelled
                        | TaskStopReason::TotalTimeout
                        | TaskStopReason::FailureThreshold
                        | TaskStopReason::CanaryFailed
                ) =>
            {
                next.stop(why, now)?;
                reason = Some(why);
                TaskRunEventKind::StopRequested
            }
            TaskRunAction::Recover => {
                for target in &mut next.targets {
                    if matches!(target.state, TargetState::Connecting | TargetState::Running) {
                        abort(target, TaskStopReason::DaemonRestarted, now)?;
                    }
                }
                if self.meta.state == TaskState::Running {
                    next.meta.state = TaskState::Queued;
                }
                reason = Some(TaskStopReason::DaemonRestarted);
                TaskRunEventKind::Recovered
            }
            TaskRunAction::Finish if self.targets.iter().all(|t| target_terminal(t.state)) => {
                next.meta.state = final_state(&self.targets, self.meta.stop_reason);
                next.meta.gate = None;
                TaskRunEventKind::Finished
            }
            _ => return Err(TaskError::Transition),
        };
        next.meta.revision = self
            .meta
            .revision
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or(TaskError::Limit)?;
        next.meta.updated_at_ms = now;
        next.validate()?;
        let changes = self
            .targets
            .iter()
            .zip(&next.targets)
            .filter(|(a, b)| a != b)
            .map(|(before, after)| TaskTargetChange {
                before: before.clone(),
                after: after.clone(),
            })
            .collect::<Vec<_>>();
        let event = TaskRunEvent {
            revision: next.meta.revision,
            at_ms: now,
            kind,
            target: target_id,
            step: step_index,
            affected_targets: changes.len(),
            reason,
        };
        Ok(TaskRunPatch {
            id: self.id(),
            fingerprint: *self.snapshot.fingerprint(),
            before: self.meta.clone(),
            after: next.meta,
            changes,
            event,
        })
    }
    fn target_mut(&mut self, host: HostId) -> Result<&mut TaskTargetRun, TaskError> {
        let index = self
            .targets
            .binary_search_by_key(&host, |target| target.host_id)
            .map_err(|_| TaskError::UnknownTarget)?;
        Ok(&mut self.targets[index])
    }
    fn stop(&mut self, why: TaskStopReason, now: u64) -> Result<(), TaskError> {
        self.meta.state = TaskState::Cancelling;
        self.meta.gate = None;
        self.meta.stop_reason = Some(why);
        for target in &mut self.targets {
            if target.state == TargetState::Pending {
                abort(target, why, now)?;
            }
        }
        Ok(())
    }
    /// Apply only after the repository acknowledges this exact revision.
    pub fn apply_committed(
        &mut self,
        patch: &TaskRunPatch,
        revision: u64,
    ) -> Result<(), TaskError> {
        if self.id() != patch.id
            || self.snapshot.fingerprint() != &patch.fingerprint
            || self.meta != patch.before
            || revision != patch.after.revision
        {
            return Err(TaskError::Transition);
        }
        for change in &patch.changes {
            if self.target(change.before.host_id) != Some(&change.before) {
                return Err(TaskError::Transition);
            }
        }
        for change in &patch.changes {
            *self.target_mut(change.before.host_id)? = change.after.clone();
        }
        self.meta = patch.after.clone();
        Ok(())
    }
    fn validate(&self) -> Result<(), TaskError> {
        let m = &self.meta;
        let batches = self.snapshot.batches();
        let policy = &self.snapshot.definition().policy;
        if m.revision == 0
            || m.revision > i64::MAX as u64
            || m.created_at_ms != self.snapshot.created_at_ms()
            || m.updated_at_ms < m.created_at_ms
            || m.updated_at_ms > i64::MAX as u64
            || m.batch_not_before_ms > i64::MAX as u64
            || m.next_batch > batches.len()
            || !matches!(
                m.state,
                TaskState::Queued
                    | TaskState::Running
                    | TaskState::Paused
                    | TaskState::Cancelling
                    | TaskState::Completed
                    | TaskState::PartialFailed
                    | TaskState::Failed
                    | TaskState::Cancelled
            )
            || (m.state == TaskState::Paused) != m.gate.is_some()
        {
            return Err(TaskError::Invalid);
        }
        if let Some(gate) = &m.gate
            && (gate.id.as_uuid().is_nil()
                || gate.next_batch != m.next_batch
                || m.next_batch == 0
                || m.next_batch == batches.len()
                || match gate.kind {
                    TaskGateKind::Canary => {
                        m.next_batch != 1
                            || !policy.approve_after_canary
                            || policy.canary_count == 0
                    }
                    TaskGateKind::BetweenBatches => {
                        !policy.pause_between_batches
                            || m.next_batch == 1 && policy.approve_after_canary
                    }
                })
        {
            return Err(TaskError::Invalid);
        }
        if self.targets.len() != self.snapshot.targets().targets().len() {
            return Err(TaskError::Invalid);
        }
        for (target, host) in self.targets.iter().zip(self.snapshot.targets().targets()) {
            if target.host_id != host.id
                || target.steps.len() != self.snapshot.definition().steps.len()
                || target_terminal(target.state) != target.finished_at_ms.is_some()
            {
                return Err(TaskError::Invalid);
            }
            for time in [target.started_at_ms, target.finished_at_ms]
                .into_iter()
                .flatten()
            {
                if time < m.created_at_ms || time > m.updated_at_ms {
                    return Err(TaskError::Invalid);
                }
            }
            if target
                .started_at_ms
                .zip(target.finished_at_ms)
                .is_some_and(|(a, b)| a > b)
            {
                return Err(TaskError::Invalid);
            }
            let mut suffix = false;
            let mut active = 0;
            for (step, definition) in target.steps.iter().zip(&self.snapshot.definition().steps) {
                if step.state == TargetState::Connecting
                    || target_terminal(step.state) != step.finished_at_ms.is_some()
                {
                    return Err(TaskError::Invalid);
                }
                for time in [step.started_at_ms, step.finished_at_ms]
                    .into_iter()
                    .flatten()
                {
                    if time < m.created_at_ms || time > m.updated_at_ms {
                        return Err(TaskError::Invalid);
                    }
                }
                if step
                    .started_at_ms
                    .zip(step.finished_at_ms)
                    .is_some_and(|(a, b)| a > b)
                    || step.started_at_ms.is_some_and(|start| {
                        target
                            .started_at_ms
                            .is_none_or(|target_start| start < target_start)
                    })
                    || matches!(
                        step.state,
                        TargetState::Succeeded
                            | TargetState::Failed
                            | TargetState::Unknown
                            | TargetState::Running
                    ) && step.started_at_ms.is_none()
                    || step.state == TargetState::Pending && step.started_at_ms.is_some()
                    || step.state != TargetState::Succeeded
                        && step.state != TargetState::Failed
                        && step.exit_code.is_some()
                    || step
                        .finished_at_ms
                        .zip(target.finished_at_ms)
                        .is_some_and(|(step_end, target_end)| step_end > target_end)
                {
                    return Err(TaskError::Invalid);
                }
                if suffix && !target_terminal(target.state) && step.state != TargetState::Pending {
                    return Err(TaskError::Invalid);
                }
                if target.state == TargetState::Running
                    && !matches!(
                        step.state,
                        TargetState::Succeeded | TargetState::Running | TargetState::Pending
                    )
                    || target_terminal(target.state)
                        && matches!(step.state, TargetState::Pending | TargetState::Running)
                {
                    return Err(TaskError::Invalid);
                }
                if step.state == TargetState::Succeeded {
                    if suffix
                        || step
                            .exit_code
                            .is_none_or(|code| !definition.success_codes.contains(&code))
                    {
                        return Err(TaskError::Invalid);
                    }
                } else {
                    suffix = true;
                }
                if step.state == TargetState::Failed
                    && step.exit_code.is_none()
                    && step.reason != Some(TaskStopReason::ExecRejected)
                {
                    return Err(TaskError::Invalid);
                }
                if step.state == TargetState::Failed
                    && step
                        .exit_code
                        .is_some_and(|code| definition.success_codes.contains(&code))
                {
                    return Err(TaskError::Invalid);
                }
                if step.state == TargetState::Running {
                    active += 1;
                    if step.started_at_ms.is_none() {
                        return Err(TaskError::Invalid);
                    }
                }
            }
            if active > 1
                || active > 0 && target.state != TargetState::Running
                || target.state == TargetState::Succeeded
                    && target
                        .steps
                        .iter()
                        .any(|s| s.state != TargetState::Succeeded)
                || matches!(target.state, TargetState::Pending | TargetState::Connecting)
                    && target.steps.iter().any(|s| s.state != TargetState::Pending)
                || target.state == TargetState::Pending && target.started_at_ms.is_some()
                || matches!(target.state, TargetState::Connecting | TargetState::Running)
                    && target.started_at_ms.is_none()
                || matches!(m.state, TaskState::Queued | TaskState::Paused)
                    && matches!(target.state, TargetState::Connecting | TargetState::Running)
                || task_terminal(m.state) && !target_terminal(target.state)
            {
                return Err(TaskError::Invalid);
            }
        }
        if self
            .targets
            .iter()
            .filter(|t| matches!(t.state, TargetState::Connecting | TargetState::Running))
            .count()
            > usize::from(policy.concurrency)
        {
            return Err(TaskError::Invalid);
        }
        for (index, batch) in batches.iter().enumerate() {
            for id in batch {
                let target = self.target(*id).ok_or(TaskError::Invalid)?;
                if index < m.next_batch && !target_terminal(target.state)
                    || index > m.next_batch && target.started_at_ms.is_some()
                    || m.gate
                        .as_ref()
                        .is_some_and(|gate| gate.kind == TaskGateKind::Canary)
                        && index == 0
                        && target.state != TargetState::Succeeded
                {
                    return Err(TaskError::Invalid);
                }
            }
        }
        if task_terminal(m.state) && m.state != final_state(&self.targets, m.stop_reason) {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}

pub fn encode_task_meta(meta: &TaskRunMeta) -> Result<Vec<u8>, TaskError> {
    encode(meta, MAX_TASK_META_BYTES)
}
pub fn decode_task_meta(bytes: &[u8]) -> Result<TaskRunMeta, TaskError> {
    decode(bytes, MAX_TASK_META_BYTES)
}
pub fn encode_task_target(target: &TaskTargetRun) -> Result<Vec<u8>, TaskError> {
    encode(target, MAX_TASK_TARGET_BYTES)
}
pub fn decode_task_target(bytes: &[u8]) -> Result<TaskTargetRun, TaskError> {
    let target: TaskTargetRun = decode(bytes, MAX_TASK_TARGET_BYTES)?;
    if target.steps.len() > MAX_TASK_STEPS {
        return Err(TaskError::Limit);
    }
    Ok(target)
}
pub fn encode_task_event(event: &TaskRunEvent) -> Result<Vec<u8>, TaskError> {
    encode(event, 4096)
}
#[must_use]
pub const fn task_state_code(state: TaskState) -> i64 {
    match state {
        TaskState::Queued => 0,
        TaskState::Running => 1,
        TaskState::Paused => 2,
        TaskState::Cancelling => 3,
        TaskState::Completed => 4,
        TaskState::PartialFailed => 5,
        TaskState::Failed => 6,
        TaskState::Cancelled => 7,
        _ => -1,
    }
}
#[must_use]
pub const fn task_target_code(state: TargetState) -> i64 {
    match state {
        TargetState::Pending => 0,
        TargetState::Connecting => 1,
        TargetState::Running => 2,
        TargetState::Succeeded => 3,
        TargetState::Failed => 4,
        TargetState::TimedOut => 5,
        TargetState::Cancelled => 6,
        TargetState::Unknown => 7,
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskRunSummary {
    pub id: TaskRunId,
    pub name: String,
    pub operator: String,
    pub meta: TaskRunMeta,
    pub targets: [u64; 8],
}

/// A status document intentionally omitting the frozen command and credentials.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRunView {
    pub id: TaskRunId,
    pub name: String,
    pub meta: TaskRunMeta,
    pub targets: Vec<TaskTargetRun>,
    #[serde(default)]
    pub runner_active: bool,
}

pub fn decode_task_view(bytes: &[u8]) -> Result<TaskRunView, TaskError> {
    let view: TaskRunView = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    let ids: BTreeSet<_> = view.targets.iter().map(|target| target.host_id).collect();
    if view.id.as_uuid().is_nil()
        || !label(&view.name)
        || view.meta.revision == 0
        || view.targets.is_empty()
        || view.targets.len() > crate::MAX_HOST_TARGETS
        || ids.len() != view.targets.len()
        || view.targets.iter().any(|target| {
            target.host_id.as_uuid().is_nil()
                || target.steps.is_empty()
                || target.steps.len() > MAX_TASK_STEPS
        })
    {
        return Err(TaskError::Invalid);
    }
    Ok(view)
}

pub fn decode_task_list(bytes: &[u8]) -> Result<Vec<TaskRunSummary>, TaskError> {
    let rows: Vec<TaskRunSummary> = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    if rows.len() > MAX_TASK_LIST_RESULTS
        || rows.iter().any(|row| {
            row.id.as_uuid().is_nil()
                || !label(&row.name)
                || row.meta.revision == 0
                || row
                    .targets
                    .iter()
                    .copied()
                    .try_fold(0_u64, u64::checked_add)
                    .is_none_or(|total| total == 0 || total > crate::MAX_HOST_TARGETS as u64)
        })
        || rows.windows(2).any(|pair| pair[0].id >= pair[1].id)
    {
        return Err(TaskError::Invalid);
    }
    Ok(rows)
}
