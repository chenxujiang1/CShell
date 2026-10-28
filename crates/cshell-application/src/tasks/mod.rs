//! Immutable task plans and durable, revisioned execution transitions.
mod budget;
mod downloads;
mod file_publication;
mod mixed;
mod output_analysis;
mod output_archive;
mod output_bundle;
mod output_merge;
mod output_search;
mod results;
mod run;
mod scheduler;
mod shell;
mod templates;
mod text_diff;
mod uploads;
mod workspace;
use crate::FrozenHostTargets;
use async_trait::async_trait;
pub use budget::*;
use cshell_domain::*;
pub use downloads::*;
pub use file_publication::*;
pub use mixed::*;
pub use output_analysis::*;
pub use output_archive::*;
pub use output_bundle::*;
pub use output_merge::*;
pub use output_search::*;
pub use results::*;
pub use run::*;
pub use scheduler::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt, io::Write, sync::Arc};
pub use templates::*;
pub use text_diff::*;
use thiserror::Error;
pub use uploads::*;
pub use workspace::*;

pub const MAX_TASK_STEPS: usize = 32;
pub const MAX_TASK_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_TASK_SNAPSHOT_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_TASK_TARGET_BYTES: usize = 64 * 1024;
pub const MAX_TASK_META_BYTES: usize = 16 * 1024;
pub const MAX_TASK_LIST_RESULTS: usize = 64;
const MAX_TIMEOUT_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum TaskError {
    #[error("invalid task definition, snapshot or execution state")]
    Invalid,
    #[error("task exceeds its resource limits")]
    Limit,
    #[error("task variable is missing")]
    MissingVariable,
    #[error("Cmd cannot safely represent this dynamic value or multiline command")]
    UnsafeCmdValue,
    #[error("task transition is not allowed in the current state")]
    Transition,
    #[error("unknown task target")]
    UnknownTarget,
    #[error("Unknown targets require explicit retry confirmation and an idempotent task")]
    UnsafeRetry,
    #[error("task clock moved backwards or exceeds the persisted time range")]
    Clock,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum TaskRepositoryError {
    #[error("task revision changed")]
    Conflict,
    #[error("task storage unavailable; the commit result may be unknown")]
    Unavailable,
    #[error("stored task data is corrupt")]
    Corrupt,
    #[error("task does not exist")]
    Missing,
}

pub(super) fn label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
struct LimitWriter {
    count: usize,
    maximum: usize,
}
impl Write for LimitWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.count = self.count.saturating_add(buf.len());
        if self.count > self.maximum {
            return Err(std::io::Error::other("task document limit"));
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn encode<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>, TaskError> {
    serde_json::to_writer(&mut LimitWriter { count: 0, maximum }, value)
        .map_err(|_| TaskError::Limit)?;
    serde_json::to_vec(value).map_err(|_| TaskError::Invalid)
}
pub(super) fn decode<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    maximum: usize,
) -> Result<T, TaskError> {
    if bytes.len() > maximum {
        return Err(TaskError::Limit);
    }
    serde_json::from_slice(bytes).map_err(|_| TaskError::Invalid)
}
pub fn validate_task_definition(definition: &TaskDefinition) -> Result<(), TaskError> {
    if definition.id.as_uuid().is_nil() || !label(&definition.name) || definition.steps.is_empty() {
        return Err(TaskError::Invalid);
    }
    if definition.steps.len() > MAX_TASK_STEPS {
        return Err(TaskError::Limit);
    }
    for step in &definition.steps {
        if !label(&step.name) || step.success_codes.is_empty() || step.success_codes.len() > 16 {
            return Err(TaskError::Invalid);
        }
    }
    let p = &definition.policy;
    if p.concurrency == 0
        || p.concurrency > 50
        || p.batch_size.is_some_and(|v| v == 0 || v > 1000)
        || p.canary_count > 1000
        || p.approve_after_canary && p.canary_count == 0
        || p.failure_limit.is_some_and(|v| v == 0 || v > 1000)
        || p.batch_delay_ms > MAX_TIMEOUT_MS
        || [
            p.queue_timeout_ms,
            p.connect_timeout_ms,
            p.step_timeout_ms,
            p.total_timeout_ms,
        ]
        .iter()
        .any(|v| *v == 0 || *v > MAX_TIMEOUT_MS)
    {
        return Err(TaskError::Invalid);
    }
    encode(definition, 1024 * 1024)?;
    shell::compile(definition)?;
    Ok(())
}
pub fn compile_task_commands(definition: &TaskDefinition) -> Result<Vec<String>, TaskError> {
    validate_task_definition(definition)?;
    shell::compile(definition)
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotData {
    format: String,
    version: u32,
    compiler_version: u32,
    id: TaskRunId,
    retry_of: Option<TaskRunId>,
    unknown_retry_confirmed: bool,
    operator: String,
    created_at_ms: u64,
    definition: TaskDefinition,
    targets: FrozenHostTargets,
    commands: Vec<String>,
}
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TaskSnapshot {
    data: Arc<SnapshotData>,
    #[serde(skip)]
    fingerprint: [u8; 32],
}
impl fmt::Debug for TaskSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskSnapshot")
            .field("id", &self.id())
            .field("targets", &self.targets().targets().len())
            .field("steps", &self.data.commands.len())
            .field("bodies", &"[REDACTED]")
            .finish()
    }
}
impl<'de> Deserialize<'de> for TaskSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let data = SnapshotData::deserialize(deserializer)?;
        validate_snapshot(&data).map_err(serde::de::Error::custom)?;
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
fn validate_snapshot(data: &SnapshotData) -> Result<(), TaskError> {
    if data.format != "cshell.task-snapshot"
        || data.version != 1
        || data.compiler_version != 1
        || data.id.as_uuid().is_nil()
        || data
            .retry_of
            .is_some_and(|id| id.as_uuid().is_nil() || id == data.id)
        || !label(&data.operator)
        || data.unknown_retry_confirmed
            && (data.retry_of.is_none()
                || data.definition.steps.iter().any(|step| !step.idempotent))
    {
        return Err(TaskError::Invalid);
    }
    if data.created_at_ms > i64::MAX as u64 - MAX_TIMEOUT_MS {
        return Err(TaskError::Clock);
    }
    if compile_task_commands(&data.definition)? != data.commands {
        return Err(TaskError::Invalid);
    }
    encode(data, MAX_TASK_SNAPSHOT_BYTES)?;
    Ok(())
}
impl TaskSnapshot {
    pub fn new(
        definition: TaskDefinition,
        targets: FrozenHostTargets,
        operator: String,
        now_ms: u64,
    ) -> Result<Self, TaskError> {
        Self::with_fixed_id(definition, targets, operator, now_ms, TaskRunId::new())
    }
    pub(super) fn with_fixed_id(
        definition: TaskDefinition,
        targets: FrozenHostTargets,
        operator: String,
        now_ms: u64,
        id: TaskRunId,
    ) -> Result<Self, TaskError> {
        let commands = compile_task_commands(&definition)?;
        let data = SnapshotData {
            format: "cshell.task-snapshot".into(),
            version: 1,
            compiler_version: 1,
            id,
            retry_of: None,
            unknown_retry_confirmed: false,
            operator,
            created_at_ms: now_ms,
            definition,
            targets,
            commands,
        };
        validate_snapshot(&data)?;
        let fingerprint = Sha256::digest(encode(&data, MAX_TASK_SNAPSHOT_BYTES)?).into();
        Ok(Self {
            data: Arc::new(data),
            fingerprint,
        })
    }
    pub fn retry(
        record: &TaskRunRecord,
        selected: &BTreeSet<HostId>,
        confirm_unknown: bool,
        operator: String,
        now_ms: u64,
    ) -> Result<Self, TaskError> {
        if selected.is_empty() {
            return Err(TaskError::UnknownTarget);
        }
        for id in selected {
            let target = record.target(*id).ok_or(TaskError::UnknownTarget)?;
            if !target_terminal(target.state) || target.state == TargetState::Succeeded {
                return Err(TaskError::Transition);
            }
            if target.state == TargetState::Unknown
                && (!confirm_unknown
                    || !record
                        .snapshot()
                        .definition()
                        .steps
                        .iter()
                        .all(|step| step.idempotent))
            {
                return Err(TaskError::UnsafeRetry);
            }
        }
        let mut next = Self::new(
            record.snapshot().definition().clone(),
            record
                .snapshot()
                .targets()
                .subset(selected)
                .map_err(|_| TaskError::UnknownTarget)?,
            operator,
            now_ms,
        )?;
        let data = Arc::make_mut(&mut next.data);
        data.retry_of = Some(record.id());
        data.unknown_retry_confirmed = selected.iter().any(|id| {
            record
                .target(*id)
                .is_some_and(|target| target.state == TargetState::Unknown)
        }) && confirm_unknown;
        next.validate_retry_parent(record)?;
        next.fingerprint = Sha256::digest(encode(&next.data, MAX_TASK_SNAPSHOT_BYTES)?).into();
        Ok(next)
    }
    /// Validate persisted retry provenance against the original durable run.
    /// A caller cannot replace the frozen endpoint, credentials or command definition.
    pub fn validate_retry_parent(&self, parent: &TaskRunRecord) -> Result<(), TaskError> {
        if self.retry_of() != Some(parent.id())
            || self.id() == parent.id()
            || self.created_at_ms() < parent.meta().updated_at_ms
            || self.definition() != parent.snapshot().definition()
            || self.commands() != parent.snapshot().commands()
            || self.targets().id() == parent.snapshot().targets().id()
        {
            return Err(TaskError::Invalid);
        }
        let ids: BTreeSet<_> = self
            .targets()
            .targets()
            .iter()
            .map(|host| host.id)
            .collect();
        let expected = parent
            .snapshot()
            .targets()
            .subset(&ids)
            .map_err(|_| TaskError::UnknownTarget)?;
        if self.targets().targets() != expected.targets()
            || self.targets().connections() != expected.connections()
            || self.targets().host_revision() != expected.host_revision()
            || self.targets().profile_revision() != expected.profile_revision()
            || self.targets().selection() != expected.selection()
        {
            return Err(TaskError::Invalid);
        }
        let mut has_unknown = false;
        for id in ids {
            let target = parent.target(id).ok_or(TaskError::UnknownTarget)?;
            if !target_terminal(target.state) || target.state == TargetState::Succeeded {
                return Err(TaskError::Transition);
            }
            has_unknown |= target.state == TargetState::Unknown;
        }
        if has_unknown != self.data.unknown_retry_confirmed {
            return Err(TaskError::UnsafeRetry);
        }
        if has_unknown && self.definition().steps.iter().any(|step| !step.idempotent) {
            return Err(TaskError::UnsafeRetry);
        }
        Ok(())
    }
    #[must_use]
    pub fn id(&self) -> TaskRunId {
        self.data.id
    }
    #[must_use]
    pub fn retry_of(&self) -> Option<TaskRunId> {
        self.data.retry_of
    }
    #[must_use]
    pub fn operator(&self) -> &str {
        &self.data.operator
    }
    #[must_use]
    pub fn created_at_ms(&self) -> u64 {
        self.data.created_at_ms
    }
    #[must_use]
    pub fn definition(&self) -> &TaskDefinition {
        &self.data.definition
    }
    #[must_use]
    pub fn targets(&self) -> &FrozenHostTargets {
        &self.data.targets
    }
    #[must_use]
    pub fn commands(&self) -> &[String] {
        &self.data.commands
    }
    #[must_use]
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }
    /// Stable batches refer only to the frozen Host IDs.
    #[must_use]
    pub fn batches(&self) -> Vec<Vec<HostId>> {
        let ids: Vec<_> = self
            .targets()
            .targets()
            .iter()
            .map(|host| host.id)
            .collect();
        let policy = &self.definition().policy;
        let canary = usize::from(policy.canary_count).min(ids.len());
        let mut result = Vec::new();
        if canary > 0 {
            result.push(ids[..canary].to_vec());
        }
        let size = policy.batch_size.map_or(ids.len(), usize::from);
        result.extend(ids[canary..].chunks(size).map(<[HostId]>::to_vec));
        result
    }
}
pub fn encode_task_snapshot(snapshot: &TaskSnapshot) -> Result<Vec<u8>, TaskError> {
    encode(snapshot, MAX_TASK_SNAPSHOT_BYTES)
}
pub fn decode_task_snapshot(bytes: &[u8]) -> Result<TaskSnapshot, TaskError> {
    decode(bytes, MAX_TASK_SNAPSHOT_BYTES)
}

#[async_trait]
pub trait TaskRepository: fmt::Debug + Send + Sync {
    async fn create_task_run(&self, record: &TaskRunRecord) -> Result<(), TaskRepositoryError>;
    async fn load_task_run(
        &self,
        id: TaskRunId,
    ) -> Result<Option<TaskRunRecord>, TaskRepositoryError>;
    async fn commit_task_patch(&self, patch: &TaskRunPatch) -> Result<u64, TaskRepositoryError>;
    async fn list_task_runs(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
        unfinished_only: bool,
    ) -> Result<Vec<TaskRunSummary>, TaskRepositoryError>;
}

#[cfg(test)]
mod scheduler_tests;
#[cfg(test)]
mod tests;
