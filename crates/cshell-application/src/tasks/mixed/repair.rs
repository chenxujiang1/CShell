use super::*;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedFileOperation {
    Probe,
    Resume,
    Cleanup,
    Adopt,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedFileProbe {
    pub temporary: Option<UploadContent>,
    pub destination: Option<UploadContent>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedRepairError {
    Cancelled,
    Conflict,
    Unavailable,
    Corrupt,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRepairJob {
    pub host_id: HostId,
    pub step: usize,
    pub parent_revision: u64,
    pub child_id: TaskRunId,
    pub child_revision: u64,
    pub operation: MixedFileOperation,
    pub status: FileRecoveryStatus,
    pub probe: Option<MixedFileProbe>,
    pub result_revision: Option<u64>,
    pub error: Option<MixedRepairError>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedRepairDocument {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub active: bool,
    pub job: Option<MixedRepairJob>,
}
impl MixedRepairDocument {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.mixed-file-repair"
            || self.version != 1
            || self.run_id.as_uuid().is_nil()
        {
            return Err(TaskError::Invalid);
        }
        if let Some(job) = &self.job {
            if job.host_id.as_uuid().is_nil()
                || job.child_id.as_uuid().is_nil()
                || job.child_id == self.run_id
                || job.step >= MAX_TASK_STEPS
                || job.parent_revision == 0
                || job.parent_revision > i64::MAX as u64
                || job.child_revision == 0
                || job.child_revision > i64::MAX as u64
                || job.result_revision.is_some_and(|revision| {
                    revision < job.parent_revision || revision > i64::MAX as u64
                })
                || job.status == FileRecoveryStatus::Running
                    && (!self.active
                        || job.error.is_some()
                        || job.probe.is_some()
                        || job.result_revision.is_some())
                || job.status == FileRecoveryStatus::Completed
                    && (job.error.is_some()
                        || job.result_revision.is_none()
                        || (job.operation == MixedFileOperation::Probe) != job.probe.is_some())
                || job.status == FileRecoveryStatus::Failed
                    && (job.error.is_none() || job.probe.is_some())
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
    #[must_use]
    pub fn matching_destination(
        &self,
        snapshot: &MixedTaskSnapshot,
        target: &MixedTargetRun,
        step: usize,
    ) -> bool {
        let Some(job) = &self.job else {
            return false;
        };
        let Ok(child) = snapshot.child(target.host_id, step) else {
            return false;
        };
        self.run_id == snapshot.id()
            && job.host_id == target.host_id
            && job.step == step
            && job.parent_revision == target.revision
            && job.child_id == child.id()
            && target
                .steps
                .get(step)
                .and_then(|step| step.receipt.file_revision())
                == Some(job.child_revision)
            && job.operation == MixedFileOperation::Probe
            && job.status == FileRecoveryStatus::Completed
            && job.probe.as_ref().is_some_and(|probe| {
                probe.destination.is_some()
                    && probe.destination
                        == target.steps[step].receipt.expected_content(snapshot, step)
            })
    }
}
pub fn decode_mixed_repair(bytes: &[u8]) -> Result<MixedRepairDocument, TaskError> {
    let value: MixedRepairDocument = decode(bytes, 32 * 1024)?;
    value.validate()?;
    Ok(value)
}
impl MixedReceipt {
    #[must_use]
    pub fn file_revision(&self) -> Option<u64> {
        match self {
            Self::Upload { row } => Some(row.revision),
            Self::Download { row } => Some(row.revision),
            _ => None,
        }
    }
    #[must_use]
    pub fn expected_content(
        &self,
        snapshot: &MixedTaskSnapshot,
        step: usize,
    ) -> Option<UploadContent> {
        match (self, snapshot.definition().steps.get(step)) {
            (Self::Upload { .. }, Some(MixedTaskAction::Upload { definition })) => {
                Some(UploadContent {
                    size: definition.size,
                    sha256: definition.sha256,
                })
            }
            (Self::Download { row }, Some(MixedTaskAction::Download { .. })) => {
                row.content.map(|content| UploadContent {
                    size: content.size,
                    sha256: content.sha256,
                })
            }
            _ => None,
        }
    }
}
impl MixedTargetRun {
    #[must_use]
    pub fn file_repair_allowed(&self, step: usize, operation: MixedFileOperation) -> bool {
        let Some(saved) = self.steps.get(step) else {
            return false;
        };
        if operation == MixedFileOperation::Probe {
            return step <= self.next_step
                && saved.state != MixedTaskState::Pending
                && saved.receipt.file_revision().is_some();
        }
        if step != self.next_step
            || !matches!(
                self.state,
                MixedTaskState::Failed
                    | MixedTaskState::Cancelled
                    | MixedTaskState::Unknown
                    | MixedTaskState::TimedOut
            )
        {
            return false;
        }
        match &saved.receipt {
            MixedReceipt::Upload { row } => match operation {
                MixedFileOperation::Resume => row.can_resume(),
                MixedFileOperation::Cleanup => row.can_cleanup(),
                MixedFileOperation::Adopt => row.can_adopt(),
                _ => false,
            },
            MixedReceipt::Download { row } => match operation {
                MixedFileOperation::Resume => row.can_resume(),
                MixedFileOperation::Cleanup => row.can_cleanup(),
                MixedFileOperation::Adopt => row.can_adopt(),
                _ => false,
            },
            _ => false,
        }
    }
}
