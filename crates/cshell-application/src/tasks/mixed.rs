//! Per-host ordered command/upload/download steps, preserving typed leaf receipts.
mod children;
mod control;
mod repair;
mod results;
mod retry;
mod rollout;
mod runner;
use super::*;
pub use control::*;
pub use repair::*;
pub use results::*;
pub use retry::*;
pub use rollout::*;
pub use runner::*;

pub const MAX_MIXED_TARGET_BYTES: usize = 128 * 1024;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MixedTaskAction {
    Command { definition: TaskDefinition },
    Upload { definition: BatchUploadDefinition },
    Download { definition: BatchDownloadDefinition },
}
impl MixedTaskAction {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Command { definition } => &definition.name,
            Self::Upload { definition } => &definition.name,
            Self::Download { definition } => &definition.name,
        }
    }
    fn validate(&self) -> Result<(), TaskError> {
        match self {
            Self::Command { definition } => {
                validate_task_definition(definition)?;
                let policy = &definition.policy;
                if definition.steps.len() != 1
                    || policy.concurrency != 1
                    || policy.batch_size.is_some()
                    || policy.canary_count != 0
                    || policy.approve_after_canary
                    || policy.pause_between_batches
                    || policy.batch_delay_ms != 0
                    || policy.failure_limit.is_some()
                {
                    return Err(TaskError::Invalid);
                }
            }
            Self::Upload { definition } => {
                definition.validate()?;
                if definition.concurrency != 1 {
                    return Err(TaskError::Invalid);
                }
            }
            Self::Download { definition } => {
                definition.validate()?;
                if definition.concurrency != 1 {
                    return Err(TaskError::Invalid);
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedTaskPolicy {
    pub concurrency: u16,
    pub queue_timeout_ms: u64,
    pub total_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<MixedRolloutPolicy>,
}
impl Default for MixedTaskPolicy {
    fn default() -> Self {
        Self {
            concurrency: 20,
            queue_timeout_ms: 300_000,
            total_timeout_ms: 3_600_000,
            rollout: None,
        }
    }
}
impl MixedTaskPolicy {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.concurrency == 0
            || self.concurrency > 50
            || [self.queue_timeout_ms, self.total_timeout_ms]
                .iter()
                .any(|value| *value == 0 || *value > MAX_TIMEOUT_MS)
        {
            return Err(TaskError::Invalid);
        }
        if let Some(policy) = &self.rollout {
            policy.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedTaskDefinition {
    pub name: String,
    pub steps: Vec<MixedTaskAction>,
    pub policy: MixedTaskPolicy,
}
impl fmt::Debug for MixedTaskDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixedTaskDefinition")
            .field("steps", &self.steps.len())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}
impl MixedTaskDefinition {
    pub fn validate(&self) -> Result<(), TaskError> {
        if !label(&self.name)
            || self.steps.is_empty()
            || self.steps.len() > MAX_TASK_STEPS
            || self.policy.concurrency == 0
            || self.policy.concurrency > 50
            || [self.policy.queue_timeout_ms, self.policy.total_timeout_ms]
                .iter()
                .any(|value| *value == 0 || *value > MAX_TIMEOUT_MS)
        {
            return Err(TaskError::Invalid);
        }
        for step in &self.steps {
            step.validate()?;
        }
        if let Some(policy) = &self.policy.rollout {
            policy.validate()?;
        }
        let mut uploads = BTreeSet::new();
        let mut downloads = BTreeSet::new();
        for step in &self.steps {
            match step {
                MixedTaskAction::Upload { definition } => {
                    if !uploads.insert(&definition.remote_destination) {
                        return Err(TaskError::Invalid);
                    }
                }
                MixedTaskAction::Download { definition } => {
                    // Host UUID prefix is fixed per target, so directory/basename must be distinct.
                    if !downloads.insert((
                        std::path::PathBuf::from(&definition.local_directory),
                        definition.local_name.to_ascii_lowercase(),
                    )) {
                        return Err(TaskError::Invalid);
                    }
                }
                _ => {}
            }
        }
        encode(self, 1024 * 1024)?;
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MixedData {
    format: String,
    version: u32,
    id: TaskRunId,
    operator: String,
    created_at_ms: u64,
    definition: MixedTaskDefinition,
    targets: FrozenHostTargets,
    children: Vec<Vec<TaskRunId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry: Option<MixedRetryProvenance>,
}
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MixedTaskSnapshot {
    data: Arc<MixedData>,
    #[serde(skip)]
    fingerprint: [u8; 32],
}
impl fmt::Debug for MixedTaskSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixedTaskSnapshot")
            .field("id", &self.id())
            .field("targets", &self.targets().targets().len())
            .field("steps", &self.definition().steps.len())
            .finish()
    }
}
impl<'de> Deserialize<'de> for MixedTaskSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let data = MixedData::deserialize(deserializer)?;
        validate_mixed_data(&data).map_err(serde::de::Error::custom)?;
        let fingerprint = Sha256::digest(
            encode(&data, MAX_TASK_SNAPSHOT_BYTES).map_err(serde::de::Error::custom)?,
        )
        .into();
        Ok(Self {
            data: Arc::new(data),
            fingerprint,
        })
    }
}
fn validate_mixed_data(data: &MixedData) -> Result<(), TaskError> {
    data.definition.validate()?;
    if data.format != "cshell.mixed-task"
        || !matches!((data.version, data.retry.is_some()), (1, false) | (2, true))
        || data.id.as_uuid().is_nil()
        || !label(&data.operator)
        || data.created_at_ms > i64::MAX as u64 - MAX_TIMEOUT_MS
        || data.targets.targets().is_empty()
        || data.targets.targets().len() > crate::MAX_HOST_TARGETS
        || data.children.len() != data.targets.targets().len()
        || data
            .children
            .iter()
            .any(|row| row.len() != data.definition.steps.len())
    {
        return Err(TaskError::Invalid);
    }
    if let Some(proof) = &data.retry {
        proof.validate_shape(data)?;
    }
    let mut ids = BTreeSet::from([data.id]);
    if data
        .children
        .iter()
        .flatten()
        .any(|id| id.as_uuid().is_nil() || !ids.insert(*id))
    {
        return Err(TaskError::Invalid);
    }
    encode(data, MAX_TASK_SNAPSHOT_BYTES)?;
    Ok(())
}
impl MixedTaskSnapshot {
    pub fn new(
        definition: MixedTaskDefinition,
        targets: FrozenHostTargets,
        operator: String,
        now: u64,
    ) -> Result<Self, TaskError> {
        definition.validate()?;
        let children = targets
            .targets()
            .iter()
            .map(|_| definition.steps.iter().map(|_| TaskRunId::new()).collect())
            .collect();
        let data = MixedData {
            format: "cshell.mixed-task".into(),
            version: 1,
            id: TaskRunId::new(),
            operator,
            created_at_ms: now,
            definition,
            targets,
            children,
            retry: None,
        };
        validate_mixed_data(&data)?;
        let fingerprint = Sha256::digest(encode(&data, MAX_TASK_SNAPSHOT_BYTES)?).into();
        Ok(Self {
            data: Arc::new(data),
            fingerprint,
        })
    }
    #[must_use]
    pub fn id(&self) -> TaskRunId {
        self.data.id
    }
    #[must_use]
    pub fn definition(&self) -> &MixedTaskDefinition {
        &self.data.definition
    }
    #[must_use]
    pub fn targets(&self) -> &FrozenHostTargets {
        &self.data.targets
    }
    #[must_use]
    pub fn created_at_ms(&self) -> u64 {
        self.data.created_at_ms
    }
    #[must_use]
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
    pub fn child(&self, host: HostId, step: usize) -> Result<MixedChildSnapshot, TaskError> {
        let index = self
            .targets()
            .targets()
            .binary_search_by_key(&host, |host| host.id)
            .map_err(|_| TaskError::UnknownTarget)?;
        let action = self
            .definition()
            .steps
            .get(step)
            .ok_or(TaskError::Invalid)?;
        let id = self.data.children[index][step];
        let targets = self
            .targets()
            .subset(&BTreeSet::from([host]))
            .map_err(|_| TaskError::Invalid)?;
        match action {
            MixedTaskAction::Command { definition } => {
                Ok(MixedChildSnapshot::Command(TaskSnapshot::with_fixed_id(
                    definition.clone(),
                    targets,
                    self.data.operator.clone(),
                    self.created_at_ms(),
                    id,
                )?))
            }
            MixedTaskAction::Upload { definition } => {
                let child = BatchUploadSnapshot {
                    format: "cshell.batch-upload".into(),
                    version: 1,
                    id,
                    definition: definition.clone(),
                    targets,
                    created_at_ms: self.created_at_ms(),
                };
                child.validate()?;
                Ok(MixedChildSnapshot::Upload(child))
            }
            MixedTaskAction::Download { definition } => {
                let child = BatchDownloadSnapshot {
                    format: "cshell.batch-download".into(),
                    version: 1,
                    id,
                    definition: definition.clone(),
                    targets,
                    created_at_ms: self.created_at_ms(),
                };
                child.validate()?;
                Ok(MixedChildSnapshot::Download(child))
            }
        }
    }
}
#[derive(Clone, Debug)]
pub enum MixedChildSnapshot {
    Command(TaskSnapshot),
    Upload(BatchUploadSnapshot),
    Download(BatchDownloadSnapshot),
}
impl MixedChildSnapshot {
    #[must_use]
    pub fn id(&self) -> TaskRunId {
        match self {
            Self::Command(snapshot) => snapshot.id(),
            Self::Upload(snapshot) => snapshot.id,
            Self::Download(snapshot) => snapshot.id,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedTaskState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
    TimedOut,
}
impl MixedTaskState {
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedStopReason {
    UserCancelled,
    QueueTimeout,
    TotalTimeout,
    ChildFailed,
    ChildUnknown,
    Restarted,
    StorageFailure,
    FailureThreshold,
    CanaryFailed,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MixedReceipt {
    Command {
        meta: TaskRunMeta,
        target: TaskTargetRun,
    },
    Upload {
        row: UploadTarget,
    },
    Download {
        row: DownloadTarget,
    },
}
impl MixedReceipt {
    fn initial(child: &MixedChildSnapshot, host: HostId) -> Result<Self, TaskError> {
        match child {
            MixedChildSnapshot::Command(snapshot) => {
                let record = TaskRunRecord::new(snapshot.clone())?;
                Ok(Self::Command {
                    meta: record.meta().clone(),
                    target: record.targets()[0].clone(),
                })
            }
            MixedChildSnapshot::Upload(snapshot) => Ok(Self::Upload {
                row: UploadTarget::initial(snapshot, host),
            }),
            MixedChildSnapshot::Download(snapshot) => Ok(Self::Download {
                row: DownloadTarget::initial(snapshot, host),
            }),
        }
    }
    fn validate(&self, child: &MixedChildSnapshot) -> Result<(), TaskError> {
        match (self, child) {
            (Self::Command { meta, target }, MixedChildSnapshot::Command(snapshot)) => {
                TaskRunRecord::restore(snapshot.clone(), meta.clone(), vec![target.clone()])?;
            }
            (Self::Upload { row }, MixedChildSnapshot::Upload(snapshot)) => {
                decode_upload_target(&encode_upload_target(row)?, snapshot)?;
            }
            (Self::Download { row }, MixedChildSnapshot::Download(snapshot)) => {
                row.validate(snapshot)?;
            }
            _ => return Err(TaskError::Invalid),
        }
        Ok(())
    }
    #[must_use]
    pub fn outcome(&self) -> MixedTaskState {
        match self {
            Self::Command { meta, target } => {
                if !task_terminal(meta.state) {
                    if target.state == TargetState::Pending {
                        MixedTaskState::Pending
                    } else {
                        MixedTaskState::Running
                    }
                } else {
                    match target.state {
                        TargetState::Succeeded => MixedTaskState::Succeeded,
                        TargetState::Unknown => MixedTaskState::Unknown,
                        TargetState::Cancelled => MixedTaskState::Cancelled,
                        TargetState::TimedOut => MixedTaskState::TimedOut,
                        _ => MixedTaskState::Failed,
                    }
                }
            }
            Self::Upload { row } => match row.phase {
                UploadPhase::Pending => MixedTaskState::Pending,
                UploadPhase::Succeeded => MixedTaskState::Succeeded,
                UploadPhase::Failed | UploadPhase::Committed | UploadPhase::CleanupRequired => {
                    MixedTaskState::Failed
                }
                UploadPhase::Cancelled => MixedTaskState::Cancelled,
                UploadPhase::Unknown => MixedTaskState::Unknown,
                _ => MixedTaskState::Running,
            },
            Self::Download { row } => match row.phase {
                DownloadPhase::Pending => MixedTaskState::Pending,
                DownloadPhase::Succeeded => MixedTaskState::Succeeded,
                DownloadPhase::Failed
                | DownloadPhase::Committed
                | DownloadPhase::CleanupRequired => MixedTaskState::Failed,
                DownloadPhase::Cancelled => MixedTaskState::Cancelled,
                DownloadPhase::Unknown => MixedTaskState::Unknown,
                _ => MixedTaskState::Running,
            },
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedStepRun {
    pub state: MixedTaskState,
    pub receipt: MixedReceipt,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedTargetRun {
    pub run_id: TaskRunId,
    pub host_id: HostId,
    pub revision: u64,
    pub state: MixedTaskState,
    pub next_step: usize,
    pub steps: Vec<MixedStepRun>,
    pub reason: Option<MixedStopReason>,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<MixedRolloutProgress>,
}
#[derive(Clone, Debug)]
pub enum MixedTargetAction {
    Start,
    EnterStep,
    CompleteStep(Option<MixedStopReason>),
    Stop(MixedStopReason),
    Recover,
    Rollout(MixedRolloutAction),
    BeginFileRepair {
        step: usize,
        operation: MixedFileOperation,
    },
    FinishFileRepair,
    Command(TaskRunPatch),
    Upload {
        before: UploadTarget,
        after: UploadTarget,
    },
    Download {
        before: DownloadTarget,
        after: DownloadTarget,
    },
}
#[derive(Clone, Debug)]
pub struct MixedTargetPatch {
    before: MixedTargetRun,
    after: MixedTargetRun,
    fingerprint: [u8; 32],
}
impl MixedTargetPatch {
    #[must_use]
    pub fn before(&self) -> &MixedTargetRun {
        &self.before
    }
    #[must_use]
    pub fn after(&self) -> &MixedTargetRun {
        &self.after
    }
    #[must_use]
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
}
impl MixedTargetRun {
    pub fn initial(snapshot: &MixedTaskSnapshot, host: HostId) -> Result<Self, TaskError> {
        let steps = (0..snapshot.definition().steps.len())
            .map(|index| {
                Ok(MixedStepRun {
                    state: MixedTaskState::Pending,
                    receipt: MixedReceipt::initial(&snapshot.child(host, index)?, host)?,
                })
            })
            .collect::<Result<_, TaskError>>()?;
        Ok(Self {
            run_id: snapshot.id(),
            host_id: host,
            revision: 1,
            state: MixedTaskState::Pending,
            next_step: 0,
            steps,
            reason: None,
            updated_at_ms: snapshot.created_at_ms(),
            rollout: (snapshot.definition().policy.rollout.is_some()
                && host == snapshot.targets().targets()[0].id)
                .then(MixedRolloutProgress::initial),
        })
    }
    pub fn validate(&self, snapshot: &MixedTaskSnapshot) -> Result<(), TaskError> {
        let carrier = snapshot.definition().policy.rollout.is_some()
            && self.host_id == snapshot.targets().targets()[0].id;
        if self.rollout.is_some() != carrier {
            return Err(TaskError::Invalid);
        }
        if let Some(progress) = &self.rollout {
            progress.validate(
                snapshot
                    .definition()
                    .policy
                    .rollout
                    .as_ref()
                    .ok_or(TaskError::Invalid)?,
                snapshot.targets().targets().len(),
                self.updated_at_ms,
            )?;
        }
        if self.run_id != snapshot.id()
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.steps.len() != snapshot.definition().steps.len()
            || self.next_step > self.steps.len()
            || self.updated_at_ms < snapshot.created_at_ms()
            || self.updated_at_ms > i64::MAX as u64
            || (self.next_step == self.steps.len()) != (self.state == MixedTaskState::Succeeded)
        {
            return Err(TaskError::Invalid);
        }
        for (index, step) in self.steps.iter().enumerate() {
            let child = snapshot.child(self.host_id, index)?;
            step.receipt.validate(&child)?;
            let at = match &step.receipt {
                MixedReceipt::Command { meta, .. } => meta.updated_at_ms,
                MixedReceipt::Upload { row } => row.updated_at_ms,
                MixedReceipt::Download { row } => row.updated_at_ms,
            };
            if at > self.updated_at_ms
                || index < self.next_step
                    && (step.state != MixedTaskState::Succeeded
                        || step.receipt.outcome() != MixedTaskState::Succeeded)
                || index > self.next_step
                    && (step.state != MixedTaskState::Pending
                        || step.receipt != MixedReceipt::initial(&child, self.host_id)?)
                || index == self.next_step
                    && match self.state {
                        MixedTaskState::Pending => {
                            step.state != MixedTaskState::Pending
                                || step.receipt.outcome() != MixedTaskState::Pending
                        }
                        MixedTaskState::Running => !matches!(
                            step.state,
                            MixedTaskState::Pending | MixedTaskState::Running
                        ),
                        MixedTaskState::Failed => {
                            !(step.state == MixedTaskState::Failed
                                && step.receipt.outcome() == MixedTaskState::Failed
                                || step.state == MixedTaskState::Pending
                                    && step.receipt.outcome() == MixedTaskState::Pending
                                    && self.reason == Some(MixedStopReason::StorageFailure))
                        }
                        MixedTaskState::Unknown => {
                            step.state != MixedTaskState::Unknown
                                || step.receipt.outcome() != MixedTaskState::Unknown
                        }
                        MixedTaskState::Cancelled => {
                            !matches!(
                                step.state,
                                MixedTaskState::Pending | MixedTaskState::Cancelled
                            ) || !matches!(
                                step.receipt.outcome(),
                                MixedTaskState::Pending | MixedTaskState::Cancelled
                            )
                        }
                        MixedTaskState::TimedOut => {
                            !matches!(
                                step.state,
                                MixedTaskState::Pending
                                    | MixedTaskState::TimedOut
                                    | MixedTaskState::Cancelled
                            ) || !matches!(
                                step.receipt.outcome(),
                                MixedTaskState::Pending
                                    | MixedTaskState::TimedOut
                                    | MixedTaskState::Cancelled
                            )
                        }
                        MixedTaskState::Succeeded => true,
                    }
            {
                return Err(TaskError::Invalid);
            }
        }
        encode(self, MAX_MIXED_TARGET_BYTES)?;
        Ok(())
    }
    pub fn prepare(
        &self,
        snapshot: &MixedTaskSnapshot,
        action: MixedTargetAction,
        now: u64,
    ) -> Result<MixedTargetPatch, TaskError> {
        self.validate(snapshot)?;
        if now < self.updated_at_ms || now > i64::MAX as u64 {
            return Err(TaskError::Clock);
        }
        let mut next = self.clone();
        next.revision = self.revision.checked_add(1).ok_or(TaskError::Limit)?;
        next.updated_at_ms = now;
        match action {
            MixedTargetAction::Rollout(action) => {
                if self.host_id != snapshot.targets().targets()[0].id
                    || self.state == MixedTaskState::Running
                {
                    return Err(TaskError::Transition);
                }
                next.rollout = Some(
                    self.rollout.as_ref().ok_or(TaskError::Invalid)?.prepare(
                        snapshot
                            .definition()
                            .policy
                            .rollout
                            .as_ref()
                            .ok_or(TaskError::Invalid)?,
                        snapshot.targets().targets().len(),
                        snapshot.definition().policy.total_timeout_ms,
                        action,
                        now,
                    )?,
                );
            }
            MixedTargetAction::BeginFileRepair { step, operation } => {
                if operation == MixedFileOperation::Probe
                    || !self.file_repair_allowed(step, operation)
                {
                    return Err(TaskError::Transition);
                }
                next.state = MixedTaskState::Running;
                next.steps[step].state = MixedTaskState::Running;
                next.reason = None;
            }
            MixedTargetAction::FinishFileRepair => {
                if self.state != MixedTaskState::Running
                    || self.steps[self.next_step].state != MixedTaskState::Running
                    || self.steps[self.next_step].receipt.file_revision().is_none()
                {
                    return Err(TaskError::Transition);
                }
                recover_child(&mut next, snapshot, now)?;
                finish_step(&mut next)?;
                if next.state == MixedTaskState::Running {
                    next.state = MixedTaskState::Pending;
                }
            }
            MixedTargetAction::Start => {
                if self.state != MixedTaskState::Pending {
                    return Err(TaskError::Transition);
                }
                next.state = MixedTaskState::Running;
                next.reason = None;
            }
            MixedTargetAction::EnterStep => {
                if self.state != MixedTaskState::Running
                    || self.steps[self.next_step].state != MixedTaskState::Pending
                {
                    return Err(TaskError::Transition);
                }
                next.steps[self.next_step].state = MixedTaskState::Running;
            }
            MixedTargetAction::CompleteStep(reason) => {
                if self.state != MixedTaskState::Running
                    || self.steps[self.next_step].state != MixedTaskState::Running
                    || matches!(
                        self.steps[self.next_step].receipt.outcome(),
                        MixedTaskState::Pending | MixedTaskState::Running
                    )
                {
                    return Err(TaskError::Transition);
                }
                finish_step(&mut next)?;
                if next.state != MixedTaskState::Succeeded
                    && let Some(reason) = reason
                {
                    if next.state == MixedTaskState::Cancelled
                        && reason == MixedStopReason::TotalTimeout
                    {
                        next.state = MixedTaskState::TimedOut;
                    }
                    next.reason = Some(reason);
                }
            }
            MixedTargetAction::Stop(reason) => {
                if !matches!(
                    self.state,
                    MixedTaskState::Pending | MixedTaskState::Running
                ) {
                    return Err(TaskError::Transition);
                }
                if self.steps[self.next_step].state == MixedTaskState::Running {
                    recover_child(&mut next, snapshot, now)?;
                }
                if next.steps[next.next_step].state == MixedTaskState::Running {
                    finish_step(&mut next)?;
                }
                if next.state == MixedTaskState::Running || next.state == MixedTaskState::Pending {
                    next.state = match reason {
                        MixedStopReason::QueueTimeout | MixedStopReason::TotalTimeout => {
                            MixedTaskState::TimedOut
                        }
                        MixedStopReason::StorageFailure => MixedTaskState::Failed,
                        _ => MixedTaskState::Cancelled,
                    };
                }
                next.reason = Some(reason);
            }
            MixedTargetAction::Recover => {
                if self.state != MixedTaskState::Running {
                    return Err(TaskError::Transition);
                }
                if self.steps[self.next_step].state == MixedTaskState::Running {
                    recover_child(&mut next, snapshot, now)?;
                }
                if next.steps[next.next_step].state == MixedTaskState::Running {
                    finish_step(&mut next)?;
                }
                if next.state == MixedTaskState::Running {
                    next.state = MixedTaskState::Pending;
                }
                next.reason = Some(MixedStopReason::Restarted);
            }
            action => {
                if self.state != MixedTaskState::Running
                    || self.steps[self.next_step].state != MixedTaskState::Running
                {
                    return Err(TaskError::Transition);
                }
                let child = snapshot.child(self.host_id, self.next_step)?;
                match (&mut next.steps[self.next_step].receipt, child, action) {
                    (
                        MixedReceipt::Command { meta, target },
                        MixedChildSnapshot::Command(child),
                        MixedTargetAction::Command(patch),
                    ) => {
                        let mut record =
                            TaskRunRecord::restore(child, meta.clone(), vec![target.clone()])?;
                        record.apply_committed(&patch, patch.after().revision)?;
                        *meta = record.meta().clone();
                        *target = record.targets()[0].clone();
                    }
                    (
                        MixedReceipt::Upload { row },
                        MixedChildSnapshot::Upload(child),
                        MixedTargetAction::Upload { before, after },
                    ) => {
                        if *row != before {
                            return Err(TaskError::Transition);
                        }
                        before.validate_next(&after, &child)?;
                        *row = after;
                    }
                    (
                        MixedReceipt::Download { row },
                        MixedChildSnapshot::Download(child),
                        MixedTargetAction::Download { before, after },
                    ) => {
                        if *row != before {
                            return Err(TaskError::Transition);
                        }
                        before.validate_next(&after, &child)?;
                        *row = after;
                    }
                    _ => return Err(TaskError::Transition),
                }
            }
        }
        next.validate(snapshot)?;
        Ok(MixedTargetPatch {
            before: self.clone(),
            after: next,
            fingerprint: *snapshot.fingerprint(),
        })
    }
}
fn finish_step(row: &mut MixedTargetRun) -> Result<(), TaskError> {
    let outcome = row.steps[row.next_step].receipt.outcome();
    match outcome {
        MixedTaskState::Succeeded => {
            row.steps[row.next_step].state = outcome;
            row.next_step += 1;
            if row.next_step == row.steps.len() {
                row.state = outcome;
            }
        }
        MixedTaskState::Failed
        | MixedTaskState::Unknown
        | MixedTaskState::Cancelled
        | MixedTaskState::TimedOut => {
            row.steps[row.next_step].state = outcome;
            row.state = outcome;
            row.reason = Some(if outcome == MixedTaskState::Unknown {
                MixedStopReason::ChildUnknown
            } else if outcome == MixedTaskState::Cancelled {
                MixedStopReason::UserCancelled
            } else {
                MixedStopReason::ChildFailed
            });
        }
        MixedTaskState::Pending => row.steps[row.next_step].state = MixedTaskState::Pending,
        MixedTaskState::Running => return Err(TaskError::Transition),
    }
    Ok(())
}
fn recover_child(
    row: &mut MixedTargetRun,
    snapshot: &MixedTaskSnapshot,
    now: u64,
) -> Result<(), TaskError> {
    let child = snapshot.child(row.host_id, row.next_step)?;
    match (&mut row.steps[row.next_step].receipt, child) {
        (MixedReceipt::Command { meta, target }, MixedChildSnapshot::Command(child)) => {
            let mut record = TaskRunRecord::restore(child, meta.clone(), vec![target.clone()])?;
            if !task_terminal(meta.state) {
                let patch = record.prepare(TaskRunAction::Recover, now)?;
                record.apply_committed(&patch, patch.after().revision)?;
                if record
                    .targets()
                    .iter()
                    .all(|target| target_terminal(target.state))
                {
                    let patch = record.prepare(TaskRunAction::Finish, now)?;
                    record.apply_committed(&patch, patch.after().revision)?;
                }
            }
            *meta = record.meta().clone();
            *target = record.targets()[0].clone();
        }
        (MixedReceipt::Upload { row }, MixedChildSnapshot::Upload(child)) => {
            if row.phase.active() {
                let mut next = row.clone();
                next.phase = UploadPhase::Unknown;
                next.reason = Some(UploadReason::Restarted);
                next.revision = row.revision.checked_add(1).ok_or(TaskError::Limit)?;
                next.updated_at_ms = now;
                row.validate_next(&next, &child)?;
                *row = next;
            }
        }
        (MixedReceipt::Download { row }, MixedChildSnapshot::Download(child)) => {
            if row.phase.active() {
                let mut next = row.clone();
                next.phase = DownloadPhase::Unknown;
                next.reason = Some(DownloadReason::Restarted);
                next.revision = row.revision.checked_add(1).ok_or(TaskError::Limit)?;
                next.updated_at_ms = now;
                row.validate_next(&next, &child)?;
                *row = next;
            }
        }
        _ => return Err(TaskError::Invalid),
    }
    Ok(())
}
pub fn encode_mixed_snapshot(snapshot: &MixedTaskSnapshot) -> Result<Vec<u8>, TaskError> {
    encode(snapshot, MAX_TASK_SNAPSHOT_BYTES)
}
pub fn decode_mixed_snapshot(bytes: &[u8]) -> Result<MixedTaskSnapshot, TaskError> {
    decode(bytes, MAX_TASK_SNAPSHOT_BYTES)
}
pub fn encode_mixed_target(
    row: &MixedTargetRun,
    snapshot: &MixedTaskSnapshot,
) -> Result<Vec<u8>, TaskError> {
    row.validate(snapshot)?;
    encode(row, MAX_MIXED_TARGET_BYTES)
}
pub fn decode_mixed_target(
    bytes: &[u8],
    snapshot: &MixedTaskSnapshot,
) -> Result<MixedTargetRun, TaskError> {
    let row: MixedTargetRun = decode(bytes, MAX_MIXED_TARGET_BYTES)?;
    row.validate(snapshot)?;
    Ok(row)
}
#[async_trait]
pub trait MixedTaskRepository: fmt::Debug + Send + Sync {
    async fn list_mixed_tasks(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<MixedRunSummary>, TaskRepositoryError>;
    async fn load_mixed_page(
        &self,
        id: TaskRunId,
        after: Option<HostId>,
        limit: usize,
    ) -> Result<Option<MixedPageData>, TaskRepositoryError>;
    /// Retry insertion must verify its immutable parent and exact source-row checkpoints
    /// atomically with initial rows/events; ordinary create has no parent provenance.
    async fn create_mixed_task(
        &self,
        snapshot: &MixedTaskSnapshot,
    ) -> Result<(), TaskRepositoryError>;
    async fn load_mixed_task(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(MixedTaskSnapshot, Vec<MixedTargetRun>)>, TaskRepositoryError>;
    async fn load_mixed_target(
        &self,
        id: TaskRunId,
        host: HostId,
    ) -> Result<Option<(MixedTaskSnapshot, MixedTargetRun)>, TaskRepositoryError>;
    async fn commit_mixed_target(
        &self,
        patch: &MixedTargetPatch,
    ) -> Result<u64, TaskRepositoryError>;
}
