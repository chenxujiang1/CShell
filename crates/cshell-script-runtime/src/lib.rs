//! Bounded private worker protocol. The supervisor owns parameters and grants;
//! the JavaScript process has only framed stdin/stdout, never daemon credentials.
use cshell_application::{script_document_fingerprint, validate_script_document};
use cshell_domain::{ScriptCapability, ScriptDocument, ScriptRunId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};
#[cfg(feature = "supervisor")]
mod broker;
mod containment;
#[cfg(feature = "supervisor")]
mod recovery;
#[cfg(feature = "supervisor")]
mod supervisor;
#[cfg(feature = "supervisor")]
pub use broker::*;
#[cfg(feature = "supervisor")]
mod control;
pub use containment::{
    enter_worker_access_policy, enter_worker_containment, prepare_worker_lifecycle,
};
#[cfg(feature = "supervisor")]
pub use control::*;
#[cfg(feature = "supervisor")]
pub use recovery::{
    ScriptRecoveryReport, recover_script_resources, recover_script_resources_bounded,
};
#[cfg(feature = "supervisor")]
pub use supervisor::*;

pub const WORKER_VERSION: u32 = 6;
pub const WORKER_STDIO_ARGUMENT: &str = "--private-stdio-v6";
pub const WORKER_HEARTBEAT_MS: u64 = 250;
pub const WORKER_LIVENESS_MS: u64 = 5_000;
pub const MAX_WORKER_FRAME: usize = 384 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ScriptWorkerError {
    #[error("invalid or stale script run request")]
    Invalid,
    #[error("script requests capabilities unavailable in offline execution")]
    Capability,
    #[error("script parameter names or values do not satisfy their bounded declarations")]
    Parameters,
    #[error("script worker protocol or byte budget failed")]
    Protocol,
    #[error("script worker is unavailable")]
    Unavailable,
    #[error("required script process containment could not be installed")]
    Isolation,
    #[error("script worker slots are busy; retry explicitly later")]
    Busy,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptParameterValues {
    pub values: BTreeMap<String, String>,
}
impl ScriptParameterValues {
    pub fn validate(&self, document: &ScriptDocument) -> Result<(), ScriptWorkerError> {
        if self.values.len() != document.parameters.len()
            || self.values.len() > 32
            || self.values.keys().ne(document.parameters.keys())
            || self
                .values
                .values()
                .any(|v| v.len() > 8192 || v.contains('\0'))
            || self.values.values().map(String::len).sum::<usize>() > 32 * 1024
        {
            return Err(ScriptWorkerError::Parameters);
        }
        Ok(())
    }
}
impl Drop for ScriptParameterValues {
    fn drop(&mut self) {
        for value in self.values.values_mut() {
            value.zeroize();
        }
    }
}
impl std::fmt::Debug for ScriptParameterValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptParameterValues(..)")
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerStart {
    pub version: u32,
    pub run_id: ScriptRunId,
    pub document: ScriptDocument,
    pub document_sha256: [u8; 32],
    pub selected: BTreeSet<ScriptCapability>,
    pub live: bool,
}
impl WorkerStart {
    pub fn validate(&self) -> Result<(), ScriptWorkerError> {
        validate_script_document(&self.document).map_err(|_| ScriptWorkerError::Invalid)?;
        if self.version != WORKER_VERSION
            || self.run_id.as_uuid().is_nil()
            || script_document_fingerprint(&self.document)
                .map_err(|_| ScriptWorkerError::Invalid)?
                != self.document_sha256
            || self.selected != self.document.requested
        {
            return Err(ScriptWorkerError::Invalid);
        }
        if !self.live
            && self
                .selected
                .iter()
                .any(|c| *c != ScriptCapability::UiNotify)
        {
            return Err(ScriptWorkerError::Capability);
        }
        Ok(())
    }
}
impl Drop for WorkerStart {
    fn drop(&mut self) {
        self.document.source.zeroize();
    }
}
impl std::fmt::Debug for WorkerStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerStart")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCall {
    Parameter { name: String },
    Sleep { milliseconds: u32 },
    Notify { message: String },
    TerminalSend { text: String, append_newline: bool },
    TerminalSendParameter { name: String, append_newline: bool },
    TerminalWait { text: String, timeout_ms: u32 },
    TerminalWaitParameter { name: String, timeout_ms: u32 },
    ProfileRead,
    TaskSubmit { binding: String },
    Unsupported,
}
impl Drop for WorkerCall {
    fn drop(&mut self) {
        match self {
            Self::Parameter { name }
            | Self::TerminalSendParameter { name, .. }
            | Self::TerminalWaitParameter { name, .. } => name.zeroize(),
            Self::TerminalSend { text, .. } | Self::TerminalWait { text, .. } => text.zeroize(),
            Self::TaskSubmit { binding } => binding.zeroize(),
            Self::Notify { message } => message.zeroize(),
            _ => {}
        }
    }
}
impl std::fmt::Debug for WorkerCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkerCall(..)")
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFinish {
    Completed,
    ScriptFailed,
    BudgetExceeded,
    ProtocolFailed,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerEvent {
    Ready {
        run_id: ScriptRunId,
        sequence: u32,
        containment: WorkerContainment,
        sandbox: WorkerSandbox,
    },
    Call {
        run_id: ScriptRunId,
        sequence: u32,
        request: WorkerCall,
    },
    Finished {
        run_id: ScriptRunId,
        sequence: u32,
        status: WorkerFinish,
    },
}
impl std::fmt::Debug for WorkerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkerEvent(..)")
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostResult {
    Value { value: String },
    Boolean { value: bool },
    Ok,
    Denied,
}
impl Drop for HostResult {
    fn drop(&mut self) {
        if let Self::Value { value } = self {
            value.zeroize();
        }
    }
}
impl std::fmt::Debug for HostResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostResult(..)")
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostReply {
    pub run_id: ScriptRunId,
    pub sequence: u32,
    pub result: HostResult,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostFrame {
    Start { start: WorkerStart },
    Reply { reply: HostReply },
    Heartbeat { run_id: ScriptRunId },
}
/// Serialization-only borrowed views keep source/parameter containers owned by
/// their original request. The wire representation is identical to HostFrame.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostFrameRef<'a> {
    Start { start: &'a WorkerStart },
    Reply { reply: &'a HostReply },
    Heartbeat { run_id: ScriptRunId },
}
impl std::fmt::Debug for HostFrameRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostFrameRef(..)")
    }
}
impl std::fmt::Debug for HostFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostFrame(..)")
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerContainment {
    WindowsJobV1,
    LinuxRlimitV1,
    MacosRlimitV1,
}
impl WorkerContainment {
    pub fn current_platform() -> Result<Self, ScriptWorkerError> {
        if cfg!(windows) {
            Ok(Self::WindowsJobV1)
        } else if cfg!(target_os = "linux") {
            Ok(Self::LinuxRlimitV1)
        } else if cfg!(target_os = "macos") {
            Ok(Self::MacosRlimitV1)
        } else {
            Err(ScriptWorkerError::Isolation)
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSandbox {
    /// Native LP64 Linux filter synchronized over all existing worker threads.
    LinuxSeccompV1,
    WindowsLpacV1,
    /// Fixed default-deny custom Seatbelt profile installed before threads.
    MacosSeatbeltV1,
}
impl WorkerSandbox {
    pub fn current_platform() -> Result<Self, ScriptWorkerError> {
        if cfg!(all(
            target_os = "linux",
            target_pointer_width = "64",
            target_endian = "little",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )) {
            Ok(Self::LinuxSeccompV1)
        } else if cfg!(windows) {
            Ok(Self::WindowsLpacV1)
        } else if cfg!(target_os = "macos") {
            Ok(Self::MacosSeatbeltV1)
        } else {
            Err(ScriptWorkerError::Isolation)
        }
    }
}
impl std::fmt::Debug for HostReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostReply(..)")
    }
}

pub fn encode_frame<T: Serialize>(value: &T) -> Result<Zeroizing<Vec<u8>>, ScriptWorkerError> {
    let bytes = Zeroizing::new(serde_json::to_vec(value).map_err(|_| ScriptWorkerError::Protocol)?);
    if bytes.is_empty() || bytes.len() > MAX_WORKER_FRAME {
        return Err(ScriptWorkerError::Protocol);
    }
    Ok(bytes)
}
pub fn write_frame<T: Serialize>(
    writer: &mut impl Write,
    value: &T,
) -> Result<(), ScriptWorkerError> {
    let bytes = encode_frame(value)?;
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .map_err(|_| ScriptWorkerError::Protocol)?;
    writer
        .write_all(&bytes)
        .map_err(|_| ScriptWorkerError::Protocol)?;
    writer.flush().map_err(|_| ScriptWorkerError::Protocol)
}
pub fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<T, ScriptWorkerError> {
    let mut prefix = [0u8; 4];
    reader
        .read_exact(&mut prefix)
        .map_err(|_| ScriptWorkerError::Protocol)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_WORKER_FRAME {
        return Err(ScriptWorkerError::Protocol);
    }
    let mut bytes = Zeroizing::new(vec![0u8; length]);
    reader
        .read_exact(&mut bytes)
        .map_err(|_| ScriptWorkerError::Protocol)?;
    serde_json::from_slice(&bytes).map_err(|_| ScriptWorkerError::Protocol)
}
