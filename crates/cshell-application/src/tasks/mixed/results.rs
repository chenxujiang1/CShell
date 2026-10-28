//! Immutable stopped-run result artifacts, containing metadata and native receipts only.
use super::*;
use std::{collections::BTreeMap, io};
pub const MAX_MIXED_RESULT_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedResultSummary {
    pub targets: [u64; 7],
    pub steps: [u64; 7],
    pub command_exit_codes: BTreeMap<u32, u64>,
    pub reasons: BTreeMap<String, u64>,
    pub uploads: u64,
    pub downloads: u64,
    pub file_bytes: u64,
    pub verified: u64,
    pub committed: u64,
    pub adopted: u64,
    pub cleanup_pending: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedResultGroup {
    pub representative: HostId,
    pub hosts: Vec<HostId>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedResultHeader {
    pub run_id: TaskRunId,
    pub name: String,
    pub created_at_ms: u64,
    pub captured_at_ms: u64,
    pub snapshot_sha256: [u8; 32],
    pub host_revision: u64,
    pub profile_revision: u64,
    pub policy: MixedTaskPolicy,
    pub steps: Vec<MixedStepInfo>,
    pub target_count: u64,
    pub summary: MixedResultSummary,
    pub groups: Vec<MixedResultGroup>,
}
impl fmt::Debug for MixedResultHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixedResultHeader")
            .field("run_id", &self.run_id)
            .field("target_count", &self.target_count)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedResultManifest {
    pub format: String,
    pub version: u32,
    pub result_id: TaskRunId,
    pub result_sha256: [u8; 32],
    pub header: MixedResultHeader,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedResultDocument {
    pub format: String,
    pub version: u32,
    pub header: MixedResultHeader,
    pub targets: Vec<MixedTargetView>,
}
impl fmt::Debug for MixedResultDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MixedResultDocument")
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixedResultPage {
    pub format: String,
    pub version: u32,
    pub run_id: TaskRunId,
    pub result_id: TaskRunId,
    pub result_sha256: [u8; 32],
    pub after: Option<HostId>,
    pub next: Option<HostId>,
    pub limit: usize,
    pub targets: Vec<MixedTargetView>,
}
fn reason<T: Serialize>(counts: &mut BTreeMap<String, u64>, prefix: &str, value: Option<T>) {
    if let Some(value) = value
        && let Ok(serde_json::Value::String(name)) = serde_json::to_value(value)
    {
        *counts.entry(format!("{prefix}:{name}")).or_default() += 1;
    }
}
fn summary(targets: &[MixedTargetView]) -> MixedResultSummary {
    let mut result = MixedResultSummary::default();
    for target in targets {
        result.targets[target.run.state.code() as usize] += 1;
        reason(&mut result.reasons, "parent", target.run.reason);
        for step in &target.run.steps {
            result.steps[step.state.code() as usize] += 1;
            match &step.receipt {
                MixedReceipt::Command { target, .. } => {
                    if let Some(code) = target.steps[0].exit_code {
                        *result.command_exit_codes.entry(code).or_default() += 1;
                    }
                    reason(&mut result.reasons, "command", target.reason);
                }
                MixedReceipt::Upload { row } => {
                    result.uploads += 1;
                    result.file_bytes += row.bytes;
                    result.verified += u64::from(row.verified);
                    result.committed += u64::from(row.committed);
                    result.adopted += u64::from(row.adopted);
                    result.cleanup_pending += u64::from(row.committed && !row.temporary_cleaned);
                    reason(&mut result.reasons, "upload", row.reason);
                }
                MixedReceipt::Download { row } => {
                    result.downloads += 1;
                    result.file_bytes += row.bytes;
                    result.verified += u64::from(row.verified);
                    result.committed += u64::from(row.committed);
                    result.adopted += u64::from(row.adopted);
                    result.cleanup_pending += u64::from(row.committed && !row.temporary_cleaned);
                    reason(&mut result.reasons, "download", row.reason);
                }
            }
        }
    }
    result
}
fn groups(targets: &[MixedTargetView]) -> Result<Vec<MixedResultGroup>, TaskError> {
    let mut map: BTreeMap<Vec<u8>, Vec<HostId>> = BTreeMap::new();
    for target in targets {
        let steps:Vec<_>=target.run.steps.iter().map(|step|match &step.receipt{
            MixedReceipt::Command{meta,target}=>serde_json::json!({"kind":"command","state":step.state,"run_state":meta.state,"stop_reason":meta.stop_reason,"target_state":target.state,"child_step_state":target.steps[0].state,"child_step_reason":target.steps[0].reason,"exit_code":target.steps[0].exit_code,"reason":target.reason}),
            MixedReceipt::Upload{row}=>serde_json::json!({"kind":"upload","state":step.state,"phase":row.phase,"intent":row.intent,"bytes":row.bytes,"owned":row.temporary_owned,"verified":row.verified,"committed":row.committed,"adopted":row.adopted,"cleaned":row.temporary_cleaned,"reason":row.reason}),
            MixedReceipt::Download{row}=>serde_json::json!({"kind":"download","state":step.state,"phase":row.phase,"intent":row.intent,"bytes":row.bytes,"content":row.content,"owned":row.temporary_owned,"verified":row.verified,"committed":row.committed,"adopted":row.adopted,"cleaned":row.temporary_cleaned,"reason":row.reason}),
        }).collect();
        let key = serde_json::to_vec(&(
            target.run.state,
            target.run.reason,
            target.run.next_step,
            steps,
        ))
        .map_err(|_| TaskError::Invalid)?;
        map.entry(key).or_default().push(target.run.host_id);
    }
    let mut result: Vec<_> = map
        .into_values()
        .filter_map(|hosts| {
            hosts
                .first()
                .copied()
                .map(|representative| MixedResultGroup {
                    representative,
                    hosts,
                })
        })
        .collect();
    result.sort_by_key(|group| group.representative);
    Ok(result)
}
impl MixedResultHeader {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.policy.validate()?;
        if self.run_id.as_uuid().is_nil()
            || !label(&self.name)
            || self.created_at_ms > i64::MAX as u64 - MAX_TIMEOUT_MS
            || self.captured_at_ms < self.created_at_ms
            || self.captured_at_ms > i64::MAX as u64
            || self.target_count == 0
            || self.target_count > crate::MAX_HOST_TARGETS as u64
            || self.steps.is_empty()
            || self.steps.len() > MAX_TASK_STEPS
            || self
                .summary
                .targets
                .iter()
                .try_fold(0_u64, |sum, value| sum.checked_add(*value))
                != Some(self.target_count)
            || self.summary.targets[0] != 0
            || self.summary.targets[1] != 0
            || self
                .summary
                .steps
                .iter()
                .try_fold(0_u64, |sum, value| sum.checked_add(*value))
                != Some(self.target_count * self.steps.len() as u64)
            || self.summary.steps[1] != 0
            || self.groups.is_empty()
            || self.groups.len() > self.target_count as usize
            || self
                .groups
                .windows(2)
                .any(|pair| pair[0].representative >= pair[1].representative)
        {
            return Err(TaskError::Invalid);
        }
        if self.steps.iter().any(|step| !step.validate())
            || self.policy.concurrency == 0
            || self.policy.concurrency > 50
            || [self.policy.queue_timeout_ms, self.policy.total_timeout_ms]
                .iter()
                .any(|value| *value == 0 || *value > MAX_TIMEOUT_MS)
        {
            return Err(TaskError::Invalid);
        }
        let uploads = self
            .steps
            .iter()
            .filter(|step| matches!(step.details, MixedStepInfoKind::Upload { .. }))
            .count() as u64
            * self.target_count;
        let downloads = self
            .steps
            .iter()
            .filter(|step| matches!(step.details, MixedStepInfoKind::Download { .. }))
            .count() as u64
            * self.target_count;
        let files = uploads + downloads;
        let commands = self.target_count * self.steps.len() as u64 - files;
        let maximum_bytes = self
            .steps
            .iter()
            .map(|step| match &step.details {
                MixedStepInfoKind::Command { .. } => 0,
                MixedStepInfoKind::Upload { content } => content.size,
                MixedStepInfoKind::Download { maximum_bytes } => *maximum_bytes,
            })
            .sum::<u64>()
            * self.target_count;
        if self.summary.uploads != uploads
            || self.summary.downloads != downloads
            || self.summary.file_bytes > maximum_bytes
            || self.summary.adopted > self.summary.committed
            || self.summary.cleanup_pending > self.summary.committed
            || self.summary.committed > self.summary.verified
            || [
                self.summary.verified,
                self.summary.committed,
                self.summary.adopted,
                self.summary.cleanup_pending,
            ]
            .iter()
            .any(|value| *value > files)
            || self
                .summary
                .command_exit_codes
                .values()
                .any(|value| *value == 0)
            || self
                .summary
                .command_exit_codes
                .values()
                .try_fold(0_u64, |sum, value| sum.checked_add(*value))
                .is_none_or(|count| count > commands)
            || self.summary.reasons.iter().any(|(name, count)| {
                name.len() > 64 || name.chars().any(char::is_control) || *count == 0
            })
            || self
                .summary
                .reasons
                .values()
                .try_fold(0_u64, |sum, value| sum.checked_add(*value))
                .is_none_or(|count| count > self.target_count * (self.steps.len() as u64 + 1))
        {
            return Err(TaskError::Invalid);
        }
        let mut ids = BTreeSet::new();
        for group in &self.groups {
            if group.hosts.first() != Some(&group.representative)
                || group.hosts.windows(2).any(|pair| pair[0] >= pair[1])
                || group
                    .hosts
                    .iter()
                    .any(|id| id.as_uuid().is_nil() || !ids.insert(*id))
            {
                return Err(TaskError::Invalid);
            }
        }
        if ids.len() as u64 != self.target_count {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
impl MixedResultDocument {
    pub fn from_rows(
        snapshot: MixedTaskSnapshot,
        rows: Vec<MixedTargetRun>,
        captured_at_ms: u64,
    ) -> Result<Self, TaskError> {
        if rows.len() != snapshot.targets().targets().len()
            || rows
                .iter()
                .any(|row| matches!(row.state, MixedTaskState::Pending | MixedTaskState::Running))
        {
            return Err(TaskError::Transition);
        }
        let mut counts = [0_u64; 7];
        for row in &rows {
            counts[row.state.code() as usize] += 1;
        }
        let mut targets = Vec::with_capacity(rows.len());
        let mut steps = None;
        for chunk in rows.chunks(MAX_MIXED_STATUS_TARGETS) {
            let page = MixedStatusPage::from_data(
                MixedPageData {
                    snapshot: snapshot.clone(),
                    rows: chunk.to_vec(),
                    counts,
                    pending: Vec::new(),
                    after: None,
                    next: None,
                    limit: MAX_MIXED_STATUS_TARGETS,
                },
                false,
            )?;
            steps = Some(page.steps);
            targets.extend(page.targets);
        }
        let header = MixedResultHeader {
            run_id: snapshot.id(),
            name: snapshot.definition().name.clone(),
            created_at_ms: snapshot.created_at_ms(),
            captured_at_ms,
            snapshot_sha256: *snapshot.fingerprint(),
            host_revision: snapshot.targets().host_revision(),
            profile_revision: snapshot.targets().profile_revision(),
            policy: snapshot.definition().policy.clone(),
            steps: steps.ok_or(TaskError::Invalid)?,
            target_count: targets.len() as u64,
            summary: summary(&targets),
            groups: groups(&targets)?,
        };
        let value = Self {
            format: "cshell.mixed-results".into(),
            version: 1,
            header,
            targets,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        self.header.validate()?;
        if self.format != "cshell.mixed-results"
            || self.version != 1
            || self.targets.len() as u64 != self.header.target_count
            || self
                .targets
                .windows(2)
                .any(|pair| pair[0].run.host_id >= pair[1].run.host_id)
        {
            return Err(TaskError::Invalid);
        }
        for chunk in self.targets.chunks(MAX_MIXED_STATUS_TARGETS) {
            validate_targets(&self.header, chunk)?;
        }
        let mut children = BTreeSet::from([self.header.run_id]);
        if self
            .targets
            .iter()
            .flat_map(|target| &target.children)
            .any(|id| !children.insert(*id))
        {
            return Err(TaskError::Invalid);
        }
        if self.header.summary != summary(&self.targets)
            || self.header.groups != groups(&self.targets)?
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn manifest(&self, result_id: TaskRunId) -> Result<MixedResultManifest, TaskError> {
        if result_id.as_uuid().is_nil() || result_id == self.header.run_id {
            return Err(TaskError::Invalid);
        }
        Ok(MixedResultManifest {
            format: "cshell.mixed-result-manifest".into(),
            version: 1,
            result_id,
            result_sha256: self.fingerprint()?,
            header: self.header.clone(),
        })
    }
    pub fn check_snapshot(&self, snapshot: &MixedTaskSnapshot) -> Result<(), TaskError> {
        self.validate()?;
        for targets in self.targets.chunks(MAX_MIXED_STATUS_TARGETS) {
            status_page(&self.header, targets).check_snapshot(snapshot)?;
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<[u8; 32], TaskError> {
        Ok(self.fingerprint_and_size()?.0)
    }
    pub fn fingerprint_and_size(&self) -> Result<([u8; 32], u64), TaskError> {
        self.validate()?;
        let mut writer = HashWriter {
            hash: Sha256::new(),
            bytes: 0,
        };
        serde_json::to_writer(&mut writer, self).map_err(|_| TaskError::Limit)?;
        Ok((writer.hash.finalize().into(), writer.bytes))
    }
    pub fn page(
        &self,
        manifest: &MixedResultManifest,
        after: Option<HostId>,
        limit: usize,
    ) -> Result<MixedResultPage, TaskError> {
        manifest.validate()?;
        if manifest.header != self.header
            || limit == 0
            || limit > MAX_MIXED_STATUS_TARGETS
            || after.is_some_and(|id| id.as_uuid().is_nil())
        {
            return Err(TaskError::Invalid);
        }
        let start = after.map_or(0, |id| {
            self.targets
                .partition_point(|target| target.run.host_id <= id)
        });
        let end = (start + limit).min(self.targets.len());
        let targets = self.targets[start..end].to_vec();
        let next = (end < self.targets.len())
            .then(|| targets.last().map(|target| target.run.host_id))
            .flatten();
        Ok(MixedResultPage {
            format: "cshell.mixed-result-page".into(),
            version: 1,
            run_id: self.header.run_id,
            result_id: manifest.result_id,
            result_sha256: manifest.result_sha256,
            after,
            next,
            limit,
            targets,
        })
    }
}
fn validate_targets(
    header: &MixedResultHeader,
    targets: &[MixedTargetView],
) -> Result<(), TaskError> {
    status_page(header, targets).validate()?;
    if targets.iter().any(|target| {
        target.run.updated_at_ms > header.captured_at_ms
            || matches!(
                target.run.state,
                MixedTaskState::Pending | MixedTaskState::Running
            )
            || target
                .run
                .steps
                .iter()
                .any(|step| step.state == MixedTaskState::Running)
    }) {
        return Err(TaskError::Invalid);
    }
    Ok(())
}
fn status_page(header: &MixedResultHeader, targets: &[MixedTargetView]) -> MixedStatusPage {
    MixedStatusPage {
        format: "cshell.mixed-status".into(),
        version: 1,
        run_id: header.run_id,
        name: header.name.clone(),
        created_at_ms: header.created_at_ms,
        snapshot_sha256: header.snapshot_sha256,
        host_revision: header.host_revision,
        profile_revision: header.profile_revision,
        policy: header.policy.clone(),
        steps: header.steps.clone(),
        total_targets: header.target_count,
        counts: header.summary.targets,
        pending: Vec::new(),
        active: false,
        after: None,
        next: None,
        limit: MAX_MIXED_STATUS_TARGETS,
        targets: targets.to_vec(),
    }
}
impl MixedResultManifest {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.header.validate()?;
        if self.format != "cshell.mixed-result-manifest"
            || self.version != 1
            || self.result_id.as_uuid().is_nil()
            || self.result_id == self.header.run_id
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
pub fn decode_mixed_result_manifest(bytes: &[u8]) -> Result<MixedResultManifest, TaskError> {
    let value: MixedResultManifest = decode(bytes, 256 * 1024)?;
    value.validate()?;
    Ok(value)
}
pub fn decode_mixed_result_page(
    bytes: &[u8],
    manifest: &MixedResultManifest,
) -> Result<MixedResultPage, TaskError> {
    manifest.validate()?;
    let value: MixedResultPage = decode(bytes, MAX_TASK_SNAPSHOT_BYTES)?;
    if value.format != "cshell.mixed-result-page"
        || value.version != 1
        || value.run_id != manifest.header.run_id
        || value.result_id != manifest.result_id
        || value.result_sha256 != manifest.result_sha256
        || value.limit == 0
        || value.limit > MAX_MIXED_STATUS_TARGETS
        || value.targets.len() > value.limit
        || value.after.is_some_and(|id| id.as_uuid().is_nil())
        || value
            .targets
            .iter()
            .any(|target| value.after.is_some_and(|id| target.run.host_id <= id))
        || value
            .targets
            .windows(2)
            .any(|pair| pair[0].run.host_id >= pair[1].run.host_id)
        || value.next.is_some_and(|id| {
            value.targets.len() != value.limit
                || value
                    .targets
                    .last()
                    .is_none_or(|target| target.run.host_id != id)
        })
    {
        return Err(TaskError::Invalid);
    }
    validate_targets(&manifest.header, &value.targets)?;
    Ok(value)
}
pub fn assemble_mixed_results(
    manifest: &MixedResultManifest,
    targets: Vec<MixedTargetView>,
) -> Result<MixedResultDocument, TaskError> {
    manifest.validate()?;
    let document = MixedResultDocument {
        format: "cshell.mixed-results".into(),
        version: 1,
        header: manifest.header.clone(),
        targets,
    };
    if document.fingerprint()? != manifest.result_sha256 {
        return Err(TaskError::Invalid);
    }
    Ok(document)
}
/// Bounds aggregate client memory and requires every page to advance the same capture.
pub struct MixedResultCollector {
    manifest: MixedResultManifest,
    targets: Vec<MixedTargetView>,
    after: Option<HostId>,
    bytes: u64,
    done: bool,
}
impl MixedResultCollector {
    pub fn new(manifest: MixedResultManifest) -> Result<Self, TaskError> {
        manifest.validate()?;
        Ok(Self {
            manifest,
            targets: Vec::new(),
            after: None,
            bytes: 0,
            done: false,
        })
    }
    pub fn after(&self) -> Option<HostId> {
        self.after
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<bool, TaskError> {
        if self.done {
            return Err(TaskError::Invalid);
        }
        let total = self
            .bytes
            .checked_add(bytes.len() as u64)
            .filter(|bytes| *bytes <= MAX_MIXED_RESULT_BYTES)
            .ok_or(TaskError::Limit)?;
        let page = decode_mixed_result_page(bytes, &self.manifest)?;
        if page.after != self.after
            || page.limit != MAX_MIXED_STATUS_TARGETS
            || page.targets.is_empty()
            || self.targets.len() + page.targets.len() > self.manifest.header.target_count as usize
        {
            return Err(TaskError::Invalid);
        }
        let count = self.targets.len() + page.targets.len();
        if page.next.is_some() != (count < self.manifest.header.target_count as usize) {
            return Err(TaskError::Invalid);
        }
        self.bytes = total;
        self.after = page.next;
        self.done = page.next.is_none();
        self.targets.extend(page.targets);
        Ok(self.done)
    }
    pub fn finish(self) -> Result<MixedResultDocument, TaskError> {
        if !self.done {
            return Err(TaskError::Invalid);
        }
        assemble_mixed_results(&self.manifest, self.targets)
    }
}
struct HashWriter {
    hash: Sha256,
    bytes: u64,
}
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("result limit"))?;
        if self.bytes > MAX_MIXED_RESULT_BYTES {
            return Err(io::Error::other("result limit"));
        }
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct LimitedWriter<'a, W> {
    inner: &'a mut W,
    bytes: u64,
}
impl<W: Write> Write for LimitedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .checked_add(bytes.len() as u64)
            .is_none_or(|next| next > MAX_MIXED_RESULT_BYTES)
        {
            return Err(io::Error::other("result limit"));
        }
        let written = self.inner.write(bytes)?;
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
pub fn write_mixed_result_export(
    document: &MixedResultDocument,
    format: TaskResultExportFormat,
    output: &mut impl Write,
) -> io::Result<()> {
    document
        .validate()
        .map_err(|_| io::Error::other("invalid mixed results"))?;
    let mut writer = LimitedWriter {
        inner: output,
        bytes: 0,
    };
    if format == TaskResultExportFormat::Json {
        serde_json::to_writer(&mut writer, document)
            .map_err(|_| io::Error::other("mixed result export failed"))?;
        writer.write_all(b"\n")?;
    } else {
        writer.write_all(b"run_id,run_name,captured_at_ms,host_id,host_name,address,port,target_revision,target_state,target_reason,step_index,step_name,step_state,kind,child_state,exit_code,child_reason,bytes,sha256,owned,verified,committed,adopted,cleaned,child_id,child_revision,intent,content_size,snapshot_sha256\r\n")?;
        for target in &document.targets {
            for (index, step) in target.run.steps.iter().enumerate() {
                let (kind, state, exit, reason, bytes, hash, flags) = match &step.receipt {
                    MixedReceipt::Command { target, .. } => (
                        "command",
                        format!("{:?}", target.state),
                        target.steps[0]
                            .exit_code
                            .map(|code| code.to_string())
                            .unwrap_or_default(),
                        target
                            .reason
                            .map(|reason| format!("{reason:?}"))
                            .unwrap_or_default(),
                        String::new(),
                        String::new(),
                        vec![String::new(); 5],
                    ),
                    MixedReceipt::Upload { row } => {
                        let hash = if let MixedStepInfoKind::Upload { content } =
                            &document.header.steps[index].details
                        {
                            content
                                .sha256
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect()
                        } else {
                            String::new()
                        };
                        (
                            "upload",
                            format!("{:?}", row.phase),
                            String::new(),
                            row.reason
                                .map(|reason| format!("{reason:?}"))
                                .unwrap_or_default(),
                            row.bytes.to_string(),
                            hash,
                            vec![
                                row.temporary_owned.to_string(),
                                row.verified.to_string(),
                                row.committed.to_string(),
                                row.adopted.to_string(),
                                row.temporary_cleaned.to_string(),
                            ],
                        )
                    }
                    MixedReceipt::Download { row } => (
                        "download",
                        format!("{:?}", row.phase),
                        String::new(),
                        row.reason
                            .map(|reason| format!("{reason:?}"))
                            .unwrap_or_default(),
                        row.bytes.to_string(),
                        row.content
                            .map(|content| {
                                content
                                    .sha256
                                    .iter()
                                    .map(|byte| format!("{byte:02x}"))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        vec![
                            row.temporary_owned.to_string(),
                            row.verified.to_string(),
                            row.committed.to_string(),
                            row.adopted.to_string(),
                            row.temporary_cleaned.to_string(),
                        ],
                    ),
                };
                let mut cells = vec![
                    document.header.run_id.to_string(),
                    document.header.name.clone(),
                    document.header.captured_at_ms.to_string(),
                    target.run.host_id.to_string(),
                    target.name.clone(),
                    target.address.clone(),
                    target.port.to_string(),
                    target.run.revision.to_string(),
                    format!("{:?}", target.run.state),
                    target
                        .run
                        .reason
                        .map(|reason| format!("{reason:?}"))
                        .unwrap_or_default(),
                    (index + 1).to_string(),
                    document.header.steps[index].name.clone(),
                    format!("{:?}", step.state),
                    kind.into(),
                    state,
                    exit,
                    reason,
                    bytes,
                    hash,
                ];
                cells.extend(flags);
                let (revision, intent, size) = match &step.receipt {
                    MixedReceipt::Command { meta, .. } => {
                        (meta.revision, String::new(), String::new())
                    }
                    MixedReceipt::Upload { row } => {
                        let size = match &document.header.steps[index].details {
                            MixedStepInfoKind::Upload { content } => content.size.to_string(),
                            _ => String::new(),
                        };
                        (row.revision, format!("{:?}", row.intent), size)
                    }
                    MixedReceipt::Download { row } => (
                        row.revision,
                        format!("{:?}", row.intent),
                        row.content
                            .map(|content| content.size.to_string())
                            .unwrap_or_default(),
                    ),
                };
                cells.extend([
                    target.children[index].to_string(),
                    revision.to_string(),
                    intent,
                    size,
                    document
                        .header
                        .snapshot_sha256
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                ]);
                for (column, cell) in cells.iter().enumerate() {
                    if column > 0 {
                        writer.write_all(b",")?;
                    }
                    super::super::results::csv_cell(
                        &mut writer,
                        cell,
                        !matches!(column, 2 | 6 | 7 | 10 | 15 | 17 | 25 | 27),
                    )?;
                }
                writer.write_all(b"\r\n")?;
            }
        }
    }
    writer.flush()
}
