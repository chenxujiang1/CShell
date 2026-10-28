//! Bounded client documents omit command bodies, file paths and credential templates.
use super::*;
pub const MAX_MIXED_STATUS_TARGETS: usize = 64;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedTargetRevision {
    pub host_id: HostId,
    pub revision: u64,
}
fn revisions_valid(rows: &[MixedTargetRevision]) -> bool {
    rows.len() <= crate::MAX_HOST_TARGETS
        && rows
            .windows(2)
            .all(|pair| pair[0].host_id < pair[1].host_id)
        && rows.iter().all(|row| {
            !row.host_id.as_uuid().is_nil() && row.revision > 0 && row.revision <= i64::MAX as u64
        })
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedStartConfirmation {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub snapshot_sha256: [u8; 32],
    pub pending: Vec<MixedTargetRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_revision: Option<u64>,
}
impl MixedStartConfirmation {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.mixed-start"
            || self.version != 1
            || self.run_id.as_uuid().is_nil()
            || self.pending.is_empty()
            || !revisions_valid(&self.pending)
            || self
                .control_revision
                .is_some_and(|revision| revision == 0 || revision > i64::MAX as u64)
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
pub fn decode_mixed_confirmation(bytes: &[u8]) -> Result<MixedStartConfirmation, TaskError> {
    let value: MixedStartConfirmation = decode(bytes, 128 * 1024)?;
    value.validate()?;
    Ok(value)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MixedStepInfoKind {
    Command { success_codes: BTreeSet<u32> },
    Upload { content: UploadContent },
    Download { maximum_bytes: u64 },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedStepInfo {
    pub name: String,
    pub details: MixedStepInfoKind,
}
impl MixedStepInfo {
    fn from_action(action: &MixedTaskAction) -> Self {
        let details = match action {
            MixedTaskAction::Command { definition } => MixedStepInfoKind::Command {
                success_codes: definition.steps[0].success_codes.clone(),
            },
            MixedTaskAction::Upload { definition } => MixedStepInfoKind::Upload {
                content: UploadContent {
                    size: definition.size,
                    sha256: definition.sha256,
                },
            },
            MixedTaskAction::Download { definition } => MixedStepInfoKind::Download {
                maximum_bytes: definition.maximum_bytes,
            },
        };
        Self {
            name: action.name().into(),
            details,
        }
    }
    pub(super) fn validate(&self) -> bool {
        label(&self.name)
            && match &self.details {
                MixedStepInfoKind::Command { success_codes } => {
                    !success_codes.is_empty() && success_codes.len() <= 16
                }
                MixedStepInfoKind::Upload { content } => content.size <= MAX_UPLOAD_FILE_BYTES,
                MixedStepInfoKind::Download { maximum_bytes } => {
                    *maximum_bytes <= MAX_DOWNLOAD_FILE_BYTES
                }
            }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedTargetView {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub children: Vec<TaskRunId>,
    pub run: MixedTargetRun,
}
#[derive(Clone, Debug)]
pub struct MixedPageData {
    pub snapshot: MixedTaskSnapshot,
    pub rows: Vec<MixedTargetRun>,
    pub counts: [u64; 7],
    pub pending: Vec<MixedTargetRevision>,
    pub after: Option<HostId>,
    pub next: Option<HostId>,
    pub limit: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedStatusPage {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub name: String,
    pub created_at_ms: u64,
    pub snapshot_sha256: [u8; 32],
    pub host_revision: u64,
    pub profile_revision: u64,
    pub policy: MixedTaskPolicy,
    pub steps: Vec<MixedStepInfo>,
    pub total_targets: u64,
    pub counts: [u64; 7],
    pub pending: Vec<MixedTargetRevision>,
    pub active: bool,
    pub after: Option<HostId>,
    pub next: Option<HostId>,
    pub limit: usize,
    pub targets: Vec<MixedTargetView>,
}
impl fmt::Debug for MixedStatusPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixedStatusPage")
            .field("run_id", &self.run_id)
            .field("targets", &self.targets.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}
impl MixedStatusPage {
    pub fn check_snapshot(&self, snapshot: &MixedTaskSnapshot) -> Result<(), TaskError> {
        self.validate()?;
        if self.run_id != snapshot.id()
            || self.snapshot_sha256 != *snapshot.fingerprint()
            || self.name != snapshot.definition().name
            || self.created_at_ms != snapshot.created_at_ms()
            || self.host_revision != snapshot.targets().host_revision()
            || self.profile_revision != snapshot.targets().profile_revision()
            || self.policy != snapshot.definition().policy
            || self.total_targets != snapshot.targets().targets().len() as u64
            || self.steps
                != snapshot
                    .definition()
                    .steps
                    .iter()
                    .map(MixedStepInfo::from_action)
                    .collect::<Vec<_>>()
        {
            return Err(TaskError::Invalid);
        }
        for target in &self.targets {
            target.run.validate(snapshot)?;
            let index = snapshot
                .targets()
                .targets()
                .binary_search_by_key(&target.run.host_id, |host| host.id)
                .map_err(|_| TaskError::Invalid)?;
            let host = &snapshot.targets().targets()[index];
            if target.name != host.name
                || target.address != host.address
                || target.port != host.port
                || target.children != snapshot.data.children[index]
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
    pub fn from_data(data: MixedPageData, active: bool) -> Result<Self, TaskError> {
        let snapshot = data.snapshot;
        let mut targets = Vec::with_capacity(data.rows.len());
        for row in data.rows {
            row.validate(&snapshot)?;
            let index = snapshot
                .targets()
                .targets()
                .binary_search_by_key(&row.host_id, |host| host.id)
                .map_err(|_| TaskError::Invalid)?;
            let host = &snapshot.targets().targets()[index];
            targets.push(MixedTargetView {
                name: host.name.clone(),
                address: host.address.clone(),
                port: host.port,
                children: snapshot.data.children[index].clone(),
                run: row,
            });
        }
        let value = Self {
            format: "cshell.mixed-status".into(),
            version: 1,
            run_id: snapshot.id(),
            name: snapshot.definition().name.clone(),
            created_at_ms: snapshot.created_at_ms(),
            snapshot_sha256: *snapshot.fingerprint(),
            host_revision: snapshot.targets().host_revision(),
            profile_revision: snapshot.targets().profile_revision(),
            policy: snapshot.definition().policy.clone(),
            steps: snapshot
                .definition()
                .steps
                .iter()
                .map(MixedStepInfo::from_action)
                .collect(),
            total_targets: snapshot.targets().targets().len() as u64,
            counts: data.counts,
            pending: data.pending,
            active,
            after: data.after,
            next: data.next,
            limit: data.limit,
            targets,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn confirmation(&self) -> Result<MixedStartConfirmation, TaskError> {
        if self.policy.rollout.is_some() {
            return Err(TaskError::Transition);
        }
        self.confirmation_bound(None)
    }
    pub fn confirmation_bound(
        &self,
        control: Option<&MixedRolloutDocument>,
    ) -> Result<MixedStartConfirmation, TaskError> {
        self.validate()?;
        if self.active {
            return Err(TaskError::Transition);
        }
        let value = MixedStartConfirmation {
            format: "cshell.mixed-start".into(),
            version: 1,
            run_id: self.run_id,
            snapshot_sha256: self.snapshot_sha256,
            pending: self.pending.clone(),
            control_revision: if self.policy.rollout.is_some() {
                let control = control.ok_or(TaskError::Transition)?;
                control.validate()?;
                if control.run_id != self.run_id
                    || control.snapshot_sha256 != self.snapshot_sha256
                    || control.policy != *self.policy.rollout.as_ref().ok_or(TaskError::Invalid)?
                    || control.active
                    || self.pending.iter().any(|target| {
                        target.host_id == control.carrier_host
                            && target.revision != control.carrier_revision
                    })
                    || control.progress.stage == MixedRolloutStage::Running
                    || control.progress.gate.is_some()
                {
                    return Err(TaskError::Transition);
                }
                Some(control.progress.revision)
            } else {
                None
            },
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        self.policy.validate()?;
        if self.format != "cshell.mixed-status"
            || self.version != 1
            || self.run_id.as_uuid().is_nil()
            || !label(&self.name)
            || self.created_at_ms > i64::MAX as u64 - MAX_TIMEOUT_MS
            || self.total_targets == 0
            || self.total_targets > crate::MAX_HOST_TARGETS as u64
            || self.steps.is_empty()
            || self.steps.len() > MAX_TASK_STEPS
            || self.steps.iter().any(|step| !step.validate())
            || self.policy.concurrency == 0
            || self.policy.concurrency > 50
            || [self.policy.queue_timeout_ms, self.policy.total_timeout_ms]
                .iter()
                .any(|value| *value == 0 || *value > MAX_TIMEOUT_MS)
            || self
                .counts
                .iter()
                .try_fold(0_u64, |sum, count| sum.checked_add(*count))
                != Some(self.total_targets)
            || !revisions_valid(&self.pending)
            || self.pending.len() as u64 != self.counts[MixedTaskState::Pending.code() as usize]
            || self.limit == 0
            || self.limit > MAX_MIXED_STATUS_TARGETS
            || self.targets.len() > self.limit
            || self.targets.len() as u64 > self.total_targets
            || self.after.is_some_and(|id| id.as_uuid().is_nil())
            || self
                .targets
                .windows(2)
                .any(|pair| pair[0].run.host_id >= pair[1].run.host_id)
            || self.next.is_some_and(|id| {
                self.targets.len() != self.limit
                    || self.targets.last().is_none_or(|row| row.run.host_id != id)
            })
        {
            return Err(TaskError::Invalid);
        }
        let mut ids = BTreeSet::from([self.run_id]);
        let mut visible = [0_u64; 7];
        for target in &self.targets {
            let row = &target.run;
            if let Some(progress) = &row.rollout {
                progress.validate(
                    self.policy.rollout.as_ref().ok_or(TaskError::Invalid)?,
                    self.total_targets as usize,
                    row.updated_at_ms,
                )?;
            }
            if row.host_id.as_uuid().is_nil()
                || self.after.is_some_and(|id| row.host_id <= id)
                || !label(&target.name)
                || target.address.is_empty()
                || target.address.len() > 255
                || target.address.chars().any(char::is_control)
                || target.port == 0
                || row.run_id != self.run_id
                || row.revision == 0
                || row.revision > i64::MAX as u64
                || row.updated_at_ms < self.created_at_ms
                || row.updated_at_ms > i64::MAX as u64
                || row.steps.len() != self.steps.len()
                || target.children.len() != self.steps.len()
                || row.next_step > self.steps.len()
                || (row.state == MixedTaskState::Succeeded) != (row.next_step == self.steps.len())
            {
                return Err(TaskError::Invalid);
            }
            let pending = self
                .pending
                .binary_search_by_key(&row.host_id, |item| item.host_id)
                .ok();
            if row.state == MixedTaskState::Pending
                && pending.is_none_or(|index| self.pending[index].revision != row.revision)
                || row.state != MixedTaskState::Pending && pending.is_some()
            {
                return Err(TaskError::Invalid);
            }
            visible[row.state.code() as usize] += 1;
            for (index, (step, info)) in row.steps.iter().zip(&self.steps).enumerate() {
                let child = target.children[index];
                if child.as_uuid().is_nil() || !ids.insert(child) {
                    return Err(TaskError::Invalid);
                }
                validate_receipt(
                    &step.receipt,
                    &info.details,
                    child,
                    row.host_id,
                    self.created_at_ms,
                    row.updated_at_ms,
                )?;
                if index < row.next_step
                    && (step.state != MixedTaskState::Succeeded
                        || step.receipt.outcome() != MixedTaskState::Succeeded)
                    || index > row.next_step
                        && (step.state != MixedTaskState::Pending
                            || step.receipt.outcome() != MixedTaskState::Pending)
                {
                    return Err(TaskError::Invalid);
                }
                if index == row.next_step
                    && match row.state {
                        MixedTaskState::Pending => {
                            step.state != MixedTaskState::Pending
                                || step.receipt.outcome() != MixedTaskState::Pending
                        }
                        MixedTaskState::Running => !matches!(
                            step.state,
                            MixedTaskState::Pending | MixedTaskState::Running
                        ),
                        MixedTaskState::Unknown => {
                            step.state != MixedTaskState::Unknown
                                || step.receipt.outcome() != MixedTaskState::Unknown
                        }
                        MixedTaskState::Failed => {
                            !(step.state == MixedTaskState::Failed
                                && step.receipt.outcome() == MixedTaskState::Failed
                                || step.state == MixedTaskState::Pending
                                    && step.receipt.outcome() == MixedTaskState::Pending
                                    && row.reason == Some(MixedStopReason::StorageFailure))
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
                                    | MixedTaskState::Cancelled
                                    | MixedTaskState::TimedOut
                            ) || !matches!(
                                step.receipt.outcome(),
                                MixedTaskState::Pending
                                    | MixedTaskState::Cancelled
                                    | MixedTaskState::TimedOut
                            )
                        }
                        MixedTaskState::Succeeded => true,
                    }
                {
                    return Err(TaskError::Invalid);
                }
            }
        }
        if visible
            .iter()
            .zip(self.counts)
            .any(|(visible, total)| *visible > total)
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
fn validate_receipt(
    receipt: &MixedReceipt,
    info: &MixedStepInfoKind,
    child: TaskRunId,
    host: HostId,
    created: u64,
    updated: u64,
) -> Result<(), TaskError> {
    match (receipt, info) {
        (MixedReceipt::Command { meta, target }, MixedStepInfoKind::Command { success_codes }) => {
            if meta.revision == 0
                || meta.revision > i64::MAX as u64
                || meta.created_at_ms != created
                || meta.updated_at_ms < created
                || meta.updated_at_ms > updated
                || target.host_id != host
                || target.steps.len() != 1
                || meta.gate.is_some()
                || meta.next_batch > 1
                || meta.batch_not_before_ms > meta.updated_at_ms
            {
                return Err(TaskError::Invalid);
            }
            for time in [
                target.started_at_ms,
                target.finished_at_ms,
                target.steps[0].started_at_ms,
                target.steps[0].finished_at_ms,
            ]
            .into_iter()
            .flatten()
            {
                if time < created || time > meta.updated_at_ms {
                    return Err(TaskError::Invalid);
                }
            }
            let step = &target.steps[0];
            if step.state == TargetState::Succeeded
                && step
                    .exit_code
                    .is_none_or(|code| !success_codes.contains(&code))
                || step.state == TargetState::Failed
                    && step
                        .exit_code
                        .is_some_and(|code| success_codes.contains(&code))
                || !matches!(step.state, TargetState::Succeeded | TargetState::Failed)
                    && step.exit_code.is_some()
                || target_terminal(step.state) != step.finished_at_ms.is_some()
                || target_terminal(target.state) != target.finished_at_ms.is_some()
                || target.state == TargetState::Succeeded && step.state != TargetState::Succeeded
                || step.state == TargetState::Connecting
                || step.state == TargetState::Pending && step.started_at_ms.is_some()
                || matches!(
                    step.state,
                    TargetState::Running
                        | TargetState::Succeeded
                        | TargetState::Failed
                        | TargetState::Unknown
                ) && step.started_at_ms.is_none()
                || task_terminal(meta.state)
                    && ((meta.state == TaskState::Completed)
                        != (target.state == TargetState::Succeeded)
                        || !target_terminal(target.state))
                || step
                    .started_at_ms
                    .zip(step.finished_at_ms)
                    .is_some_and(|(start, end)| start > end)
                || target
                    .started_at_ms
                    .zip(target.finished_at_ms)
                    .is_some_and(|(start, end)| start > end)
            {
                return Err(TaskError::Invalid);
            }
        }
        (MixedReceipt::Upload { row }, MixedStepInfoKind::Upload { content }) => {
            if row.run_id != child
                || row.host_id != host
                || row.revision == 0
                || row.revision > i64::MAX as u64
                || row.updated_at_ms < created
                || row.updated_at_ms > updated
                || row.bytes > content.size
                || row.verified && (!row.temporary_owned || row.bytes != content.size)
                || row.committed && !row.verified
                || row.adopted && !row.committed
                || row.temporary_cleaned && !row.temporary_owned
                || row.phase == UploadPhase::Succeeded && (!row.committed || !row.temporary_cleaned)
            {
                return Err(TaskError::Invalid);
            }
        }
        (MixedReceipt::Download { row }, MixedStepInfoKind::Download { maximum_bytes }) => {
            row.validate_state(child, created, *maximum_bytes)?;
            if row.host_id != host || row.updated_at_ms > updated {
                return Err(TaskError::Invalid);
            }
        }
        _ => return Err(TaskError::Invalid),
    }
    Ok(())
}
pub fn decode_mixed_status(bytes: &[u8]) -> Result<MixedStatusPage, TaskError> {
    let value: MixedStatusPage = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    value.validate()?;
    Ok(value)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRunSummary {
    pub id: TaskRunId,
    pub name: String,
    pub created_at_ms: u64,
    pub targets: u64,
}
pub fn decode_mixed_list(bytes: &[u8]) -> Result<Vec<MixedRunSummary>, TaskError> {
    let rows: Vec<MixedRunSummary> = decode(bytes, 256 * 1024)?;
    if rows.len() > 64
        || rows.windows(2).any(|pair| pair[0].id >= pair[1].id)
        || rows.iter().any(|row| {
            row.id.as_uuid().is_nil()
                || !label(&row.name)
                || row.created_at_ms > i64::MAX as u64
                || row.targets == 0
                || row.targets > crate::MAX_HOST_TARGETS as u64
        })
    {
        return Err(TaskError::Invalid);
    }
    Ok(rows)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedOutputContext {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub host_id: HostId,
    pub child_id: TaskRunId,
    pub step: u32,
    pub target_revision: u64,
    pub step_state: MixedTaskState,
    pub stderr: bool,
    pub offset: u64,
    pub next_offset: u64,
    pub length: u64,
}
pub fn decode_mixed_output(bytes: &[u8], body: &[u8]) -> Result<MixedOutputContext, TaskError> {
    let value: MixedOutputContext = decode(bytes, 16 * 1024)?;
    if value.format != "cshell.mixed-output"
        || value.version != 1
        || value.run_id.as_uuid().is_nil()
        || value.host_id.as_uuid().is_nil()
        || value.child_id.as_uuid().is_nil()
        || value.child_id == value.run_id
        || value.step as usize >= MAX_TASK_STEPS
        || value.target_revision == 0
        || value.target_revision > i64::MAX as u64
        || value.step_state == MixedTaskState::Pending
        || body.len() > 64 * 1024
        || value.next_offset
            != value
                .offset
                .checked_add(body.len() as u64)
                .ok_or(TaskError::Limit)?
        || value.next_offset > value.length
        || value.length > 1024 * 1024 * 1024
    {
        return Err(TaskError::Invalid);
    }
    Ok(value)
}
