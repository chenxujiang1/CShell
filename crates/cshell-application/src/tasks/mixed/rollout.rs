//! Durable rollout state is carried by the first frozen target's existing CAS ledger.
use super::*;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRolloutPolicy {
    pub batch_size: u16,
    pub canary_count: u16,
    pub approve_after_canary: bool,
    pub pause_between_batches: bool,
    pub batch_delay_ms: u64,
    pub failure_limit: Option<u16>,
}
impl Default for MixedRolloutPolicy {
    fn default() -> Self {
        Self {
            batch_size: 20,
            canary_count: 0,
            approve_after_canary: false,
            pause_between_batches: false,
            batch_delay_ms: 0,
            failure_limit: None,
        }
    }
}
impl MixedRolloutPolicy {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.batch_size == 0
            || self.batch_size as usize > crate::MAX_HOST_TARGETS
            || self.canary_count as usize > crate::MAX_HOST_TARGETS
            || self.approve_after_canary && self.canary_count == 0
            || self.batch_delay_ms > MAX_TIMEOUT_MS
            || self
                .failure_limit
                .is_some_and(|value| value == 0 || value as usize > crate::MAX_HOST_TARGETS)
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn batch_count(&self, targets: usize) -> usize {
        let canary = usize::from(self.canary_count).min(targets);
        usize::from(canary != 0) + (targets - canary).div_ceil(usize::from(self.batch_size))
    }
}
pub fn mixed_rollout_batches(snapshot: &MixedTaskSnapshot) -> Vec<Vec<HostId>> {
    let hosts: Vec<_> = snapshot
        .targets()
        .targets()
        .iter()
        .map(|host| host.id)
        .collect();
    let Some(policy) = &snapshot.definition().policy.rollout else {
        return vec![hosts];
    };
    let canary = usize::from(policy.canary_count).min(hosts.len());
    let mut batches = Vec::new();
    if canary != 0 {
        batches.push(hosts[..canary].to_vec());
    }
    batches.extend(
        hosts[canary..]
            .chunks(usize::from(policy.batch_size))
            .map(|chunk| chunk.to_vec()),
    );
    batches
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedRolloutStage {
    Ready,
    Running,
    Paused,
    Stopped,
    Finished,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedRolloutStop {
    UserCancelled,
    TotalTimeout,
    FailureThreshold,
    CanaryFailed,
    Restarted,
    StorageFailure,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRolloutGate {
    pub id: TaskGateId,
    pub kind: TaskGateKind,
    pub next_batch: usize,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRolloutProgress {
    pub revision: u64,
    pub stage: MixedRolloutStage,
    pub next_batch: usize,
    pub approved: BTreeSet<usize>,
    pub gate: Option<MixedRolloutGate>,
    pub not_before_ms: u64,
    pub deadline_ms: Option<u64>,
    pub reason: Option<MixedRolloutStop>,
}
#[derive(Clone, Debug)]
pub enum MixedRolloutAction {
    Start { batch: usize },
    Advance { batch: usize },
    Approve { gate: TaskGateId },
    Stop(MixedRolloutStop),
    Finish,
}
impl MixedRolloutProgress {
    pub fn initial() -> Self {
        Self {
            revision: 1,
            stage: MixedRolloutStage::Ready,
            next_batch: 0,
            approved: BTreeSet::new(),
            gate: None,
            not_before_ms: 0,
            deadline_ms: None,
            reason: None,
        }
    }
    pub fn validate(
        &self,
        policy: &MixedRolloutPolicy,
        targets: usize,
        updated: u64,
    ) -> Result<(), TaskError> {
        policy.validate()?;
        let batches = policy.batch_count(targets);
        if targets == 0
            || targets > crate::MAX_HOST_TARGETS
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.next_batch > batches
            || self.approved.iter().any(|batch| *batch >= batches)
            || (self.stage == MixedRolloutStage::Paused) != self.gate.is_some()
            || (self.stage == MixedRolloutStage::Stopped) != self.reason.is_some()
            || self.stage == MixedRolloutStage::Finished && self.next_batch != batches
            || self.not_before_ms > i64::MAX as u64
            || self.not_before_ms > updated.saturating_add(MAX_TIMEOUT_MS)
            || self
                .deadline_ms
                .is_some_and(|deadline| deadline > i64::MAX as u64)
            || self.deadline_ms.is_none()
                && (!matches!(
                    self.stage,
                    MixedRolloutStage::Ready | MixedRolloutStage::Stopped
                ) || self.next_batch != 0
                    || !self.approved.is_empty()
                    || self.not_before_ms != 0)
            || self.gate.as_ref().is_some_and(|gate| {
                gate.id.as_uuid().is_nil()
                    || gate.next_batch != self.next_batch
                    || gate.next_batch == 0
                    || gate.next_batch >= batches
                    || self.approved.contains(&(gate.next_batch - 1))
                    || match gate.kind {
                        TaskGateKind::Canary => {
                            !policy.approve_after_canary
                                || policy.canary_count == 0
                                || gate.next_batch != 1
                        }
                        TaskGateKind::BetweenBatches => !policy.pause_between_batches,
                    }
            })
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn prepare(
        &self,
        policy: &MixedRolloutPolicy,
        targets: usize,
        total_timeout: u64,
        action: MixedRolloutAction,
        now: u64,
    ) -> Result<Self, TaskError> {
        self.validate(policy, targets, now)?;
        let batches = policy.batch_count(targets);
        let mut next = self.clone();
        next.revision += 1;
        match action {
            MixedRolloutAction::Start { batch } => {
                if self.stage == MixedRolloutStage::Running
                    || self.gate.is_some()
                    || batch >= batches
                    || self.deadline_ms.is_some_and(|deadline| now >= deadline)
                {
                    return Err(TaskError::Transition);
                }
                next.stage = MixedRolloutStage::Running;
                next.next_batch = batch;
                next.reason = None;
                next.deadline_ms = Some(
                    self.deadline_ms
                        .unwrap_or(now.checked_add(total_timeout).ok_or(TaskError::Clock)?),
                );
            }
            MixedRolloutAction::Advance { batch } => {
                if self.stage != MixedRolloutStage::Running
                    || batch != self.next_batch
                    || batch >= batches
                {
                    return Err(TaskError::Transition);
                }
                next.next_batch = batch + 1;
                next.not_before_ms = now
                    .checked_add(policy.batch_delay_ms)
                    .ok_or(TaskError::Clock)?;
                if next.next_batch == batches {
                    next.stage = MixedRolloutStage::Finished;
                } else if !self.approved.contains(&batch)
                    && (batch == 0 && policy.canary_count != 0 && policy.approve_after_canary
                        || policy.pause_between_batches)
                {
                    next.stage = MixedRolloutStage::Paused;
                    next.gate = Some(MixedRolloutGate {
                        id: TaskGateId::new(),
                        kind: if batch == 0
                            && policy.canary_count != 0
                            && policy.approve_after_canary
                        {
                            TaskGateKind::Canary
                        } else {
                            TaskGateKind::BetweenBatches
                        },
                        next_batch: batch + 1,
                    });
                }
            }
            MixedRolloutAction::Approve { gate } => {
                let saved = self.gate.as_ref().ok_or(TaskError::Transition)?;
                if self.stage != MixedRolloutStage::Paused
                    || saved.id != gate
                    || self.deadline_ms.is_some_and(|deadline| now >= deadline)
                {
                    return Err(TaskError::Transition);
                }
                next.approved.insert(saved.next_batch - 1);
                next.gate = None;
                next.stage = MixedRolloutStage::Ready;
            }
            MixedRolloutAction::Stop(reason) => {
                next.stage = MixedRolloutStage::Stopped;
                next.reason = Some(reason);
                next.gate = None;
            }
            MixedRolloutAction::Finish => {
                if self.stage != MixedRolloutStage::Running {
                    return Err(TaskError::Transition);
                }
                next.next_batch = batches;
                next.stage = MixedRolloutStage::Finished;
                next.gate = None;
            }
        }
        next.validate(policy, targets, now)?;
        Ok(next)
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRolloutDocument {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub snapshot_sha256: [u8; 32],
    pub carrier_host: HostId,
    pub carrier_revision: u64,
    pub target_count: usize,
    pub policy: MixedRolloutPolicy,
    pub progress: MixedRolloutProgress,
    pub updated_at_ms: u64,
    pub active: bool,
}
impl MixedRolloutDocument {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.mixed-rollout"
            || self.version != 1
            || self.run_id.as_uuid().is_nil()
            || self.carrier_host.as_uuid().is_nil()
            || self.carrier_revision == 0
            || self.carrier_revision > i64::MAX as u64
            || self.updated_at_ms > i64::MAX as u64
        {
            return Err(TaskError::Invalid);
        }
        self.progress
            .validate(&self.policy, self.target_count, self.updated_at_ms)
    }
    pub fn check_snapshot(&self, plan: &MixedTaskSnapshot) -> Result<(), TaskError> {
        self.validate()?;
        if self.run_id != plan.id()
            || self.snapshot_sha256 != *plan.fingerprint()
            || plan.definition().policy.rollout.as_ref() != Some(&self.policy)
            || self.carrier_host != plan.targets().targets()[0].id
            || self.target_count != plan.targets().targets().len()
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
pub fn decode_mixed_rollout(bytes: &[u8]) -> Result<MixedRolloutDocument, TaskError> {
    let value: MixedRolloutDocument = decode(bytes, 128 * 1024)?;
    value.validate()?;
    Ok(value)
}
