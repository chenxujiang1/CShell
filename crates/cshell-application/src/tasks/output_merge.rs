//! Per-command daemon recording order; no inferred remote or cross-host chronology.
use super::*;
use zeroize::Zeroize;
pub const MAX_OUTPUT_MERGE_EVENTS: u64 = 65536;
pub const MAX_OUTPUT_MERGE_PAGE: usize = 64;
pub const MAX_OUTPUT_MERGE_BYTES: usize = 64 * 1024;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputMergeRequest {
    pub format: String,
    pub version: u32,
    pub scope: OutputAnalysisRequest,
    pub after: u64,
    pub limit: usize,
    pub expected_journal_sha256: Option<[u8; 32]>,
}
impl OutputMergeRequest {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.scope.validate()?;
        if self.format != "cshell.output-merge-request"
            || self.version != 1
            || self.scope.right.is_some()
            || self.scope.stderr
            || self.after > MAX_OUTPUT_MERGE_EVENTS
            || self.limit == 0
            || self.limit > MAX_OUTPUT_MERGE_PAGE
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn next(&self, page: &OutputMergePage) -> Result<Option<Self>, TaskError> {
        self.validate()?;
        page.validate_metadata()?;
        if page.request != *self {
            return Err(TaskError::Invalid);
        }
        Ok(page.next.map(|after| {
            let mut next = self.clone();
            next.after = after;
            next.expected_journal_sha256 = Some(page.journal_sha256);
            next
        }))
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputMergeEvent {
    pub sequence: u64,
    pub recorded_at_ms: u64,
    pub stderr: bool,
    pub stream_offset: u64,
    pub bytes: u32,
    pub sha256: [u8; 32],
    pub page_offset: usize,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputMergePage {
    pub format: String,
    pub version: u32,
    pub request: OutputMergeRequest,
    pub binding: OutputAnalysisBinding,
    pub journal_sha256: [u8; 32],
    pub created_at_ms: u64,
    pub total_events: u64,
    pub sealed: bool,
    pub truncated: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub indexed_stdout_bytes: u64,
    pub indexed_stderr_bytes: u64,
    pub partial_tail_bytes: usize,
    pub next: Option<u64>,
    pub events: Vec<OutputMergeEvent>,
    pub body_bytes: usize,
}
impl OutputMergePage {
    pub fn validate_metadata(&self) -> Result<(), TaskError> {
        self.request.validate()?;
        self.binding.validate_for(&self.request.scope)?;
        if self.format != "cshell.output-merge"
            || self.version != 1
            || self.created_at_ms > i64::MAX as u64
            || self.total_events > MAX_OUTPUT_MERGE_EVENTS
            || self.request.after > self.total_events
            || self
                .stdout_bytes
                .checked_add(self.stderr_bytes)
                .is_none_or(|bytes| bytes > MAX_ANALYSIS_STREAM_BYTES)
            || self.indexed_stdout_bytes > self.stdout_bytes
            || self.indexed_stderr_bytes > self.stderr_bytes
            || self.truncated && !self.sealed
            || self.truncated && self.total_events != MAX_OUTPUT_MERGE_EVENTS
            || self.truncated
                && self.indexed_stdout_bytes == self.stdout_bytes
                && self.indexed_stderr_bytes == self.stderr_bytes
            || self.sealed
                && !self.truncated
                && (self.indexed_stdout_bytes != self.stdout_bytes
                    || self.indexed_stderr_bytes != self.stderr_bytes)
            || self.partial_tail_bytes >= 72
            || self.sealed && self.partial_tail_bytes != 0
            || self.total_events == 0
                && (self.indexed_stdout_bytes != 0 || self.indexed_stderr_bytes != 0)
            || self.events.len() > self.request.limit
            || self.body_bytes > MAX_OUTPUT_MERGE_BYTES
            || self
                .request
                .expected_journal_sha256
                .is_some_and(|hash| hash != self.journal_sha256)
        {
            return Err(TaskError::Invalid);
        }
        let mut sequence = self.request.after;
        let mut bytes = 0_usize;
        let mut time = self.created_at_ms;
        let mut offsets = if sequence == 0 {
            [Some(0), Some(0)]
        } else {
            [None, None]
        };
        for event in &self.events {
            sequence += 1;
            let stream = usize::from(event.stderr);
            let indexed = if event.stderr {
                self.indexed_stderr_bytes
            } else {
                self.indexed_stdout_bytes
            };
            if event.sequence != sequence
                || event.sequence > self.total_events
                || event.recorded_at_ms < time
                || event.recorded_at_ms > i64::MAX as u64
                || event.page_offset != bytes
                || event.bytes == 0
                || event.bytes as usize > MAX_OUTPUT_MERGE_BYTES
                || event
                    .stream_offset
                    .checked_add(event.bytes as u64)
                    .is_none_or(|end| end > indexed)
                || offsets[stream].is_some_and(|offset| event.stream_offset != offset)
            {
                return Err(TaskError::Invalid);
            }
            time = event.recorded_at_ms;
            offsets[stream] = Some(event.stream_offset + event.bytes as u64);
            bytes += event.bytes as usize;
        }
        if bytes != self.body_bytes
            || self.events.is_empty() != (self.request.after == self.total_events)
            || self.next != (sequence < self.total_events).then_some(sequence)
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn validate(&self, body: &[u8]) -> Result<(), TaskError> {
        self.validate_metadata()?;
        if body.len() != self.body_bytes {
            return Err(TaskError::Invalid);
        }
        for event in &self.events {
            let digest: [u8; 32] =
                Sha256::digest(&body[event.page_offset..event.page_offset + event.bytes as usize])
                    .into();
            if digest != event.sha256 {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
}
pub fn decode_output_merge_request(bytes: &[u8]) -> Result<OutputMergeRequest, TaskError> {
    let value: OutputMergeRequest = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
pub fn decode_output_merge(bytes: &[u8], body: &[u8]) -> Result<OutputMergePage, TaskError> {
    let value: OutputMergePage = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate(body)?;
    Ok(value)
}
pub struct OutputMergedBody(pub Vec<u8>);
impl Drop for OutputMergedBody {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
