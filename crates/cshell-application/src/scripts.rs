//! Draft validation, inert permission plans and deliberately payload-free input
//! recording. Runtime grants and QuickJS execution are separate use cases.
use cshell_domain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const MAX_SCRIPT_DOCUMENT_BYTES: usize = 128 * 1024;
pub const MAX_SCRIPT_SOURCE_BYTES: usize = 64 * 1024;
pub const MAX_SCRIPT_RECORDING_STEPS: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ScriptDraftError {
    #[error("script draft exceeds its byte, parameter, operation or step budget")]
    Limit,
    #[error("invalid script draft format, identity, metadata or limits")]
    Invalid,
    #[error("invalid script draft JSON")]
    Json,
    #[error(
        "recording contains omitted input or truncation; edit and review every gap before generating"
    )]
    NeedsReview,
    #[error("a recording parameter has incompatible uses")]
    ParameterConflict,
    #[error("selected capabilities exceed the script's declared request")]
    Capability,
    #[error("recording is stopped or has no bound session")]
    Stopped,
}
fn identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
fn parameter(value: &str) -> bool {
    identity(value)
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && !matches!(value, "constructor" | "prototype")
}
fn title(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
fn bounded_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ScriptDraftError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ScriptDraftError::Json)?;
    if bytes.len() > MAX_SCRIPT_DOCUMENT_BYTES {
        return Err(ScriptDraftError::Limit);
    }
    Ok(bytes)
}
fn limits(value: ScriptLimits) -> bool {
    (8 * 1024 * 1024..=64 * 1024 * 1024).contains(&value.memory_bytes)
        && (64 * 1024..=1024 * 1024).contains(&value.stack_bytes)
        && (100..=30_000).contains(&value.timeout_ms)
        && (100..=10_000).contains(&value.cpu_time_ms)
        && value.cpu_time_ms <= value.timeout_ms
        && (1..=256).contains(&value.max_operations)
        && (1024..=64 * 1024).contains(&value.max_result_bytes)
}
pub fn validate_script_document(document: &ScriptDocument) -> Result<(), ScriptDraftError> {
    if document.format != "cshell.script"
        || document.version != 1
        || document.revision == 0
        || !identity(&document.script_id)
        || !title(&document.name)
        || !limits(document.limits)
        || document.source.trim().is_empty()
        || document.source.contains('\0')
    {
        return Err(ScriptDraftError::Invalid);
    }
    if document.source.len() > MAX_SCRIPT_SOURCE_BYTES || document.parameters.len() > 32 {
        return Err(ScriptDraftError::Limit);
    }
    if document.parameters.keys().any(|key| !parameter(key)) {
        return Err(ScriptDraftError::Invalid);
    }
    bounded_json(document)?;
    Ok(())
}
pub fn decode_script_document(bytes: &[u8]) -> Result<ScriptDocument, ScriptDraftError> {
    if bytes.len() > MAX_SCRIPT_DOCUMENT_BYTES {
        return Err(ScriptDraftError::Limit);
    }
    let value = serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
        .map_err(|_| ScriptDraftError::Json)?;
    validate_script_document(&value)?;
    Ok(value)
}
pub fn script_document_fingerprint(
    document: &ScriptDocument,
) -> Result<[u8; 32], ScriptDraftError> {
    validate_script_document(document)?;
    Ok(Sha256::digest(bounded_json(document)?).into())
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptPermissionPlan {
    pub format: String,
    pub version: u32,
    pub api_version: u32,
    pub script_id: String,
    pub revision: u64,
    pub document_sha256: [u8; 32],
    pub requested: BTreeSet<ScriptCapability>,
    pub selected: BTreeSet<ScriptCapability>,
    pub missing: BTreeSet<ScriptCapability>,
    pub parameters: BTreeMap<String, ScriptParameterKind>,
    pub limits: ScriptLimits,
}
/// A plan is an offline comparison, never an execution token or stored grant.
pub fn plan_script_permissions(
    document: &ScriptDocument,
    selected: &BTreeSet<ScriptCapability>,
) -> Result<ScriptPermissionPlan, ScriptDraftError> {
    let digest = script_document_fingerprint(document)?;
    if !selected.is_subset(&document.requested) {
        return Err(ScriptDraftError::Capability);
    }
    Ok(ScriptPermissionPlan {
        format: "cshell.script-permission-plan".into(),
        version: 1,
        api_version: 1,
        script_id: document.script_id.clone(),
        revision: document.revision,
        document_sha256: digest,
        requested: document.requested.clone(),
        selected: selected.clone(),
        missing: document.requested.difference(selected).copied().collect(),
        parameters: document.parameters.clone(),
        limits: document.limits,
    })
}
fn validate_step(step: &ScriptRecordingStep) -> Result<(), ScriptDraftError> {
    let valid = match step {
        ScriptRecordingStep::SendParameter { parameter: p, .. } => parameter(p),
        ScriptRecordingStep::WaitForParameter {
            parameter: p,
            timeout_ms,
        } => parameter(p) && (100..=30_000).contains(timeout_ms),
        ScriptRecordingStep::Sleep { milliseconds } => (1..=30_000).contains(milliseconds),
        ScriptRecordingStep::Notify { message } => title(message),
        ScriptRecordingStep::Omitted { .. } => true,
    };
    if valid {
        Ok(())
    } else {
        Err(ScriptDraftError::Invalid)
    }
}
pub fn validate_script_recording(recording: &ScriptRecording) -> Result<(), ScriptDraftError> {
    if recording.format != "cshell.script-recording"
        || recording.version != 1
        || recording.revision == 0
        || !identity(&recording.script_id)
        || !title(&recording.name)
    {
        return Err(ScriptDraftError::Invalid);
    }
    if recording.steps.len() > MAX_SCRIPT_RECORDING_STEPS {
        return Err(ScriptDraftError::Limit);
    }
    for step in &recording.steps {
        validate_step(step)?;
    }
    bounded_json(recording)?;
    Ok(())
}
pub fn decode_script_recording(bytes: &[u8]) -> Result<ScriptRecording, ScriptDraftError> {
    if bytes.len() > MAX_SCRIPT_DOCUMENT_BYTES {
        return Err(ScriptDraftError::Limit);
    }
    let value = serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
        .map_err(|_| ScriptDraftError::Json)?;
    validate_script_recording(&value)?;
    Ok(value)
}
fn declare(
    parameters: &mut BTreeMap<String, ScriptParameterKind>,
    name: &str,
    kind: ScriptParameterKind,
) -> Result<(), ScriptDraftError> {
    if parameters.get(name).is_some_and(|old| *old != kind) {
        return Err(ScriptDraftError::ParameterConflict);
    }
    parameters.insert(name.into(), kind);
    if parameters.len() > 32 {
        return Err(ScriptDraftError::Limit);
    }
    Ok(())
}
fn quoted(value: &str) -> Result<String, ScriptDraftError> {
    // JSON strings are JavaScript string literals too; explicitly escape the
    // two Unicode line separators for engines/consumers with older grammars.
    serde_json::to_string(value)
        .map(|s| {
            s.replace('\u{2028}', "\\u2028")
                .replace('\u{2029}', "\\u2029")
        })
        .map_err(|_| ScriptDraftError::Json)
}
pub fn render_script_recording(
    recording: &ScriptRecording,
) -> Result<ScriptDocument, ScriptDraftError> {
    validate_script_recording(recording)?;
    if recording.truncated
        || recording
            .steps
            .iter()
            .any(|s| matches!(s, ScriptRecordingStep::Omitted { .. }))
    {
        return Err(ScriptDraftError::NeedsReview);
    }
    if recording.steps.is_empty() {
        return Err(ScriptDraftError::Invalid);
    }
    let mut parameters = BTreeMap::new();
    let mut requested = BTreeSet::new();
    let mut source = String::from(
        "// CShell automation API v1 draft. Review before requesting a runtime grant.\n(async () => {\n",
    );
    let mut duration = 0u32;
    let mut operations = 0u16;
    for step in &recording.steps {
        let line = match step {
            ScriptRecordingStep::SendParameter {
                parameter,
                append_newline,
            } => {
                declare(&mut parameters, parameter, ScriptParameterKind::Secret)?;
                requested.insert(ScriptCapability::TerminalWrite);
                operations = operations.checked_add(1).ok_or(ScriptDraftError::Limit)?;
                format!(
                    "  await cshell.terminal.sendParameter({{name: {}, append_newline: {append_newline}}});\n",
                    quoted(parameter)?
                )
            }
            ScriptRecordingStep::WaitForParameter {
                parameter,
                timeout_ms,
            } => {
                declare(
                    &mut parameters,
                    parameter,
                    ScriptParameterKind::LiteralPattern,
                )?;
                requested.insert(ScriptCapability::TerminalRead);
                duration = duration
                    .checked_add(*timeout_ms)
                    .ok_or(ScriptDraftError::Limit)?;
                operations = operations.checked_add(1).ok_or(ScriptDraftError::Limit)?;
                format!(
                    "  await cshell.terminal.waitForParameter({{name: {}, timeout_ms: {timeout_ms}}});\n",
                    quoted(parameter)?
                )
            }
            ScriptRecordingStep::Sleep { milliseconds } => {
                duration = duration
                    .checked_add(*milliseconds)
                    .ok_or(ScriptDraftError::Limit)?;
                operations = operations.checked_add(1).ok_or(ScriptDraftError::Limit)?;
                format!("  await cshell.clock.sleep({milliseconds});\n")
            }
            ScriptRecordingStep::Notify { message } => {
                requested.insert(ScriptCapability::UiNotify);
                operations = operations.checked_add(1).ok_or(ScriptDraftError::Limit)?;
                format!("  await cshell.ui.notify({});\n", quoted(message)?)
            }
            ScriptRecordingStep::Omitted { .. } => return Err(ScriptDraftError::NeedsReview),
        };
        source.push_str(&line);
    }
    source.push_str("})();\n");
    let timeout_ms = duration
        .checked_add(1000)
        .ok_or(ScriptDraftError::Limit)?
        .max(1000);
    let document = ScriptDocument {
        format: "cshell.script".into(),
        version: 1,
        script_id: recording.script_id.clone(),
        revision: recording.revision,
        name: recording.name.clone(),
        source,
        requested,
        parameters,
        limits: ScriptLimits {
            timeout_ms,
            max_operations: operations,
            ..Default::default()
        },
    };
    validate_script_document(&document)?;
    Ok(document)
}

/// A recorder never accepts terminal payloads, authentication answers or output.
/// Consecutive input omissions coalesce, so no keystroke count/timing is retained.
#[derive(Debug, Default)]
pub struct ScriptRecorder {
    session: Option<SessionId>,
    document: ScriptRecording,
}
impl ScriptRecorder {
    pub fn start(session: SessionId) -> Result<Self, ScriptDraftError> {
        if session.as_uuid().is_nil() {
            return Err(ScriptDraftError::Invalid);
        }
        Ok(Self {
            session: Some(session),
            document: ScriptRecording::default(),
        })
    }
    pub fn active(&self) -> bool {
        self.session.is_some()
    }
    pub fn document(&self) -> &ScriptRecording {
        &self.document
    }
    pub fn stop(&mut self) {
        self.session = None;
    }
    pub fn observe_session(&mut self, session: Option<SessionId>) {
        if self.active() && session != self.session {
            if self.document.steps.len() < MAX_SCRIPT_RECORDING_STEPS {
                self.document.steps.push(ScriptRecordingStep::Omitted {
                    reason: RecordingOmission::SessionChanged,
                });
            } else {
                self.document.truncated = true;
            }
            self.stop();
        }
    }
    pub fn record_omission(
        &mut self,
        session: SessionId,
        reason: RecordingOmission,
    ) -> Result<(), ScriptDraftError> {
        self.observe_session(Some(session));
        if !self.active() {
            return Err(ScriptDraftError::Stopped);
        }
        if matches!(
            self.document.steps.last(),
            Some(ScriptRecordingStep::Omitted { .. })
        ) {
            return Ok(());
        }
        self.append(ScriptRecordingStep::Omitted { reason })
    }
    pub fn append(&mut self, step: ScriptRecordingStep) -> Result<(), ScriptDraftError> {
        if !self.active() {
            return Err(ScriptDraftError::Stopped);
        }
        validate_step(&step)?;
        if self.document.steps.len() >= MAX_SCRIPT_RECORDING_STEPS {
            self.document.truncated = true;
            self.stop();
            return Err(ScriptDraftError::Limit);
        }
        self.document.steps.push(step);
        if self.document.steps.len() == MAX_SCRIPT_RECORDING_STEPS {
            self.document.truncated = true;
            self.stop();
            return Err(ScriptDraftError::Limit);
        }
        Ok(())
    }
}
