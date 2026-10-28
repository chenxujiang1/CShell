//! Uploads have filesystem receipts, never synthetic SSH exit codes.
mod runner;
use super::*;
pub use runner::*;
use std::path::Path;

pub const UPLOAD_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_UPLOAD_FILE_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub const MAX_UPLOAD_ROW_BYTES: usize = 16 * 1024;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchUploadDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_policy: Option<FilePublicationPolicy>,
    pub name: String,
    pub local_source: String,
    pub remote_destination: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub concurrency: u16,
    pub queue_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub transfer_timeout_ms: u64,
}
impl BatchUploadDefinition {
    pub fn validate(&self) -> Result<(), TaskError> {
        if !label(&self.name)
            || !Path::new(&self.local_source).is_absolute()
            || self.local_source.len() > 4096
            || self.local_source.chars().any(char::is_control)
            || !upload_remote_path(&self.remote_destination)
            || self.size > MAX_UPLOAD_FILE_BYTES
            || self.concurrency == 0
            || self.concurrency > 50
            || [
                self.queue_timeout_ms,
                self.connect_timeout_ms,
                self.transfer_timeout_ms,
            ]
            .iter()
            .any(|value| *value == 0 || *value > super::MAX_TIMEOUT_MS)
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
impl fmt::Debug for BatchUploadDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchUploadDefinition")
            .field("size", &self.size)
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchUploadSnapshot {
    pub format: String,
    pub version: u32,
    pub id: TaskRunId,
    pub definition: BatchUploadDefinition,
    pub targets: FrozenHostTargets,
    pub created_at_ms: u64,
}
impl fmt::Debug for BatchUploadSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchUploadSnapshot")
            .field("id", &self.id)
            .field("targets", &self.targets.targets().len())
            .finish_non_exhaustive()
    }
}
impl BatchUploadSnapshot {
    pub fn backup(&self, host: HostId) -> Result<String, TaskError> {
        self.temporary(host)?;
        let (parent, _) = self
            .definition
            .remote_destination
            .rsplit_once('/')
            .ok_or(TaskError::Invalid)?;
        Ok(format!("{parent}/.cshell-backup-{}-{host}.bak", self.id))
    }
    pub fn new(
        definition: BatchUploadDefinition,
        targets: FrozenHostTargets,
        now: u64,
    ) -> Result<Self, TaskError> {
        let result = Self {
            format: "cshell.batch-upload".into(),
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
        let d = &self.definition;
        if self.format != "cshell.batch-upload"
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || !label(&d.name)
            || !Path::new(&d.local_source).is_absolute()
            || d.local_source.len() > 4096
            || d.local_source.chars().any(char::is_control)
            || !upload_remote_path(&d.remote_destination)
            || d.size > MAX_UPLOAD_FILE_BYTES
            || d.concurrency == 0
            || d.concurrency > 50
            || [
                d.queue_timeout_ms,
                d.connect_timeout_ms,
                d.transfer_timeout_ms,
            ]
            .iter()
            .any(|timeout| *timeout == 0 || *timeout > super::MAX_TIMEOUT_MS)
            || self.targets.targets().is_empty()
            || self.targets.targets().len() > crate::MAX_HOST_TARGETS
            || self.created_at_ms > i64::MAX as u64 - super::MAX_TIMEOUT_MS
        {
            return Err(TaskError::Invalid);
        }
        if self.targets.targets().iter().any(|host| {
            self.temporary(host.id).is_err()
                || self
                    .temporary(host.id)
                    .is_ok_and(|path| path == d.remote_destination)
        }) {
            return Err(TaskError::Invalid);
        }
        encode(self, MAX_TASK_SNAPSHOT_BYTES)?;
        Ok(())
    }
    pub fn temporary(&self, host: HostId) -> Result<String, TaskError> {
        if !self
            .targets
            .targets()
            .iter()
            .any(|target| target.id == host)
        {
            return Err(TaskError::UnknownTarget);
        }
        let (parent, _) = self
            .definition
            .remote_destination
            .rsplit_once('/')
            .ok_or(TaskError::Invalid)?;
        let path = format!("{parent}/.cshell-{}-{host}.tmp", self.id);
        if path.len() > 4096 {
            return Err(TaskError::Limit);
        }
        Ok(path)
    }
}
pub fn upload_remote_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 4000
        && !path.ends_with('/')
        && !path.chars().any(char::is_control)
        && !path.contains('\\')
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadPhase {
    Pending,
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
impl UploadPhase {
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
    #[must_use]
    pub const fn active(self) -> bool {
        matches!(
            self,
            Self::Creating | Self::Writing | Self::Verifying | Self::PublishIntent
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadIntent {
    None,
    Create,
    Write,
    Verify,
    Publish,
    Cleanup,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadReason {
    Connection,
    Unsupported,
    DestinationExists,
    SourceChanged,
    Checksum,
    Transport,
    Storage,
    Cancelled,
    Timeout,
    Cleanup,
    Restarted,
    Invalid,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<FilePublicationReceipt>,
    pub run_id: TaskRunId,
    pub host_id: HostId,
    pub revision: u64,
    pub phase: UploadPhase,
    pub intent: UploadIntent,
    pub bytes: u64,
    pub temporary_owned: bool,
    pub verified: bool,
    pub committed: bool,
    pub adopted: bool,
    pub temporary_cleaned: bool,
    pub reason: Option<UploadReason>,
    pub updated_at_ms: u64,
}
impl UploadTarget {
    #[must_use]
    pub fn can_resume(&self) -> bool {
        self.phase == UploadPhase::Unknown
            && self.temporary_owned
            && !self.temporary_cleaned
            && !self.verified
            && !self.committed
            && self.intent != UploadIntent::Publish
    }
    #[must_use]
    pub fn can_cleanup(&self) -> bool {
        self.temporary_owned
            && !self.temporary_cleaned
            && matches!(
                self.phase,
                UploadPhase::Failed
                    | UploadPhase::Cancelled
                    | UploadPhase::Committed
                    | UploadPhase::CleanupRequired
                    | UploadPhase::Unknown
            )
            && !(self.phase == UploadPhase::Unknown && self.intent == UploadIntent::Publish)
    }
    #[must_use]
    pub fn can_adopt(&self) -> bool {
        self.phase == UploadPhase::Unknown
            && self.intent == UploadIntent::Publish
            && self.verified
            && self.temporary_owned
            && !self.temporary_cleaned
            && !self.committed
    }
    #[must_use]
    pub fn initial(snapshot: &BatchUploadSnapshot, host_id: HostId) -> Self {
        Self {
            run_id: snapshot.id,
            publication: None,
            host_id,
            revision: 1,
            phase: UploadPhase::Pending,
            intent: UploadIntent::None,
            bytes: 0,
            temporary_owned: false,
            verified: false,
            committed: false,
            adopted: false,
            temporary_cleaned: false,
            reason: None,
            updated_at_ms: snapshot.created_at_ms,
        }
    }
    pub fn validate(&self, snapshot: &BatchUploadSnapshot) -> Result<(), TaskError> {
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
                receipt.validate_commit(
                    policy,
                    UploadContent {
                        size: snapshot.definition.size,
                        sha256: snapshot.definition.sha256,
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
        if self.run_id != snapshot.id
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.updated_at_ms < snapshot.created_at_ms
            || self.updated_at_ms > i64::MAX as u64
            || self.bytes > snapshot.definition.size
            || self.host_id.as_uuid().is_nil()
            || !snapshot
                .targets
                .targets()
                .iter()
                .any(|target| target.id == self.host_id)
            || self.verified && (!self.temporary_owned || self.bytes != snapshot.definition.size)
            || self.committed && !self.verified
            || self.adopted && !self.committed
            || self.temporary_cleaned && !self.temporary_owned
            || self.phase == UploadPhase::Succeeded && !self.temporary_cleaned
            || matches!(
                self.phase,
                UploadPhase::PublishIntent | UploadPhase::Committed | UploadPhase::Succeeded
            ) && !self.verified
            || matches!(self.phase, UploadPhase::Committed | UploadPhase::Succeeded)
                && !self.committed
            || self.phase == UploadPhase::Pending && *self != Self::initial(snapshot, self.host_id)
        {
            return Err(TaskError::Invalid);
        }
        encode(self, MAX_UPLOAD_ROW_BYTES)?;
        Ok(())
    }
    pub fn validate_next(
        &self,
        next: &Self,
        snapshot: &BatchUploadSnapshot,
    ) -> Result<(), TaskError> {
        self.validate(snapshot)?;
        next.validate(snapshot)?;
        match (&self.publication, &next.publication) {
            (Some(old), Some(new)) => old.validate_next(new)?,
            (Some(_), None) => return Err(TaskError::Transition),
            (None, Some(_))
                if self.phase != UploadPhase::Pending || next.phase != UploadPhase::Creating =>
            {
                return Err(TaskError::Transition);
            }
            _ => {}
        }
        if next.run_id != self.run_id
            || next.host_id != self.host_id
            || Some(next.revision) != self.revision.checked_add(1)
            || next.updated_at_ms < self.updated_at_ms
            || next.bytes < self.bytes
            || self.temporary_owned && !next.temporary_owned
            || self.verified && !next.verified
            || self.committed && !next.committed
            || self.adopted && !next.adopted
            || self.temporary_cleaned && !next.temporary_cleaned
        {
            return Err(TaskError::Transition);
        }
        let allowed = match self.phase {
            UploadPhase::Pending => matches!(
                next.phase,
                UploadPhase::Creating | UploadPhase::Failed | UploadPhase::Cancelled
            ),
            UploadPhase::Creating => matches!(
                next.phase,
                UploadPhase::Writing | UploadPhase::Unknown | UploadPhase::Failed
            ),
            UploadPhase::Writing => matches!(
                next.phase,
                UploadPhase::Writing
                    | UploadPhase::Verifying
                    | UploadPhase::Failed
                    | UploadPhase::Unknown
                    | UploadPhase::Cancelled
            ),
            UploadPhase::Verifying => matches!(
                next.phase,
                UploadPhase::PublishIntent
                    | UploadPhase::Failed
                    | UploadPhase::Unknown
                    | UploadPhase::Cancelled
            ),
            UploadPhase::PublishIntent => matches!(
                next.phase,
                UploadPhase::Committed | UploadPhase::Unknown | UploadPhase::Failed
            ),
            UploadPhase::Committed => matches!(
                next.phase,
                UploadPhase::Succeeded | UploadPhase::CleanupRequired
            ),
            UploadPhase::CleanupRequired => matches!(
                next.phase,
                UploadPhase::CleanupRequired
                    | UploadPhase::Succeeded
                    | UploadPhase::Failed
                    | UploadPhase::Cancelled
            ),
            UploadPhase::Failed | UploadPhase::Cancelled => {
                next.phase == UploadPhase::CleanupRequired
                    && self.temporary_owned
                    && !self.temporary_cleaned
            }
            UploadPhase::Unknown => {
                next.phase == UploadPhase::Unknown
                    || (next.phase == UploadPhase::Writing
                        && self.temporary_owned
                        && !self.verified
                        && self.intent != UploadIntent::Publish)
                    || (next.phase == UploadPhase::Committed
                        && next.adopted
                        && self.intent == UploadIntent::Publish
                        && self.verified)
                    || (next.phase == UploadPhase::CleanupRequired
                        && self.temporary_owned
                        && self.intent != UploadIntent::Publish)
            }
            UploadPhase::Succeeded => false,
        };
        if !allowed {
            return Err(TaskError::Transition);
        }
        if !self.temporary_owned
            && next.temporary_owned
            && !(self.phase == UploadPhase::Creating && next.phase == UploadPhase::Writing)
            || !self.verified
                && next.verified
                && !(self.phase == UploadPhase::Verifying
                    && next.phase == UploadPhase::PublishIntent)
            || !self.committed
                && next.committed
                && !(next.phase == UploadPhase::Committed
                    && (self.phase == UploadPhase::PublishIntent
                        || self.phase == UploadPhase::Unknown && next.adopted))
            || !self.adopted
                && next.adopted
                && !(self.phase == UploadPhase::Unknown
                    && next.phase == UploadPhase::Committed
                    && self.intent == UploadIntent::Publish)
            || !self.temporary_cleaned
                && next.temporary_cleaned
                && !(self.phase == UploadPhase::CleanupRequired
                    && matches!(
                        next.phase,
                        UploadPhase::Succeeded | UploadPhase::Failed | UploadPhase::Cancelled
                    ))
        {
            return Err(TaskError::Transition);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UploadRepositoryError {
    #[error("upload revision changed")]
    Conflict,
    #[error("upload record missing")]
    Missing,
    #[error("upload storage unavailable; commit may be uncertain")]
    Unavailable,
    #[error("upload storage is corrupt")]
    Corrupt,
}
#[async_trait]
pub trait UploadRepository: fmt::Debug + Send + Sync {
    async fn list_uploads(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
    ) -> Result<Vec<UploadRunSummary>, UploadRepositoryError>;
    async fn create_upload(
        &self,
        snapshot: &BatchUploadSnapshot,
    ) -> Result<(), UploadRepositoryError>;
    async fn load_upload(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchUploadSnapshot, Vec<UploadTarget>)>, UploadRepositoryError>;
    async fn commit_upload_target(
        &self,
        expected: &UploadTarget,
        next: &UploadTarget,
    ) -> Result<(), UploadRepositoryError>;
}
pub fn encode_upload_snapshot(snapshot: &BatchUploadSnapshot) -> Result<Vec<u8>, TaskError> {
    snapshot.validate()?;
    encode(snapshot, MAX_TASK_SNAPSHOT_BYTES)
}
pub fn decode_upload_snapshot(bytes: &[u8]) -> Result<BatchUploadSnapshot, TaskError> {
    let value: BatchUploadSnapshot = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    value.validate()?;
    Ok(value)
}
pub fn encode_upload_target(target: &UploadTarget) -> Result<Vec<u8>, TaskError> {
    encode(target, MAX_UPLOAD_ROW_BYTES)
}
pub fn decode_upload_target(
    bytes: &[u8],
    snapshot: &BatchUploadSnapshot,
) -> Result<UploadTarget, TaskError> {
    let value: UploadTarget = decode(bytes, MAX_UPLOAD_ROW_BYTES)?;
    value.validate(snapshot)?;
    Ok(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadContent {
    pub size: u64,
    pub sha256: [u8; 32],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UploadProbe {
    pub temporary: Option<UploadContent>,
    pub destination: Option<UploadContent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadRecoveryOperation {
    Probe,
    Cleanup,
    Adopt,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRecoveryStatus {
    Running,
    Completed,
    Failed,
}
pub type UploadRecoveryStatus = FileRecoveryStatus;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRecoveryJob {
    pub host_id: HostId,
    pub row_revision: u64,
    pub operation: UploadRecoveryOperation,
    pub status: UploadRecoveryStatus,
    pub probe: Option<UploadProbe>,
    pub error: Option<UploadReason>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadStatusTarget {
    pub host_id: HostId,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub row: UploadTarget,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadStatusDocument {
    pub format: String,
    pub version: u32,
    pub id: TaskRunId,
    pub definition: BatchUploadDefinition,
    pub created_at_ms: u64,
    pub host_revision: u64,
    pub profile_revision: u64,
    pub active: bool,
    pub targets: Vec<UploadStatusTarget>,
    #[serde(default)]
    pub recovery: Option<UploadRecoveryJob>,
}
impl fmt::Debug for UploadStatusDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadStatusDocument")
            .field("id", &self.id)
            .field("targets", &self.targets.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}
impl UploadStatusDocument {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.definition.validate()?;
        if self.format != "cshell.batch-upload-status"
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || self.created_at_ms > i64::MAX as u64
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
            let row = &target.row;
            if target.host_id.as_uuid().is_nil()
                || row.host_id != target.host_id
                || row.run_id != self.id
                || !label(&target.name)
                || target.address.is_empty()
                || target.address.len() > 255
                || target.address.chars().any(char::is_control)
                || target.port == 0
                || row.revision == 0
                || row.revision > i64::MAX as u64
                || row.bytes > self.definition.size
                || row.updated_at_ms < self.created_at_ms
                || row.verified && (!row.temporary_owned || row.bytes != self.definition.size)
                || row.committed && !row.verified
                || row.adopted && !row.committed
                || row.temporary_cleaned && !row.temporary_owned
                || row.phase == UploadPhase::Succeeded && (!row.committed || !row.temporary_cleaned)
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
                || job.probe.is_some()
                    && (job.operation != UploadRecoveryOperation::Probe
                        || job.status != UploadRecoveryStatus::Completed)
                || job.status == UploadRecoveryStatus::Running
                    && (!self.active || job.error.is_some() || job.probe.is_some())
                || job.status == UploadRecoveryStatus::Completed && job.error.is_some()
                || job.status == UploadRecoveryStatus::Completed
                    && job.operation == UploadRecoveryOperation::Probe
                    && job.probe.is_none()
                || job.status == UploadRecoveryStatus::Failed
                    && (job.error.is_none() || job.probe.is_some())
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
    #[must_use]
    pub fn target(&self, host: HostId) -> Option<&UploadStatusTarget> {
        self.targets
            .binary_search_by_key(&host, |target| target.host_id)
            .ok()
            .map(|index| &self.targets[index])
    }
}
pub fn decode_upload_status(bytes: &[u8]) -> Result<UploadStatusDocument, TaskError> {
    let document: UploadStatusDocument = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    document.validate()?;
    Ok(document)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRunSummary {
    pub id: TaskRunId,
    pub name: String,
    pub created_at_ms: u64,
    pub targets: u64,
}
pub fn decode_upload_list(bytes: &[u8]) -> Result<Vec<UploadRunSummary>, TaskError> {
    let rows: Vec<UploadRunSummary> = decode(bytes, 256 * 1024)?;
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
pub trait UploadSource: fmt::Debug + Send {
    async fn read(&mut self, offset: u64, limit: usize) -> Result<Vec<u8>, UploadReason>;
}
#[async_trait]
pub trait UploadRemote: fmt::Debug + Send {
    async fn plan_publication(
        &mut self,
        temporary: &str,
        destination: &str,
        backup: &str,
        policy: Option<FilePublicationPolicy>,
        expected: UploadContent,
    ) -> Result<Option<FilePublicationReceipt>, UploadReason> {
        let _ = (backup, expected);
        if policy.is_some() {
            return Err(UploadReason::Unsupported);
        }
        self.preflight(temporary, destination).await?;
        Ok(None)
    }
    async fn publish_planned(
        &mut self,
        temporary: &str,
        destination: &str,
        backup: &str,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: UploadContent,
    ) -> Result<Option<FilePublicationReceipt>, UploadReason> {
        let _ = (backup, planned, expected);
        if policy.is_some() {
            return Err(UploadReason::Unsupported);
        }
        self.publish(temporary, destination).await?;
        Ok(None)
    }
    async fn recover_publication(
        &mut self,
        backup: &str,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: UploadContent,
    ) -> Result<Option<FilePublicationReceipt>, UploadReason> {
        let _ = (backup, planned, expected);
        if policy.is_some() {
            Err(UploadReason::Unsupported)
        } else {
            Ok(None)
        }
    }
    async fn preflight(&mut self, temporary: &str, destination: &str) -> Result<(), UploadReason>;
    async fn create(&mut self, temporary: &str) -> Result<(), UploadReason>;
    async fn reopen(&mut self, temporary: &str) -> Result<u64, UploadReason>;
    async fn read(
        &mut self,
        temporary: &str,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>, UploadReason>;
    async fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), UploadReason>;
    async fn verify(&mut self, temporary: &str) -> Result<UploadContent, UploadReason>;
    async fn publish(&mut self, temporary: &str, destination: &str) -> Result<(), UploadReason>;
    async fn cleanup(&mut self, temporary: &str) -> Result<(), UploadReason>;
    async fn probe(
        &mut self,
        temporary: &str,
        destination: &str,
    ) -> Result<UploadProbe, UploadReason>;
    async fn close(&mut self);
}
#[async_trait]
pub trait UploadExecutor: fmt::Debug + Send + Sync {
    async fn source(
        &self,
        snapshot: &BatchUploadSnapshot,
    ) -> Result<Box<dyn UploadSource>, UploadReason>;
    async fn connect(
        &self,
        snapshot: &BatchUploadSnapshot,
        host: HostId,
        cancel: TaskCancellation,
    ) -> Result<Box<dyn UploadRemote>, UploadReason>;
}
