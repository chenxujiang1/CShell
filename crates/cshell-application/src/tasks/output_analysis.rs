//! Full stored-output observations; they never grant execution or repair authority.
use super::*;
pub const MAX_OUTPUT_ANALYSIS_DOCUMENT: usize = 64 * 1024;
pub const MAX_ANALYSIS_STREAM_BYTES: u64 = 1024 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputAnalysisKind {
    Task,
    Mixed,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputAnalysisRequest {
    pub format: String,
    pub version: u32,
    pub kind: OutputAnalysisKind,
    pub run_id: TaskRunId,
    pub left: HostId,
    pub left_revision: u64,
    pub right: Option<HostId>,
    pub right_revision: Option<u64>,
    pub step: u32,
    pub stderr: bool,
}
impl OutputAnalysisRequest {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.output-analysis-request"
            || self.version != 1
            || self.run_id.as_uuid().is_nil()
            || self.left.as_uuid().is_nil()
            || self.left_revision == 0
            || self.left_revision > i64::MAX as u64
            || self.step as usize >= MAX_TASK_STEPS
            || self.right.is_some() != self.right_revision.is_some()
            || self
                .right
                .is_some_and(|id| id.as_uuid().is_nil() || id == self.left)
            || self
                .right_revision
                .is_some_and(|revision| revision == 0 || revision > i64::MAX as u64)
            || self.kind == OutputAnalysisKind::Task
                && self
                    .right_revision
                    .is_some_and(|revision| revision != self.left_revision)
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputAnalysisBinding {
    pub child_id: TaskRunId,
    pub child_step: u32,
    pub snapshot_sha256: [u8; 32],
    pub target_state: TargetState,
    pub step_state: TargetState,
}
impl OutputAnalysisBinding {
    pub fn validate_for(&self, scope: &OutputAnalysisRequest) -> Result<(), TaskError> {
        scope.validate()?;
        if self.child_id.as_uuid().is_nil()
            || !target_terminal(self.target_state)
            || !target_terminal(self.step_state)
            || self.child_step
                != if scope.kind == OutputAnalysisKind::Mixed {
                    0
                } else {
                    scope.step
                }
            || scope.kind == OutputAnalysisKind::Task && self.child_id != scope.run_id
            || scope.kind == OutputAnalysisKind::Mixed && self.child_id == scope.run_id
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputAnalysisState {
    Running,
    Completed,
    Failed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputAnalysisError {
    Missing,
    Invalid,
    Changed,
    Storage,
    Timeout,
    Worker,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputStreamDigest {
    pub bytes: u64,
    pub sha256: [u8; 32],
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputAnalysisResult {
    pub left: OutputStreamDigest,
    pub right: Option<OutputStreamDigest>,
    pub equal: Option<bool>,
    /// Position-aligned unequal bytes plus bytes beyond the shorter stream. No text normalization.
    pub differing_bytes: Option<u64>,
    pub first_difference: Option<u64>,
}
impl OutputAnalysisResult {
    pub fn validate(&self, comparison: bool) -> Result<(), TaskError> {
        if self.left.bytes > MAX_ANALYSIS_STREAM_BYTES
            || self
                .right
                .as_ref()
                .is_some_and(|digest| digest.bytes > MAX_ANALYSIS_STREAM_BYTES)
            || self.right.is_some() != comparison
            || self.equal.is_some() != comparison
            || self.differing_bytes.is_some() != comparison
        {
            return Err(TaskError::Invalid);
        }
        if let Some(right) = &self.right {
            let maximum = self.left.bytes.max(right.bytes);
            let differing = self.differing_bytes.ok_or(TaskError::Invalid)?;
            if differing > maximum
                || differing < self.left.bytes.abs_diff(right.bytes)
                || self.equal != Some(differing == 0)
                || self.first_difference.is_some() != (differing != 0)
                || self
                    .first_difference
                    .is_some_and(|offset| offset >= maximum || differing > maximum - offset)
                || differing == 0
                    && (self.left.bytes != right.bytes || self.left.sha256 != right.sha256)
            {
                return Err(TaskError::Invalid);
            }
        } else if self.first_difference.is_some() {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputAnalysisDocument {
    pub format: String,
    pub version: u32,
    pub job_id: TaskRunId,
    pub request: OutputAnalysisRequest,
    pub left: OutputAnalysisBinding,
    pub right: Option<OutputAnalysisBinding>,
    pub state: OutputAnalysisState,
    pub left_bytes: u64,
    pub right_bytes: Option<u64>,
    pub result: Option<OutputAnalysisResult>,
    pub error: Option<OutputAnalysisError>,
}
impl OutputAnalysisDocument {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.request.validate()?;
        if self.format != "cshell.output-analysis"
            || self.version != 1
            || self.job_id.as_uuid().is_nil()
            || self.job_id == self.request.run_id
            || self.right.is_some() != self.request.right.is_some()
            || self.right_bytes.is_some() != self.request.right.is_some()
            || self.left_bytes > MAX_ANALYSIS_STREAM_BYTES
            || self
                .right_bytes
                .is_some_and(|bytes| bytes > MAX_ANALYSIS_STREAM_BYTES)
            || self.result.is_some() != (self.state == OutputAnalysisState::Completed)
            || self.error.is_some() != (self.state == OutputAnalysisState::Failed)
        {
            return Err(TaskError::Invalid);
        }
        for binding in std::iter::once(&self.left).chain(self.right.iter()) {
            if binding.child_id.as_uuid().is_nil()
                || !target_terminal(binding.target_state)
                || !target_terminal(binding.step_state)
                || binding.child_step
                    != if self.request.kind == OutputAnalysisKind::Mixed {
                        0
                    } else {
                        self.request.step
                    }
                || self.request.kind == OutputAnalysisKind::Task
                    && binding.child_id != self.request.run_id
                || self.request.kind == OutputAnalysisKind::Mixed
                    && binding.child_id == self.request.run_id
                || binding.snapshot_sha256 != self.left.snapshot_sha256
            {
                return Err(TaskError::Invalid);
            }
        }
        if self.request.kind == OutputAnalysisKind::Mixed
            && self
                .right
                .as_ref()
                .is_some_and(|binding| binding.child_id == self.left.child_id)
        {
            return Err(TaskError::Invalid);
        }
        if let Some(result) = &self.result {
            result.validate(self.request.right.is_some())?;
            if self.left_bytes != result.left.bytes
                || self.right_bytes != result.right.as_ref().map(|digest| digest.bytes)
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
}
pub fn decode_output_analysis_request(bytes: &[u8]) -> Result<OutputAnalysisRequest, TaskError> {
    let value: OutputAnalysisRequest = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
pub fn decode_output_analysis(bytes: &[u8]) -> Result<OutputAnalysisDocument, TaskError> {
    let value: OutputAnalysisDocument = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
