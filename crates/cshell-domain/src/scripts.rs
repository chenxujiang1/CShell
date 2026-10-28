//! Portable automation drafts without runtime grants, session bindings or
//! permission to execute. Recording has no terminal/credential payload fields;
//! hand-authored JavaScript still requires the user's content review.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SCRIPT_WAIT_PATTERN_BYTES: usize = 4096;

pub fn valid_script_wait_pattern(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_SCRIPT_WAIT_PATTERN_BYTES
        && !text.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptCapability {
    TerminalRead,
    TerminalWrite,
    ProfileRead,
    TaskSubmit,
    UiNotify,
}
impl ScriptCapability {
    pub const fn label(self) -> &'static str {
        match self {
            Self::TerminalRead => "terminal_read",
            Self::TerminalWrite => "terminal_write",
            Self::ProfileRead => "profile_read",
            Self::TaskSubmit => "task_submit",
            Self::UiNotify => "ui_notify",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptLimits {
    pub memory_bytes: u64,
    pub stack_bytes: u64,
    pub timeout_ms: u32,
    pub cpu_time_ms: u32,
    pub max_operations: u16,
    pub max_result_bytes: u32,
}
impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 16 * 1024 * 1024,
            stack_bytes: 256 * 1024,
            timeout_ms: 10_000,
            cpu_time_ms: 1000,
            max_operations: 64,
            max_result_bytes: 16 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptParameterKind {
    Text,
    Secret,
    LiteralPattern,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptDocument {
    pub format: String,
    pub version: u32,
    pub script_id: String,
    pub revision: u64,
    pub name: String,
    pub source: String,
    pub requested: BTreeSet<ScriptCapability>,
    pub parameters: BTreeMap<String, ScriptParameterKind>,
    pub limits: ScriptLimits,
}
impl std::fmt::Debug for ScriptDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptDocument")
            .field("script_id", &self.script_id)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingOmission {
    TerminalInput,
    Authentication,
    KeyboardInteractive,
    InputBroadcast,
    SessionChanged,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptRecordingStep {
    SendParameter {
        parameter: String,
        append_newline: bool,
    },
    WaitForParameter {
        parameter: String,
        timeout_ms: u32,
    },
    Sleep {
        milliseconds: u32,
    },
    Notify {
        message: String,
    },
    Omitted {
        reason: RecordingOmission,
    },
}
impl std::fmt::Debug for ScriptRecordingStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SendParameter { .. } => "SendParameter(..)",
            Self::WaitForParameter { .. } => "WaitForParameter(..)",
            Self::Sleep { .. } => "Sleep(..)",
            Self::Notify { .. } => "Notify(..)",
            Self::Omitted { .. } => "Omitted(..)",
        })
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptRecording {
    pub format: String,
    pub version: u32,
    pub script_id: String,
    pub revision: u64,
    pub name: String,
    pub truncated: bool,
    pub steps: Vec<ScriptRecordingStep>,
}
impl Default for ScriptRecording {
    fn default() -> Self {
        Self {
            format: "cshell.script-recording".into(),
            version: 1,
            script_id: "recorded-script".into(),
            revision: 1,
            name: "录制草稿".into(),
            truncated: false,
            steps: Vec::new(),
        }
    }
}
impl std::fmt::Debug for ScriptRecording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptRecording")
            .field("steps", &self.steps.len())
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}
