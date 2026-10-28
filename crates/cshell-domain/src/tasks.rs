use crate::TaskId;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

/// Explicit command dialect, never inferred from a Host platform hint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteShellProfile {
    Posix,
    PowerShell,
    /// Cmd must be explicitly selected; dynamic values use a restricted safe alphabet.
    Cmd,
    /// A trusted launcher with exactly one {script} slot, quoted using this dialect.
    Custom {
        launcher: String,
        dialect: TaskShellDialect,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskShellDialect {
    Posix,
    PowerShell,
    Cmd,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskArgument {
    Literal {
        text: String,
    },
    /// An explicitly quoted argument, never interpolated into arbitrary script text.
    Variable {
        name: String,
    },
}
impl fmt::Debug for TaskArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal { text } => f
                .debug_struct("Literal")
                .field("bytes", &text.len())
                .finish(),
            Self::Variable { name } => f.debug_struct("Variable").field("name", name).finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCommand {
    /// Trusted static source. The application never substitutes variables in this text.
    Script { text: String },
    Invocation {
        program: String,
        arguments: Vec<TaskArgument>,
    },
}
impl fmt::Debug for TaskCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Script { text } => f
                .debug_struct("Script")
                .field("bytes", &text.len())
                .finish(),
            Self::Invocation { arguments, .. } => f
                .debug_struct("Invocation")
                .field("arguments", &arguments.len())
                .field("body", &"[REDACTED]")
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStep {
    pub name: String,
    pub command: TaskCommand,
    pub success_codes: BTreeSet<u32>,
    /// A hint for an explicitly requested new run, never an automatic retry grant.
    pub idempotent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPolicy {
    pub concurrency: u16,
    pub batch_size: Option<u16>,
    pub canary_count: u16,
    pub approve_after_canary: bool,
    pub pause_between_batches: bool,
    pub batch_delay_ms: u64,
    pub failure_limit: Option<u16>,
    pub queue_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub step_timeout_ms: u64,
    pub total_timeout_ms: u64,
}
impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            concurrency: 20,
            batch_size: None,
            canary_count: 0,
            approve_after_canary: false,
            pause_between_batches: false,
            batch_delay_ms: 0,
            failure_limit: None,
            queue_timeout_ms: 300_000,
            connect_timeout_ms: 30_000,
            step_timeout_ms: 300_000,
            total_timeout_ms: 3_600_000,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDefinition {
    pub id: TaskId,
    pub name: String,
    pub shell: RemoteShellProfile,
    pub steps: Vec<TaskStep>,
    pub variables: BTreeMap<String, String>,
    pub environment: BTreeMap<String, String>,
    pub policy: TaskPolicy,
}
impl fmt::Debug for TaskDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskDefinition")
            .field("id", &self.id)
            .field("steps", &self.steps.len())
            .field("variables", &self.variables.len())
            .field("environment", &self.environment.len())
            .field("bodies", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}
