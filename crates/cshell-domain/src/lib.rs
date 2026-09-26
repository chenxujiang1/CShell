//! Stable domain types shared by CShell use cases and adapters.

mod error;
mod id;
mod input;
mod profile;
mod state;

pub use error::{DomainError, ErrorCode};
pub use id::{ConnectionId, FolderId, ProfileId, SessionId, TargetId, TaskId, TaskRunId};
pub use input::{ControlAction, InputAction, KeyCode, KeyEvent, Modifiers, TerminalSize};
pub use profile::{
    LocalClosePolicy, LocalConnectionRecord, LocalWorkingDirectory, ProfileFolder, ProfileKind,
    ProfileRecord, ResolvedField, ResolvedTerminalSettings, SettingSource, SshAgentBackend,
    SshAuthMethod, SshConnectionRecord, SshRoute, TerminalDefaults, TerminalOverrides,
    reserved_local_environment_name,
};
pub use state::{ConnectionState, TargetState, TaskState};
