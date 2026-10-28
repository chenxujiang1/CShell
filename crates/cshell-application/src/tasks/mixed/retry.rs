//! Explicit whole-plan replay into a new stopped-run descendant, never cursor recovery.
use super::*;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRetryConfirmation {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub snapshot_sha256: [u8; 32],
    pub targets: Vec<MixedTargetRevision>,
    pub confirm_unknown: bool,
    pub confirm_file_replay: bool,
}
impl MixedRetryConfirmation {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.mixed-retry"
            || self.version != 1
            || self.run_id.as_uuid().is_nil()
            || self.targets.is_empty()
            || self.targets.len() > crate::MAX_HOST_TARGETS
            || self
                .targets
                .windows(2)
                .any(|pair| pair[0].host_id >= pair[1].host_id)
            || self.targets.iter().any(|target| {
                target.host_id.as_uuid().is_nil()
                    || target.revision == 0
                    || target.revision > i64::MAX as u64
            })
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
pub fn decode_mixed_retry_confirmation(bytes: &[u8]) -> Result<MixedRetryConfirmation, TaskError> {
    let value: MixedRetryConfirmation = decode(bytes, 128 * 1024)?;
    value.validate()?;
    Ok(value)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRetrySource {
    pub host_id: HostId,
    pub revision: u64,
    pub state: MixedTaskState,
    pub row_sha256: [u8; 32],
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRetryProvenance {
    pub parent_id: TaskRunId,
    pub parent_sha256: [u8; 32],
    pub sources: Vec<MixedRetrySource>,
    pub replayed_commands: BTreeSet<usize>,
    pub unknown_confirmed: bool,
    pub file_replay_confirmed: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixedRetryBlock {
    NotStopped,
    Succeeded,
    CommandNotIdempotent,
    UncertainFile,
    ActiveFile,
}
pub fn mixed_retry_block(
    snapshot: &MixedTaskSnapshot,
    row: &MixedTargetRun,
) -> Option<MixedRetryBlock> {
    if matches!(row.state, MixedTaskState::Pending | MixedTaskState::Running) {
        return Some(MixedRetryBlock::NotStopped);
    }
    if row.state == MixedTaskState::Succeeded {
        return Some(MixedRetryBlock::Succeeded);
    }
    for (index, step) in row.steps.iter().enumerate() {
        match &step.receipt {
            MixedReceipt::Command { target, .. } => {
                let Some(MixedTaskAction::Command { definition }) =
                    snapshot.definition().steps.get(index)
                else {
                    return Some(MixedRetryBlock::NotStopped);
                };
                if target.steps.first().is_none_or(|step| {
                    step.started_at_ms.is_some() && !definition.steps[0].idempotent
                }) {
                    return Some(MixedRetryBlock::CommandNotIdempotent);
                }
            }
            MixedReceipt::Upload { row } => {
                if row.phase == UploadPhase::Unknown {
                    return Some(MixedRetryBlock::UncertainFile);
                }
                if row.phase.active() {
                    return Some(MixedRetryBlock::ActiveFile);
                }
            }
            MixedReceipt::Download { row } => {
                if row.phase == DownloadPhase::Unknown {
                    return Some(MixedRetryBlock::UncertainFile);
                }
                if row.phase.active() {
                    return Some(MixedRetryBlock::ActiveFile);
                }
            }
        }
    }
    None
}
impl MixedRetryProvenance {
    pub(super) fn validate_shape(&self, data: &MixedData) -> Result<(), TaskError> {
        if self.parent_id.as_uuid().is_nil()||self.parent_id==data.id||self.sources.len()!=data.targets.targets().len()||self.sources.is_empty()
            ||self.sources.iter().zip(data.targets.targets()).any(|(source,host)|source.host_id!=host.id||source.revision==0||source.revision>i64::MAX as u64||matches!(source.state,MixedTaskState::Pending|MixedTaskState::Running|MixedTaskState::Succeeded))
            ||self.sources.iter().any(|source|source.state==MixedTaskState::Unknown)!=self.unknown_confirmed
            ||self.file_replay_confirmed!=data.definition.steps.iter().any(|step|!matches!(step,MixedTaskAction::Command{..}))
            ||self.replayed_commands.iter().any(|index|!matches!(data.definition.steps.get(*index),Some(MixedTaskAction::Command{definition})if definition.steps[0].idempotent)){return Err(TaskError::Invalid);}
        Ok(())
    }
}
impl MixedTaskSnapshot {
    pub fn retry_provenance(&self) -> Option<&MixedRetryProvenance> {
        self.data.retry.as_ref()
    }
    pub fn retry_has_files(&self) -> bool {
        self.definition()
            .steps
            .iter()
            .any(|step| !matches!(step, MixedTaskAction::Command { .. }))
    }
    pub fn retry(
        parent: &Self,
        rows: &[MixedTargetRun],
        confirmation: &MixedRetryConfirmation,
        operator: String,
        now: u64,
    ) -> Result<Self, TaskError> {
        confirmation.validate()?;
        if confirmation.run_id != parent.id()
            || confirmation.snapshot_sha256 != *parent.fingerprint()
        {
            return Err(TaskError::Transition);
        }
        let mut sources = Vec::new();
        let mut replayed = BTreeSet::new();
        let mut unknown = false;
        for expected in &confirmation.targets {
            let row = rows
                .iter()
                .find(|row| row.host_id == expected.host_id)
                .ok_or(TaskError::UnknownTarget)?;
            row.validate(parent)?;
            if row.revision != expected.revision || row.updated_at_ms > now {
                return Err(TaskError::Transition);
            }
            if mixed_retry_block(parent, row).is_some() {
                return Err(TaskError::UnsafeRetry);
            }
            unknown |= row.state == MixedTaskState::Unknown;
            for (index, step) in row.steps.iter().enumerate() {
                if let MixedReceipt::Command { target, .. } = &step.receipt
                    && target.steps[0].started_at_ms.is_some()
                {
                    replayed.insert(index);
                }
            }
            sources.push(MixedRetrySource {
                host_id: row.host_id,
                revision: row.revision,
                state: row.state,
                row_sha256: Sha256::digest(encode_mixed_target(row, parent)?).into(),
            });
        }
        if unknown && !confirmation.confirm_unknown
            || parent.retry_has_files() && !confirmation.confirm_file_replay
        {
            return Err(TaskError::UnsafeRetry);
        }
        let selected = sources.iter().map(|source| source.host_id).collect();
        let mut next = Self::new(
            parent.definition().clone(),
            parent
                .targets()
                .subset(&selected)
                .map_err(|_| TaskError::UnknownTarget)?,
            operator,
            now,
        )?;
        let data = Arc::make_mut(&mut next.data);
        data.version = 2;
        data.retry = Some(MixedRetryProvenance {
            parent_id: parent.id(),
            parent_sha256: *parent.fingerprint(),
            sources,
            replayed_commands: replayed,
            unknown_confirmed: unknown,
            file_replay_confirmed: parent.retry_has_files(),
        });
        validate_mixed_data(&next.data)?;
        next.validate_retry_parent(parent)?;
        next.fingerprint = Sha256::digest(encode(&next.data, MAX_TASK_SNAPSHOT_BYTES)?).into();
        Ok(next)
    }
    pub fn validate_retry_parent(&self, parent: &Self) -> Result<(), TaskError> {
        let proof = self.retry_provenance().ok_or(TaskError::Invalid)?;
        if proof.parent_id != parent.id()
            || proof.parent_sha256 != *parent.fingerprint()
            || self.id() == parent.id()
            || self.created_at_ms() < parent.created_at_ms()
            || self.definition() != parent.definition()
            || self.targets().id() == parent.targets().id()
        {
            return Err(TaskError::Invalid);
        }
        let ids = proof.sources.iter().map(|source| source.host_id).collect();
        let expected = parent
            .targets()
            .subset(&ids)
            .map_err(|_| TaskError::UnknownTarget)?;
        if self.targets().targets() != expected.targets()
            || self.targets().connections() != expected.connections()
            || self.targets().selection() != expected.selection()
            || self.targets().host_revision() != expected.host_revision()
            || self.targets().profile_revision() != expected.profile_revision()
        {
            return Err(TaskError::Invalid);
        }
        let old: BTreeSet<_> = std::iter::once(parent.id())
            .chain(parent.data.children.iter().flatten().copied())
            .collect();
        if old.contains(&self.id())
            || self
                .data
                .children
                .iter()
                .flatten()
                .any(|id| old.contains(id))
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn check_retry_confirmation(
        &self,
        parent: &Self,
        confirmation: &MixedRetryConfirmation,
    ) -> Result<(), TaskError> {
        confirmation.validate()?;
        self.validate_retry_parent(parent)?;
        let proof = self.retry_provenance().ok_or(TaskError::Invalid)?;
        let versions: Vec<_> = proof
            .sources
            .iter()
            .map(|source| MixedTargetRevision {
                host_id: source.host_id,
                revision: source.revision,
            })
            .collect();
        if confirmation.run_id != parent.id()
            || confirmation.snapshot_sha256 != *parent.fingerprint()
            || versions != confirmation.targets
            || proof.unknown_confirmed && !confirmation.confirm_unknown
            || proof.file_replay_confirmed && !confirmation.confirm_file_replay
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    /// Called during new-run insertion against the source rows in that same transaction.
    pub fn validate_retry_rows(
        &self,
        parent: &Self,
        rows: &[MixedTargetRun],
    ) -> Result<(), TaskError> {
        self.validate_retry_parent(parent)?;
        let proof = self.retry_provenance().ok_or(TaskError::Invalid)?;
        let mut replayed = BTreeSet::new();
        if rows.len() != proof.sources.len() {
            return Err(TaskError::Invalid);
        }
        for (source, row) in proof.sources.iter().zip(rows) {
            row.validate(parent)?;
            let hash: [u8; 32] = Sha256::digest(encode_mixed_target(row, parent)?).into();
            if source.host_id != row.host_id
                || source.revision != row.revision
                || source.state != row.state
                || source.row_sha256 != hash
                || row.updated_at_ms > self.created_at_ms()
                || mixed_retry_block(parent, row).is_some()
            {
                return Err(TaskError::UnsafeRetry);
            }
            for (index, step) in row.steps.iter().enumerate() {
                if let MixedReceipt::Command { target, .. } = &step.receipt
                    && target.steps[0].started_at_ms.is_some()
                {
                    replayed.insert(index);
                }
            }
        }
        if replayed != proof.replayed_commands {
            return Err(TaskError::UnsafeRetry);
        }
        Ok(())
    }
}
