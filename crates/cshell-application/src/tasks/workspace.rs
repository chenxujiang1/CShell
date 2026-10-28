//! Version-bound aggregate output work. No operation grants execution authority.
use super::*;
pub const MAX_WORKSPACE_SOURCES: usize = 4096;
pub const MAX_WORKSPACE_DOCUMENT: usize = 8 * 1024 * 1024;
pub const MAX_WORKSPACE_RESULT: usize = 64 * 1024 * 1024;
pub const WORKSPACE_PAGE_ROWS: usize = 64;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputWorkspaceAction {
    Index,
    Search,
    TextDiff,
    Bundle,
    RetentionPreview,
    RetentionApply,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputWorkspaceRequest {
    pub format: String,
    pub version: u32,
    pub action: OutputWorkspaceAction,
    pub sources: Vec<OutputAnalysisRequest>,
    pub query: Option<String>,
    pub retention: Option<OutputRetentionPlan>,
}
impl fmt::Debug for OutputWorkspaceRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputWorkspaceRequest")
            .field("action", &self.action)
            .field("sources", &self.sources.len())
            .field("query", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}
impl OutputWorkspaceRequest {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.output-workspace-request"
            || self.version != 1
            || self.sources.is_empty()
            || self.sources.len() > MAX_WORKSPACE_SOURCES
            || self.query.is_some() != (self.action == OutputWorkspaceAction::Search)
            || self
                .query
                .as_ref()
                .is_some_and(|q| q.is_empty() || q.len() > 4096)
            || self.retention.is_some() != (self.action == OutputWorkspaceAction::RetentionApply)
            || self.action == OutputWorkspaceAction::TextDiff && self.sources.len() != 2
        {
            return Err(TaskError::Invalid);
        }
        let mut keys = BTreeSet::new();
        for source in &self.sources {
            source.validate()?;
            if source.run_id != self.sources[0].run_id
                || source.kind != self.sources[0].kind
                || source.right.is_some()
                || !keys.insert((source.left, source.step, source.stderr))
                || matches!(
                    self.action,
                    OutputWorkspaceAction::Bundle
                        | OutputWorkspaceAction::RetentionPreview
                        | OutputWorkspaceAction::RetentionApply
                ) && source.stderr
            {
                return Err(TaskError::Invalid);
            }
        }
        if let Some(plan) = &self.retention {
            plan.validate()?;
            if plan.sources != self.sources {
                return Err(TaskError::Invalid);
            }
        }
        encode(self, MAX_WORKSPACE_DOCUMENT)?;
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputIndexEntry {
    pub source: usize,
    pub binding: OutputAnalysisBinding,
    pub digest: OutputStreamDigest,
    pub lines: u64,
    pub indexed_lines: usize,
    pub index_sha256: [u8; 32],
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDigestGroup {
    pub digest: OutputStreamDigest,
    pub sources: Vec<usize>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputWorkspaceRow {
    Index {
        entry: OutputIndexEntry,
    },
    Match {
        source: usize,
        offset: u64,
        bytes: usize,
        line: u64,
    },
    Diff {
        row: TextDiffRow,
    },
    Bundle {
        manifest: OutputArchiveManifest,
    },
    Retained {
        source: usize,
        removed: bool,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRetentionEntry {
    pub binding: OutputAnalysisBinding,
    pub stdout: OutputStreamDigest,
    pub stderr: OutputStreamDigest,
    pub journal: Option<OutputStreamDigest>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRetentionPlan {
    pub format: String,
    pub version: u32,
    pub id: TaskRunId,
    pub sources: Vec<OutputAnalysisRequest>,
    pub entries: Vec<OutputRetentionEntry>,
    pub created_at_ms: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputRetentionState {
    Pending,
    Quarantined,
    Deleted,
    Changed,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRetentionReceipt {
    pub format: String,
    pub version: u32,
    pub plan: OutputRetentionPlan,
    pub states: Vec<OutputRetentionState>,
    pub updated_at_ms: u64,
}
impl OutputRetentionReceipt {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.plan.validate()?;
        if self.format != "cshell.output-retention-receipt"
            || self.version != 1
            || self.states.len() != self.plan.entries.len()
            || self.updated_at_ms < self.plan.created_at_ms
            || self.updated_at_ms > i64::MAX as u64
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
impl OutputRetentionPlan {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.output-retention"
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || self.sources.is_empty()
            || self.sources.len() > MAX_WORKSPACE_SOURCES
            || self.entries.len() != self.sources.len()
            || self.created_at_ms > i64::MAX as u64
        {
            return Err(TaskError::Invalid);
        }
        for (scope, entry) in self.sources.iter().zip(&self.entries) {
            scope.validate()?;
            entry.binding.validate_for(scope)?;
            if scope.stderr
                || scope.right.is_some()
                || entry.stdout.bytes > MAX_ANALYSIS_STREAM_BYTES
                || entry.stderr.bytes > MAX_ANALYSIS_STREAM_BYTES
                || entry
                    .journal
                    .as_ref()
                    .is_some_and(|j| j.bytes > MAX_ARCHIVE_JOURNAL_BYTES)
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputWorkspaceDocument {
    pub format: String,
    pub version: u32,
    pub job_id: TaskRunId,
    pub action: OutputWorkspaceAction,
    pub sources: Vec<OutputAnalysisRequest>,
    pub bindings: Vec<OutputAnalysisBinding>,
    pub query_sha256: Option<[u8; 32]>,
    pub state: OutputAnalysisState,
    pub completed_sources: usize,
    pub bytes: u64,
    pub rows: usize,
    pub result_sha256: Option<[u8; 32]>,
    pub groups: Vec<OutputDigestGroup>,
    pub retention: Option<OutputRetentionPlan>,
    pub artifact: Option<OutputBundleArtifact>,
    pub truncated: bool,
    pub error: Option<OutputAnalysisError>,
}
impl OutputWorkspaceDocument {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.output-workspace"
            || self.version != 1
            || self.job_id.as_uuid().is_nil()
            || self.sources.is_empty()
            || self.sources.len() > MAX_WORKSPACE_SOURCES
            || self.bindings.len() != self.sources.len()
            || self.completed_sources > self.sources.len()
            || self.error.is_some() != (self.state == OutputAnalysisState::Failed)
            || self.result_sha256.is_some() != (self.state == OutputAnalysisState::Completed)
            || self.query_sha256.is_some() != (self.action == OutputWorkspaceAction::Search)
        {
            return Err(TaskError::Invalid);
        }
        for (source, binding) in self.sources.iter().zip(&self.bindings) {
            binding.validate_for(source)?;
            if source.run_id != self.sources[0].run_id
                || source.kind != self.sources[0].kind
                || source.right.is_some()
            {
                return Err(TaskError::Invalid);
            }
        }
        let complete = self.state == OutputAnalysisState::Completed;
        if complete && self.completed_sources != self.sources.len()
            || self.artifact.is_some() != (complete && self.action == OutputWorkspaceAction::Bundle)
            || self.retention.is_some()
                != (complete
                    && matches!(
                        self.action,
                        OutputWorkspaceAction::RetentionPreview
                            | OutputWorkspaceAction::RetentionApply
                    ))
            || self.rows > 100_000
        {
            return Err(TaskError::Invalid);
        }
        let mut grouped = BTreeSet::new();
        for group in &self.groups {
            if group.digest.bytes > MAX_ANALYSIS_STREAM_BYTES
                || group.sources.is_empty()
                || group
                    .sources
                    .iter()
                    .any(|index| *index >= self.sources.len() || !grouped.insert(*index))
            {
                return Err(TaskError::Invalid);
            }
        }
        if complete
            && matches!(
                self.action,
                OutputWorkspaceAction::Index
                    | OutputWorkspaceAction::Search
                    | OutputWorkspaceAction::TextDiff
            )
            && grouped.len() != self.sources.len()
        {
            return Err(TaskError::Invalid);
        }
        if let Some(plan) = &self.retention {
            plan.validate()?;
            if plan.sources != self.sources {
                return Err(TaskError::Invalid);
            }
            if plan
                .entries
                .iter()
                .zip(&self.bindings)
                .any(|(entry, binding)| &entry.binding != binding)
            {
                return Err(TaskError::Invalid);
            }
        }
        if let Some(artifact) = &self.artifact {
            artifact.validate()?;
            if artifact.manifest.entries.len() != self.sources.len()
                || artifact
                    .manifest
                    .entries
                    .iter()
                    .zip(self.sources.iter().zip(&self.bindings))
                    .any(|(entry, (source, binding))| {
                        entry.manifest.scope != *source || entry.manifest.binding != *binding
                    })
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputWorkspacePage {
    pub format: String,
    pub version: u32,
    pub job_id: TaskRunId,
    pub result_sha256: [u8; 32],
    pub offset: usize,
    pub next: usize,
    pub total: usize,
    pub rows: Vec<OutputWorkspaceRow>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputIndexPage {
    pub format: String,
    pub version: u32,
    pub entry: OutputIndexEntry,
    pub scope: OutputAnalysisRequest,
    pub offset: usize,
    pub next: usize,
    pub positions: Vec<u64>,
}
impl OutputIndexPage {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.scope.validate()?;
        self.entry.binding.validate_for(&self.scope)?;
        if self.format != "cshell.output-index-page"
            || self.version != 1
            || self.entry.digest.bytes > MAX_ANALYSIS_STREAM_BYTES
            || self.entry.indexed_lines > 131_072
            || self.offset > self.entry.indexed_lines
            || self.positions.len() > WORKSPACE_PAGE_ROWS
            || self.next != self.offset + self.positions.len()
            || self.next > self.entry.indexed_lines
            || self.positions.windows(2).any(|p| p[0] >= p[1])
            || self
                .positions
                .iter()
                .any(|offset| *offset >= self.entry.digest.bytes)
            || self.positions.is_empty() && self.offset != self.entry.indexed_lines
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
impl OutputWorkspacePage {
    pub fn validate(&self, document: &OutputWorkspaceDocument) -> Result<(), TaskError> {
        document.validate()?;
        for row in &self.rows {
            match row {
                OutputWorkspaceRow::Index { entry }
                    if document.action == OutputWorkspaceAction::Index =>
                {
                    if entry.source >= document.sources.len()
                        || entry.binding != document.bindings[entry.source]
                        || entry.digest.bytes > MAX_ANALYSIS_STREAM_BYTES
                        || entry.indexed_lines > 131_072
                        || entry.indexed_lines as u64 > entry.lines
                    {
                        return Err(TaskError::Invalid);
                    }
                }
                OutputWorkspaceRow::Match {
                    source,
                    offset,
                    bytes,
                    line,
                } if document.action == OutputWorkspaceAction::Search => {
                    if *source >= document.sources.len()
                        || *offset >= MAX_ANALYSIS_STREAM_BYTES
                        || *bytes == 0
                        || *bytes > 4096
                        || *line == 0
                    {
                        return Err(TaskError::Invalid);
                    }
                }
                OutputWorkspaceRow::Diff { row }
                    if document.action == OutputWorkspaceAction::TextDiff =>
                {
                    if row.text.len() > 1024 * 1024
                        || row.left_line.is_some() != row.left_offset.is_some()
                        || row.right_line.is_some() != row.right_offset.is_some()
                        || match row.kind {
                            TextDiffKind::Equal => {
                                row.left_line.is_none() || row.right_line.is_none()
                            }
                            TextDiffKind::Delete => {
                                row.left_line.is_none() || row.right_line.is_some()
                            }
                            TextDiffKind::Insert => {
                                row.left_line.is_some() || row.right_line.is_none()
                            }
                        }
                    {
                        return Err(TaskError::Invalid);
                    }
                }
                OutputWorkspaceRow::Bundle { manifest }
                    if document.action == OutputWorkspaceAction::Bundle =>
                {
                    manifest.validate()?;
                }
                OutputWorkspaceRow::Retained { source, removed }
                    if document.action == OutputWorkspaceAction::RetentionApply =>
                {
                    if *source >= document.sources.len() || !removed {
                        return Err(TaskError::Invalid);
                    }
                }
                _ => return Err(TaskError::Invalid),
            }
        }
        if self.format != "cshell.output-workspace-page"
            || self.version != 1
            || document.state != OutputAnalysisState::Completed
            || self.job_id != document.job_id
            || Some(self.result_sha256) != document.result_sha256
            || self.total != document.rows
            || self.offset > self.total
            || self.next != self.offset + self.rows.len()
            || self.next > self.total
            || self.rows.len() > WORKSPACE_PAGE_ROWS
            || self.rows.is_empty() && self.offset != self.total
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
pub fn decode_output_workspace_request(bytes: &[u8]) -> Result<OutputWorkspaceRequest, TaskError> {
    let value: OutputWorkspaceRequest = decode(bytes, MAX_WORKSPACE_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
pub fn decode_output_workspace(bytes: &[u8]) -> Result<OutputWorkspaceDocument, TaskError> {
    let value: OutputWorkspaceDocument = decode(bytes, MAX_WORKSPACE_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
