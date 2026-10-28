//! Durable downloads keep remote reads separate from local publication receipts.
mod runner;
use super::*;
pub use runner::*;
use std::path::{Path, PathBuf};

pub const MAX_DOWNLOAD_FILE_BYTES: u64 = MAX_UPLOAD_FILE_BYTES;
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchDownloadDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_policy: Option<FilePublicationPolicy>,
    pub name: String,
    pub remote_source: String,
    pub local_directory: String,
    pub local_name: String,
    pub maximum_bytes: u64,
    pub concurrency: u16,
    pub queue_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub transfer_timeout_ms: u64,
}
impl fmt::Debug for BatchDownloadDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchDownloadDefinition")
            .field("maximum_bytes", &self.maximum_bytes)
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}
impl BatchDownloadDefinition {
    pub fn destination(&self, host: HostId) -> Result<PathBuf, TaskError> {
        self.validate()?;
        if host.as_uuid().is_nil() {
            return Err(TaskError::UnknownTarget);
        }
        Ok(Path::new(&self.local_directory).join(format!("{host}-{}", self.local_name)))
    }
    fn local_paths(&self, run: TaskRunId, host: HostId) -> Result<(PathBuf, PathBuf), TaskError> {
        if run.as_uuid().is_nil() {
            return Err(TaskError::Invalid);
        }
        let destination = self.destination(host)?;
        Ok((
            Path::new(&self.local_directory).join(format!(".cshell-download-{run}-{host}.part")),
            destination,
        ))
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        if !label(&self.name)
            || !upload_remote_path(&self.remote_source)
            || !Path::new(&self.local_directory).is_absolute()
            || self.local_directory.len() > 3500
            || self.local_directory.chars().any(char::is_control)
            || self.local_name.is_empty()
            || self.local_name.len() > 128
            || !self
                .local_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            || self.local_name == "."
            || self.local_name == ".."
            || self.local_name.ends_with('.')
            || self.maximum_bytes > MAX_DOWNLOAD_FILE_BYTES
            || self.concurrency == 0
            || self.concurrency > 50
            || [
                self.queue_timeout_ms,
                self.connect_timeout_ms,
                self.transfer_timeout_ms,
            ]
            .iter()
            .any(|value| *value == 0 || *value > MAX_TIMEOUT_MS)
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchDownloadSnapshot {
    pub format: String,
    pub version: u32,
    pub id: TaskRunId,
    pub definition: BatchDownloadDefinition,
    pub targets: FrozenHostTargets,
    pub created_at_ms: u64,
}
impl fmt::Debug for BatchDownloadSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchDownloadSnapshot")
            .field("id", &self.id)
            .field("targets", &self.targets.targets().len())
            .finish_non_exhaustive()
    }
}
impl BatchDownloadSnapshot {
    pub fn new(
        definition: BatchDownloadDefinition,
        targets: FrozenHostTargets,
        now: u64,
    ) -> Result<Self, TaskError> {
        let result = Self {
            format: "cshell.batch-download".into(),
            version: 1,
            id: TaskRunId::new(),
            definition,
            targets,
            created_at_ms: now,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        self.definition.validate()?;
        if self.format != "cshell.batch-download"
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || self.created_at_ms > i64::MAX as u64 - MAX_TIMEOUT_MS
            || self.targets.targets().is_empty()
            || self.targets.targets().len() > crate::MAX_HOST_TARGETS
        {
            return Err(TaskError::Invalid);
        }
        encode(self, MAX_TASK_SNAPSHOT_BYTES)?;
        Ok(())
    }
    pub fn local_paths(&self, host: HostId) -> Result<(PathBuf, PathBuf), TaskError> {
        if !self
            .targets
            .targets()
            .iter()
            .any(|target| target.id == host)
        {
            return Err(TaskError::UnknownTarget);
        }
        self.definition.local_paths(self.id, host)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadPhase {
    Pending,
    Inspecting,
    Creating,
    Writing,
    Verifying,
    PublishIntent,
    Committed,
    CleanupRequired,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}
impl DownloadPhase {
    #[must_use]
    pub const fn active(self) -> bool {
        matches!(
            self,
            Self::Inspecting
                | Self::Creating
                | Self::Writing
                | Self::Verifying
                | Self::PublishIntent
        )
    }
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadIntent {
    None,
    Inspect,
    Create,
    Write,
    Verify,
    Publish,
    Cleanup,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadReason {
    Unsupported,
    Connection,
    SourceChanged,
    DestinationExists,
    LocalIo,
    Transport,
    Checksum,
    Storage,
    Cancelled,
    Timeout,
    Cleanup,
    Restarted,
    Invalid,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadContent {
    pub size: u64,
    pub sha256: [u8; 32],
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<FilePublicationReceipt>,
    pub run_id: TaskRunId,
    pub host_id: HostId,
    pub revision: u64,
    pub phase: DownloadPhase,
    pub intent: DownloadIntent,
    pub bytes: u64,
    pub content: Option<DownloadContent>,
    pub temporary_owned: bool,
    pub verified: bool,
    pub committed: bool,
    pub adopted: bool,
    pub temporary_cleaned: bool,
    pub reason: Option<DownloadReason>,
    pub updated_at_ms: u64,
}
impl DownloadTarget {
    #[must_use]
    pub fn can_resume(&self) -> bool {
        self.phase == DownloadPhase::Unknown
            && self.temporary_owned
            && !self.temporary_cleaned
            && !self.verified
            && !self.committed
            && self.content.is_some()
            && self.intent != DownloadIntent::Publish
    }
    #[must_use]
    pub fn can_cleanup(&self) -> bool {
        self.temporary_owned
            && !self.temporary_cleaned
            && matches!(
                self.phase,
                DownloadPhase::Failed
                    | DownloadPhase::Cancelled
                    | DownloadPhase::Committed
                    | DownloadPhase::CleanupRequired
                    | DownloadPhase::Unknown
            )
            && !(self.phase == DownloadPhase::Unknown && self.intent == DownloadIntent::Publish)
    }
    #[must_use]
    pub fn can_adopt(&self) -> bool {
        self.phase == DownloadPhase::Unknown
            && self.intent == DownloadIntent::Publish
            && self.temporary_owned
            && !self.temporary_cleaned
            && self.verified
            && !self.committed
            && self.content.is_some()
    }
    #[must_use]
    pub fn initial(snapshot: &BatchDownloadSnapshot, host: HostId) -> Self {
        Self {
            run_id: snapshot.id,
            publication: None,
            host_id: host,
            revision: 1,
            phase: DownloadPhase::Pending,
            intent: DownloadIntent::None,
            bytes: 0,
            content: None,
            temporary_owned: false,
            verified: false,
            committed: false,
            adopted: false,
            temporary_cleaned: false,
            reason: None,
            updated_at_ms: snapshot.created_at_ms,
        }
    }
    pub fn validate(&self, snapshot: &BatchDownloadSnapshot) -> Result<(), TaskError> {
        if let Some(receipt) = &self.publication {
            receipt.validate()?;
            if !self.committed && (receipt.skipped_identical || receipt.backup_verified) {
                return Err(TaskError::Invalid);
            }
            let policy = snapshot
                .definition
                .publication_policy
                .ok_or(TaskError::Invalid)?;
            if self.committed {
                let content = self.content.ok_or(TaskError::Invalid)?;
                receipt.validate_commit(
                    policy,
                    UploadContent {
                        size: content.size,
                        sha256: content.sha256,
                    },
                )?;
            }
        }
        if snapshot.definition.publication_policy.is_some()
            && self.temporary_owned
            && self.publication.is_none()
        {
            return Err(TaskError::Invalid);
        }
        if snapshot
            .targets
            .targets()
            .binary_search_by_key(&self.host_id, |target| target.id)
            .is_err()
        {
            return Err(TaskError::Invalid);
        }
        self.validate_state(
            snapshot.id,
            snapshot.created_at_ms,
            snapshot.definition.maximum_bytes,
        )
    }
    pub(super) fn validate_state(
        &self,
        id: TaskRunId,
        created_at_ms: u64,
        maximum: u64,
    ) -> Result<(), TaskError> {
        if self.run_id != id
            || self.host_id.as_uuid().is_nil()
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.updated_at_ms < created_at_ms
            || self.updated_at_ms > i64::MAX as u64
            || self.content.is_some_and(|value| value.size > maximum)
            || self.bytes > self.content.map_or(0, |value| value.size)
            || self.temporary_owned && self.content.is_none()
            || self.verified
                && (!self.temporary_owned
                    || self.content.is_none_or(|value| self.bytes != value.size))
            || self.committed && !self.verified
            || self.adopted && !self.committed
            || self.temporary_cleaned && !self.temporary_owned
            || self.phase == DownloadPhase::Succeeded
                && (!self.committed || !self.temporary_cleaned)
            || matches!(
                self.phase,
                DownloadPhase::Committed | DownloadPhase::CleanupRequired
            ) && !self.temporary_owned
            || self.phase == DownloadPhase::Committed && !self.committed
            || self.phase == DownloadPhase::PublishIntent
                && (!self.verified || self.intent != DownloadIntent::Publish)
            || self.phase == DownloadPhase::Pending
                && (self.intent != DownloadIntent::None
                    || self.content.is_some()
                    || self.bytes != 0
                    || self.temporary_owned)
            || self.bytes != 0 && !self.temporary_owned
            || self.committed
                && !matches!(
                    self.phase,
                    DownloadPhase::Committed
                        | DownloadPhase::CleanupRequired
                        | DownloadPhase::Succeeded
                )
            || self.phase == DownloadPhase::Inspecting
                && (self.intent != DownloadIntent::Inspect
                    || self.content.is_some()
                    || self.temporary_owned)
            || self.phase == DownloadPhase::Creating
                && (self.intent != DownloadIntent::Create
                    || self.content.is_none()
                    || self.temporary_owned)
            || self.phase == DownloadPhase::Writing
                && (self.intent != DownloadIntent::Write || !self.temporary_owned || self.verified)
            || self.phase == DownloadPhase::Verifying
                && (self.intent != DownloadIntent::Verify
                    || !self.temporary_owned
                    || self.content.is_none_or(|value| value.size != self.bytes))
            || matches!(
                self.phase,
                DownloadPhase::CleanupRequired | DownloadPhase::Succeeded
            ) && self.intent != DownloadIntent::Cleanup
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn validate_next(
        &self,
        next: &Self,
        snapshot: &BatchDownloadSnapshot,
    ) -> Result<(), TaskError> {
        self.validate(snapshot)?;
        next.validate(snapshot)?;
        match (&self.publication, &next.publication) {
            (Some(old), Some(new)) => old.validate_next(new)?,
            (Some(_), None) => return Err(TaskError::Transition),
            (None, Some(_))
                if self.phase != DownloadPhase::Inspecting
                    || next.phase != DownloadPhase::Creating =>
            {
                return Err(TaskError::Transition);
            }
            _ => {}
        }
        if next.run_id != self.run_id
            || next.host_id != self.host_id
            || next.revision != self.revision.checked_add(1).ok_or(TaskError::Limit)?
            || next.updated_at_ms < self.updated_at_ms
            || next.bytes < self.bytes
            || self.content.is_some() && next.content != self.content
            || self.temporary_owned && !next.temporary_owned
            || self.verified && !next.verified
            || self.committed && !next.committed
            || self.adopted && !next.adopted
            || self.temporary_cleaned && !next.temporary_cleaned
            || self.content.is_none()
                && next.content.is_some()
                && !(self.phase == DownloadPhase::Inspecting
                    && next.phase == DownloadPhase::Creating)
            || !self.temporary_owned
                && next.temporary_owned
                && !(self.phase == DownloadPhase::Creating && next.phase == DownloadPhase::Writing)
            || !self.verified
                && next.verified
                && !(self.phase == DownloadPhase::Verifying
                    && next.phase == DownloadPhase::PublishIntent)
            || !self.committed
                && next.committed
                && !(next.phase == DownloadPhase::Committed
                    && (self.phase == DownloadPhase::PublishIntent
                        || self.phase == DownloadPhase::Unknown && next.adopted))
            || !self.adopted
                && next.adopted
                && !(self.phase == DownloadPhase::Unknown && next.phase == DownloadPhase::Committed)
            || !self.temporary_cleaned
                && next.temporary_cleaned
                && !(self.phase == DownloadPhase::CleanupRequired
                    && matches!(next.phase, DownloadPhase::Succeeded | DownloadPhase::Failed))
        {
            return Err(TaskError::Transition);
        }
        let allowed = match self.phase {
            DownloadPhase::Pending => matches!(
                next.phase,
                DownloadPhase::Inspecting | DownloadPhase::Cancelled | DownloadPhase::Failed
            ),
            DownloadPhase::Inspecting => matches!(
                next.phase,
                DownloadPhase::Creating | DownloadPhase::Failed | DownloadPhase::Unknown
            ),
            DownloadPhase::Creating => matches!(
                next.phase,
                DownloadPhase::Writing | DownloadPhase::Failed | DownloadPhase::Unknown
            ),
            DownloadPhase::Writing => matches!(
                next.phase,
                DownloadPhase::Writing
                    | DownloadPhase::Verifying
                    | DownloadPhase::Failed
                    | DownloadPhase::Unknown
            ),
            DownloadPhase::Verifying => matches!(
                next.phase,
                DownloadPhase::PublishIntent | DownloadPhase::Failed | DownloadPhase::Unknown
            ),
            DownloadPhase::PublishIntent => matches!(
                next.phase,
                DownloadPhase::Committed | DownloadPhase::Failed | DownloadPhase::Unknown
            ),
            DownloadPhase::Committed => matches!(
                next.phase,
                DownloadPhase::CleanupRequired | DownloadPhase::Succeeded
            ),
            DownloadPhase::CleanupRequired => matches!(
                next.phase,
                DownloadPhase::CleanupRequired | DownloadPhase::Succeeded | DownloadPhase::Failed
            ),
            DownloadPhase::Unknown => {
                next.phase == DownloadPhase::Unknown
                    || next.phase == DownloadPhase::Writing
                        && self.temporary_owned
                        && !self.verified
                        && !self.committed
                        && self.intent != DownloadIntent::Publish
                    || next.phase == DownloadPhase::Committed
                        && self.verified
                        && self.intent == DownloadIntent::Publish
                        && next.adopted
                    || next.phase == DownloadPhase::CleanupRequired
                        && self.temporary_owned
                        && self.intent != DownloadIntent::Publish
            }
            DownloadPhase::Failed | DownloadPhase::Cancelled => {
                next.phase == DownloadPhase::CleanupRequired
                    && self.temporary_owned
                    && !self.temporary_cleaned
            }
            DownloadPhase::Succeeded => false,
        };
        if !allowed {
            return Err(TaskError::Transition);
        }
        encode(next, MAX_UPLOAD_ROW_BYTES)?;
        Ok(())
    }
}
pub fn encode_download_snapshot(snapshot: &BatchDownloadSnapshot) -> Result<Vec<u8>, TaskError> {
    snapshot.validate()?;
    encode(snapshot, MAX_TASK_SNAPSHOT_BYTES)
}
pub fn decode_download_snapshot(bytes: &[u8]) -> Result<BatchDownloadSnapshot, TaskError> {
    let value: BatchDownloadSnapshot = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    value.validate()?;
    Ok(value)
}
pub fn encode_download_target(
    row: &DownloadTarget,
    snapshot: &BatchDownloadSnapshot,
) -> Result<Vec<u8>, TaskError> {
    row.validate(snapshot)?;
    encode(row, MAX_UPLOAD_ROW_BYTES)
}
pub fn decode_download_target(
    bytes: &[u8],
    snapshot: &BatchDownloadSnapshot,
) -> Result<DownloadTarget, TaskError> {
    let value: DownloadTarget = decode(bytes, MAX_UPLOAD_ROW_BYTES)?;
    value.validate(snapshot)?;
    Ok(value)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadRunSummary {
    pub id: TaskRunId,
    pub name: String,
    pub created_at_ms: u64,
    pub targets: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadProbe {
    pub temporary: Option<DownloadContent>,
    pub destination: Option<DownloadContent>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadRecoveryOperation {
    Probe,
    Cleanup,
    Adopt,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadRecoveryJob {
    pub host_id: HostId,
    pub row_revision: u64,
    pub operation: DownloadRecoveryOperation,
    pub status: FileRecoveryStatus,
    pub probe: Option<DownloadProbe>,
    pub error: Option<DownloadReason>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadStatusTarget {
    pub host_id: HostId,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub row: DownloadTarget,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadStatusDocument {
    pub format: String,
    pub version: u32,
    pub id: TaskRunId,
    pub definition: BatchDownloadDefinition,
    pub created_at_ms: u64,
    pub host_revision: u64,
    pub profile_revision: u64,
    pub active: bool,
    pub targets: Vec<DownloadStatusTarget>,
    pub recovery: Option<DownloadRecoveryJob>,
}
impl fmt::Debug for DownloadStatusDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadStatusDocument")
            .field("id", &self.id)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}
impl DownloadStatusDocument {
    /// A content observation only, never proof that this run published the file.
    #[must_use]
    pub fn has_matching_destination_probe(&self, host: HostId) -> bool {
        let Some(target) = self.target(host) else {
            return false;
        };
        self.recovery.as_ref().is_some_and(|job| {
            job.operation == DownloadRecoveryOperation::Probe
                && job.status == FileRecoveryStatus::Completed
                && job.host_id == host
                && job.row_revision == target.row.revision
                && target.row.content.is_some()
                && job
                    .probe
                    .as_ref()
                    .is_some_and(|probe| probe.destination == target.row.content)
        })
    }
    #[must_use]
    pub fn target(&self, host: HostId) -> Option<&DownloadStatusTarget> {
        self.targets
            .binary_search_by_key(&host, |target| target.host_id)
            .ok()
            .map(|index| &self.targets[index])
    }
    pub fn local_paths(&self, host: HostId) -> Result<(PathBuf, PathBuf), TaskError> {
        if self.target(host).is_none() {
            return Err(TaskError::UnknownTarget);
        }
        self.definition.local_paths(self.id, host)
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        self.definition.validate()?;
        if self.format != "cshell.batch-download-status"
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || self.created_at_ms > i64::MAX as u64 - MAX_TIMEOUT_MS
            || self.targets.is_empty()
            || self.targets.len() > crate::MAX_HOST_TARGETS
            || self
                .targets
                .windows(2)
                .any(|pair| pair[0].host_id >= pair[1].host_id)
        {
            return Err(TaskError::Invalid);
        }
        for target in &self.targets {
            target.row.validate_state(
                self.id,
                self.created_at_ms,
                self.definition.maximum_bytes,
            )?;
            if target.host_id != target.row.host_id
                || !label(&target.name)
                || target.address.is_empty()
                || target.address.len() > 255
                || target.address.chars().any(char::is_control)
                || target.port == 0
            {
                return Err(TaskError::Invalid);
            }
        }
        if let Some(job) = &self.recovery {
            if job.row_revision == 0
                || job.row_revision > i64::MAX as u64
                || !self
                    .targets
                    .iter()
                    .any(|target| target.host_id == job.host_id)
                || job.status == FileRecoveryStatus::Running
                    && (!self.active || job.probe.is_some() || job.error.is_some())
                || job.status == FileRecoveryStatus::Completed
                    && (job.error.is_some()
                        || (job.operation == DownloadRecoveryOperation::Probe)
                            != job.probe.is_some())
                || job.status == FileRecoveryStatus::Failed
                    && (job.error.is_none() || job.probe.is_some())
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
}
pub fn decode_download_status(bytes: &[u8]) -> Result<DownloadStatusDocument, TaskError> {
    let value: DownloadStatusDocument = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    value.validate()?;
    Ok(value)
}
pub fn decode_download_list(bytes: &[u8]) -> Result<Vec<DownloadRunSummary>, TaskError> {
    let rows: Vec<DownloadRunSummary> = decode(bytes, 256 * 1024)?;
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
#[async_trait]
pub trait DownloadRepository: fmt::Debug + Send + Sync {
    async fn create_download(
        &self,
        snapshot: &BatchDownloadSnapshot,
    ) -> Result<(), TaskRepositoryError>;
    async fn load_download(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchDownloadSnapshot, Vec<DownloadTarget>)>, TaskRepositoryError>;
    async fn commit_download_target(
        &self,
        expected: &DownloadTarget,
        next: &DownloadTarget,
    ) -> Result<(), TaskRepositoryError>;
    async fn list_downloads(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<DownloadRunSummary>, TaskRepositoryError>;
}
#[async_trait]
pub trait DownloadRemote: fmt::Debug + Send {
    async fn inspect(
        &mut self,
        source: &str,
        maximum: u64,
    ) -> Result<DownloadContent, DownloadReason>;
    async fn read(&mut self, offset: u64, limit: usize) -> Result<Vec<u8>, DownloadReason>;
    async fn recheck(&mut self) -> Result<DownloadContent, DownloadReason>;
    async fn close(&mut self);
}
#[async_trait]
pub trait DownloadLocal: fmt::Debug + Send {
    async fn plan_publication(
        &mut self,
        policy: Option<FilePublicationPolicy>,
        expected: DownloadContent,
    ) -> Result<Option<FilePublicationReceipt>, DownloadReason> {
        let _ = expected;
        if policy.is_some() {
            Err(DownloadReason::Unsupported)
        } else {
            Ok(None)
        }
    }
    async fn publish_planned(
        &mut self,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: DownloadContent,
    ) -> Result<Option<FilePublicationReceipt>, DownloadReason> {
        let _ = (planned, expected);
        if policy.is_some() {
            return Err(DownloadReason::Unsupported);
        }
        self.publish().await?;
        Ok(None)
    }
    async fn recover_publication(
        &mut self,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: DownloadContent,
    ) -> Result<Option<FilePublicationReceipt>, DownloadReason> {
        let _ = (planned, expected);
        if policy.is_some() {
            Err(DownloadReason::Unsupported)
        } else {
            Ok(None)
        }
    }
    async fn create(&mut self) -> Result<(), DownloadReason>;
    async fn reopen(&mut self) -> Result<u64, DownloadReason>;
    async fn read(&mut self, offset: u64, limit: usize) -> Result<Vec<u8>, DownloadReason>;
    async fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), DownloadReason>;
    async fn flush(&mut self) -> Result<(), DownloadReason>;
    async fn verify(&mut self, maximum: u64) -> Result<DownloadContent, DownloadReason>;
    async fn publish(&mut self) -> Result<(), DownloadReason>;
    async fn cleanup(&mut self) -> Result<(), DownloadReason>;
    async fn probe(&mut self, maximum: u64) -> Result<DownloadProbe, DownloadReason>;
    async fn close(&mut self);
}
#[async_trait]
pub trait DownloadExecutor: fmt::Debug + Send + Sync {
    async fn connect(
        &self,
        snapshot: &BatchDownloadSnapshot,
        host: HostId,
        cancel: TaskCancellation,
    ) -> Result<Box<dyn DownloadRemote>, DownloadReason>;
    async fn local(
        &self,
        snapshot: &BatchDownloadSnapshot,
        host: HostId,
    ) -> Result<Box<dyn DownloadLocal>, DownloadReason>;
}
