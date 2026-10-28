//! Readonly result models. Neither command bodies nor credential templates enter exports.
use super::*;
use std::{collections::BTreeMap, io};
use zeroize::Zeroize;

pub const MAX_TASK_RESULTS_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_TASK_RESULT_EXPORT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TASK_COMPARE_PAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStateCounts {
    pub pending: u64,
    pub connecting: u64,
    pub running: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub cancelled: u64,
    pub unknown: u64,
}
impl TaskStateCounts {
    fn add(&mut self, state: TargetState) {
        match state {
            TargetState::Pending => self.pending += 1,
            TargetState::Connecting => self.connecting += 1,
            TargetState::Running => self.running += 1,
            TargetState::Succeeded => self.succeeded += 1,
            TargetState::Failed => self.failed += 1,
            TargetState::TimedOut => self.timed_out += 1,
            TargetState::Cancelled => self.cancelled += 1,
            TargetState::Unknown => self.unknown += 1,
        }
    }
    #[must_use]
    pub fn get(&self, state: TargetState) -> u64 {
        match state {
            TargetState::Pending => self.pending,
            TargetState::Connecting => self.connecting,
            TargetState::Running => self.running,
            TargetState::Succeeded => self.succeeded,
            TargetState::Failed => self.failed,
            TargetState::TimedOut => self.timed_out,
            TargetState::Cancelled => self.cancelled,
            TargetState::Unknown => self.unknown,
        }
    }
    #[must_use]
    pub fn total(&self) -> u64 {
        self.pending
            + self.connecting
            + self.running
            + self.succeeded
            + self.failed
            + self.timed_out
            + self.cancelled
            + self.unknown
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResultStep {
    pub name: String,
    pub success_codes: BTreeSet<u32>,
    pub idempotent: bool,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResultTarget {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub profile_id: ProfileId,
    pub run: TaskTargetRun,
}
impl fmt::Debug for TaskResultTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskResultTarget")
            .field("host_id", &self.run.host_id)
            .field("state", &self.run.state)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResultSummary {
    pub targets: TaskStateCounts,
    pub steps: TaskStateCounts,
    pub exit_codes: BTreeMap<u32, u64>,
    pub target_reasons: BTreeMap<String, u64>,
    pub step_reasons: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutcomeGroup {
    pub representative: HostId,
    pub hosts: Vec<HostId>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResultDocument {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub name: String,
    pub operator: String,
    pub retry_of: Option<TaskRunId>,
    pub host_revision: u64,
    pub profile_revision: u64,
    pub meta: TaskRunMeta,
    pub runner_active: bool,
    pub steps: Vec<TaskResultStep>,
    pub targets: Vec<TaskResultTarget>,
    pub summary: TaskResultSummary,
    pub outcomes: Vec<TaskOutcomeGroup>,
}
impl fmt::Debug for TaskResultDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskResultDocument")
            .field("run_id", &self.run_id)
            .field("revision", &self.meta.revision)
            .field("targets", &self.targets.len())
            .finish_non_exhaustive()
    }
}
#[must_use]
pub const fn task_reason_name(reason: TaskStopReason) -> &'static str {
    match reason {
        TaskStopReason::UserCancelled => "user_cancelled",
        TaskStopReason::ConnectionFailure => "connection_failure",
        TaskStopReason::ConnectionTimeout => "connection_timeout",
        TaskStopReason::QueueTimeout => "queue_timeout",
        TaskStopReason::StepTimeout => "step_timeout",
        TaskStopReason::TotalTimeout => "total_timeout",
        TaskStopReason::TransportLost => "transport_lost",
        TaskStopReason::OutputFailure => "output_failure",
        TaskStopReason::ExecRejected => "exec_rejected",
        TaskStopReason::DaemonRestarted => "daemon_restarted",
        TaskStopReason::FailureThreshold => "failure_threshold",
        TaskStopReason::CanaryFailed => "canary_failed",
    }
}
#[must_use]
pub const fn task_target_state_name(state: TargetState) -> &'static str {
    match state {
        TargetState::Pending => "pending",
        TargetState::Connecting => "connecting",
        TargetState::Running => "running",
        TargetState::Succeeded => "succeeded",
        TargetState::Failed => "failed",
        TargetState::TimedOut => "timed_out",
        TargetState::Cancelled => "cancelled",
        TargetState::Unknown => "unknown",
    }
}
fn summary(targets: &[TaskResultTarget]) -> TaskResultSummary {
    let mut result = TaskResultSummary::default();
    for target in targets {
        result.targets.add(target.run.state);
        if let Some(reason) = target.run.reason {
            *result
                .target_reasons
                .entry(task_reason_name(reason).into())
                .or_default() += 1;
        }
        for step in &target.run.steps {
            result.steps.add(step.state);
            if let Some(code) = step.exit_code {
                *result.exit_codes.entry(code).or_default() += 1;
            }
            if let Some(reason) = step.reason {
                *result
                    .step_reasons
                    .entry(task_reason_name(reason).into())
                    .or_default() += 1;
            }
        }
    }
    result
}
// Group by actual outcome, ignoring endpoint and timing differences.
fn groups(targets: &[TaskResultTarget]) -> Vec<TaskOutcomeGroup> {
    type StepKey = (&'static str, Option<u32>, Option<&'static str>);
    type Key = (&'static str, Option<&'static str>, Vec<StepKey>);
    let mut groups: BTreeMap<Key, Vec<HostId>> = BTreeMap::new();
    for target in targets {
        let key = (
            task_target_state_name(target.run.state),
            target.run.reason.map(task_reason_name),
            target
                .run
                .steps
                .iter()
                .map(|step| {
                    (
                        task_target_state_name(step.state),
                        step.exit_code,
                        step.reason.map(task_reason_name),
                    )
                })
                .collect(),
        );
        groups.entry(key).or_default().push(target.run.host_id);
    }
    let mut result: Vec<_> = groups
        .into_values()
        .filter_map(|hosts| {
            hosts
                .first()
                .copied()
                .map(|representative| TaskOutcomeGroup {
                    representative,
                    hosts,
                })
        })
        .collect();
    result.sort_by_key(|group| group.representative);
    result
}
impl TaskResultDocument {
    pub fn from_record(record: &TaskRunRecord, runner_active: bool) -> Result<Self, TaskError> {
        let plan = record.snapshot();
        let hosts: BTreeMap<_, _> = plan
            .targets()
            .targets()
            .iter()
            .map(|host| (host.id, host))
            .collect();
        let targets = record
            .targets()
            .iter()
            .map(|run| {
                let host = hosts.get(&run.host_id).ok_or(TaskError::UnknownTarget)?;
                Ok(TaskResultTarget {
                    name: host.name.clone(),
                    address: host.address.clone(),
                    port: host.port,
                    profile_id: host.profile_id,
                    run: run.clone(),
                })
            })
            .collect::<Result<Vec<_>, TaskError>>()?;
        let result = Self {
            format: "cshell.task-results".into(),
            version: 1,
            run_id: record.id(),
            name: plan.definition().name.clone(),
            operator: plan.operator().into(),
            retry_of: plan.retry_of(),
            host_revision: plan.targets().host_revision(),
            profile_revision: plan.targets().profile_revision(),
            meta: record.meta().clone(),
            runner_active,
            steps: plan
                .definition()
                .steps
                .iter()
                .map(|step| TaskResultStep {
                    name: step.name.clone(),
                    success_codes: step.success_codes.clone(),
                    idempotent: step.idempotent,
                })
                .collect(),
            summary: summary(&targets),
            outcomes: groups(&targets),
            targets,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.task-results"
            || self.version != 1
            || self.run_id.as_uuid().is_nil()
            || !label(&self.name)
            || !label(&self.operator)
            || self.meta.revision == 0
            || self.meta.updated_at_ms < self.meta.created_at_ms
            || self
                .retry_of
                .is_some_and(|id| id == self.run_id || id.as_uuid().is_nil())
            || self.targets.is_empty()
            || self.targets.len() > crate::MAX_HOST_TARGETS
            || self.steps.is_empty()
            || self.steps.len() > MAX_TASK_STEPS
            || self.steps.iter().any(|step| {
                !label(&step.name) || step.success_codes.is_empty() || step.success_codes.len() > 16
            })
            || self
                .targets
                .windows(2)
                .any(|pair| pair[0].run.host_id >= pair[1].run.host_id)
            || self.targets.iter().any(|target| {
                target.run.host_id.as_uuid().is_nil()
                    || target.profile_id.as_uuid().is_nil()
                    || !label(&target.name)
                    || target.address.is_empty()
                    || target.address.len() > 255
                    || target.address.chars().any(char::is_control)
                    || target.port == 0
                    || target.run.steps.len() != self.steps.len()
                    || task_terminal(self.meta.state) && !target_terminal(target.run.state)
                    || self.meta.state == TaskState::Completed
                        && target.run.state != TargetState::Succeeded
                    || !valid_times(target.run.started_at_ms, target.run.finished_at_ms)
                    || target
                        .run
                        .steps
                        .iter()
                        .any(|step| !valid_times(step.started_at_ms, step.finished_at_ms))
            })
        {
            return Err(TaskError::Invalid);
        }
        if self.summary != summary(&self.targets) || self.outcomes != groups(&self.targets) {
            return Err(TaskError::Invalid);
        }
        encode(self, MAX_TASK_RESULTS_BYTES)?;
        Ok(())
    }
    #[must_use]
    pub fn view(&self) -> TaskRunView {
        TaskRunView {
            id: self.run_id,
            name: self.name.clone(),
            meta: self.meta.clone(),
            targets: self
                .targets
                .iter()
                .map(|target| target.run.clone())
                .collect(),
            runner_active: self.runner_active,
        }
    }
    #[must_use]
    pub fn target(&self, host: HostId) -> Option<&TaskResultTarget> {
        self.targets
            .binary_search_by_key(&host, |target| target.run.host_id)
            .ok()
            .map(|index| &self.targets[index])
    }
}
fn valid_times(start: Option<u64>, finish: Option<u64>) -> bool {
    match (start, finish) {
        (Some(start), Some(finish)) => start <= finish,
        _ => true,
    }
}
pub fn encode_task_results(document: &TaskResultDocument) -> Result<Vec<u8>, TaskError> {
    document.validate()?;
    encode(document, MAX_TASK_RESULTS_BYTES)
}
pub fn decode_task_results(bytes: &[u8]) -> Result<TaskResultDocument, TaskError> {
    let document: TaskResultDocument = decode(bytes, MAX_TASK_RESULTS_BYTES)?;
    document.validate()?;
    Ok(document)
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskResultFilter {
    pub state: Option<TargetState>,
    pub reason: Option<TaskStopReason>,
    pub exit_code: Option<u32>,
    pub step: Option<usize>,
}
impl TaskResultFilter {
    #[must_use]
    pub fn matches(&self, target: &TaskTargetRun) -> bool {
        if self.state.is_some_and(|state| state != target.state) {
            return false;
        }
        let selected = self.step.map(|index| target.steps.get(index));
        if selected == Some(None) {
            return false;
        }
        let steps: Vec<_> = match selected {
            Some(Some(step)) => vec![step],
            _ => target.steps.iter().collect(),
        };
        if self.reason.is_some_and(|reason| {
            (self.step.is_some() || target.reason != Some(reason))
                && !steps.iter().any(|step| step.reason == Some(reason))
        }) {
            return false;
        }
        !self
            .exit_code
            .is_some_and(|code| !steps.iter().any(|step| step.exit_code == Some(code)))
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TaskResultExportFormat {
    #[default]
    Json,
    Csv,
}
struct ExportWriter<'a, W> {
    inner: &'a mut W,
    count: u64,
}
impl<W: Write> Write for ExportWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .count
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("task result export limit"))?;
        if next > MAX_TASK_RESULT_EXPORT_BYTES {
            return Err(io::Error::other("task result export limit"));
        }
        let written = self.inner.write(bytes)?;
        self.count += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
pub fn write_task_result_export(
    document: &TaskResultDocument,
    format: TaskResultExportFormat,
    output: &mut impl Write,
) -> io::Result<()> {
    document
        .validate()
        .map_err(|_| io::Error::other("invalid task result snapshot"))?;
    let mut writer = ExportWriter {
        inner: output,
        count: 0,
    };
    if format == TaskResultExportFormat::Json {
        serde_json::to_writer_pretty(&mut writer, document)
            .map_err(|_| io::Error::other("task result export failed"))?;
        writer.write_all(b"\n")?;
    } else {
        writer.write_all(b"run_id,revision,run_name,operator,run_state,host_id,host_name,address,port,profile_id,target_state,target_start_ms,target_finish_ms,target_elapsed_ms,target_reason,step_index,step_name,step_state,step_start_ms,step_finish_ms,step_elapsed_ms,exit_code,step_reason\r\n")?;
        for target in &document.targets {
            for (index, step) in target.run.steps.iter().enumerate() {
                let cells = [
                    document.run_id.to_string(),
                    document.meta.revision.to_string(),
                    document.name.clone(),
                    document.operator.clone(),
                    format!("{:?}", document.meta.state),
                    target.run.host_id.to_string(),
                    target.name.clone(),
                    target.address.clone(),
                    target.port.to_string(),
                    target.profile_id.to_string(),
                    task_target_state_name(target.run.state).into(),
                    optional(target.run.started_at_ms),
                    optional(target.run.finished_at_ms),
                    elapsed(target.run.started_at_ms, target.run.finished_at_ms),
                    target.run.reason.map(task_reason_name).unwrap_or("").into(),
                    (index + 1).to_string(),
                    document.steps[index].name.clone(),
                    task_target_state_name(step.state).into(),
                    optional(step.started_at_ms),
                    optional(step.finished_at_ms),
                    elapsed(step.started_at_ms, step.finished_at_ms),
                    step.exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_default(),
                    step.reason.map(task_reason_name).unwrap_or("").into(),
                ];
                for (column, cell) in cells.iter().enumerate() {
                    if column > 0 {
                        writer.write_all(b",")?;
                    }
                    csv_cell(
                        &mut writer,
                        cell,
                        matches!(
                            column,
                            0 | 2 | 3 | 4 | 5 | 6 | 7 | 9 | 10 | 14 | 16 | 17 | 22
                        ),
                    )?;
                }
                writer.write_all(b"\r\n")?;
            }
        }
    }
    writer.flush()
}
fn optional(value: Option<u64>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}
fn elapsed(start: Option<u64>, finish: Option<u64>) -> String {
    optional(
        start
            .zip(finish)
            .and_then(|(start, finish)| finish.checked_sub(start)),
    )
}
pub(super) fn csv_cell(writer: &mut impl Write, value: &str, text: bool) -> io::Result<()> {
    writer.write_all(b"\"")?;
    // Prefix every textual cell, including leading-whitespace formulas and UUIDs.
    if text && !value.is_empty() {
        writer.write_all(b"'")?;
    }
    for character in value.chars() {
        if character == '"' {
            writer.write_all(b"\"\"")?;
        } else if character.is_control()
            || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        {
            write!(writer, "\\u{{{:x}}}", character as u32)?;
        } else {
            let mut bytes = [0; 4];
            writer.write_all(character.encode_utf8(&mut bytes).as_bytes())?;
        }
    }
    writer.write_all(b"\"")
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutputComparison {
    pub run_id: TaskRunId,
    pub revision: u64,
    pub left_host: HostId,
    pub right_host: HostId,
    pub step: u32,
    pub stderr: bool,
    pub offset: u64,
    pub left_length: u64,
    pub right_length: u64,
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    pub page_equal: bool,
    pub differing_bytes: u64,
    pub first_difference: Option<u64>,
    pub covers_left_stream: bool,
    pub covers_right_stream: bool,
}
impl fmt::Debug for TaskOutputComparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskOutputComparison")
            .field("run_id", &self.run_id)
            .field("revision", &self.revision)
            .field("left_bytes", &self.left.len())
            .field("right_bytes", &self.right.len())
            .finish_non_exhaustive()
    }
}
impl Drop for TaskOutputComparison {
    fn drop(&mut self) {
        self.left.zeroize();
        self.right.zeroize();
    }
}
impl TaskOutputComparison {
    pub fn recompute(&mut self) -> Result<(), TaskError> {
        if self.run_id.as_uuid().is_nil()
            || self.revision == 0
            || self.left_host.as_uuid().is_nil()
            || self.right_host.as_uuid().is_nil()
            || self.left_host == self.right_host
            || self.step as usize >= MAX_TASK_STEPS
            || self.offset > 1024 * 1024 * 1024
            || self.left.len() > MAX_TASK_COMPARE_PAGE_BYTES
            || self.right.len() > MAX_TASK_COMPARE_PAGE_BYTES
            || !page_bounds(self.offset, self.left.len(), self.left_length)
            || !page_bounds(self.offset, self.right.len(), self.right_length)
        {
            return Err(TaskError::Invalid);
        }
        self.page_equal = self.left == self.right;
        self.differing_bytes = 0;
        self.first_difference = None;
        for index in 0..self.left.len().max(self.right.len()) {
            if self.left.get(index) != self.right.get(index) {
                self.differing_bytes += 1;
                if self.first_difference.is_none() {
                    self.first_difference = self.offset.checked_add(index as u64);
                }
            }
        }
        self.covers_left_stream = self.offset == 0 && self.left.len() as u64 == self.left_length;
        self.covers_right_stream = self.offset == 0 && self.right.len() as u64 == self.right_length;
        Ok(())
    }
}
fn page_bounds(offset: u64, bytes: usize, length: u64) -> bool {
    length <= 1024 * 1024 * 1024
        && if offset >= length {
            bytes == 0
        } else {
            offset
                .checked_add(bytes as u64)
                .is_some_and(|end| end <= length)
        }
}
pub fn decode_task_comparison(bytes: &[u8]) -> Result<TaskOutputComparison, TaskError> {
    let document: TaskOutputComparison = decode(bytes, 1024 * 1024)?;
    let mut expected = document.clone();
    expected.recompute()?;
    if expected != document {
        return Err(TaskError::Invalid);
    }
    Ok(document)
}
