//! Stable domain types shared by CShell use cases and adapters.

mod clipboard;
mod error;
pub use clipboard::{ClipboardHost, ClipboardTrust};
mod hosts;
mod id;
mod input;
pub use hosts::*;
pub use id::{HostGroupId, HostId, HostTargetSnapshotId};
mod quick_commands;
pub use quick_commands::*;
mod profile;
pub use profile::SftpVerification;
mod ssh_provider;
mod ssh_route;
pub use ssh_provider::*;
pub use ssh_route::{MAX_SSH_JUMP_HOPS, SshRouteError, ssh_jump_chain};
mod scripts;
mod state;
pub use scripts::*;
mod terminal_tools;
mod theme;
pub use terminal_tools::*;
pub use theme::{TerminalPalette, ThemeRecord};
mod workspace;

pub use error::{DomainError, ErrorCode};
pub use id::HostAuthorityId;
pub use id::ScriptRunId;
pub use id::SftpQueueItemId;
pub use id::TaskGateId;
pub use id::TerminalPolicyId;
pub use id::TunnelId;
pub use id::{ConnectionId, FolderId, ProfileId, SessionId, TargetId, TaskId, TaskRunId};
mod tasks;
pub use id::{PaneId, TabGroupId, TabId, WorkspaceWindowId};
pub use id::{QuickCommandFolderId, QuickCommandId};
pub use id::{SyncInputGroupId, SyncInputSendId, SyncInputSnapshotId};
pub use input::{ControlAction, InputAction, KeyCode, KeyEvent, Modifiers, TerminalSize, MouseButton, MouseEvent, MouseEventKind};
pub use profile::{
    DEFAULT_TERMINAL_FONT_PIXELS, MIN_TERMINAL_FONT_PIXELS, MAX_TERMINAL_FONT_PIXELS,
    LocalClosePolicy, LocalConnectionRecord, LocalWorkingDirectory, ProfileFolder, ProfileKind,
    ProfileRecord, ResolvedField, ResolvedTerminalSettings, SettingSource, SshAgentBackend,
    SshAuthMethod, SshAuthStep, SshConnectionRecord, SshConnectionOptions, SshRoute, TerminalDefaults, TerminalOverrides,
    reserved_local_environment_name,
};
pub use state::{ConnectionState, TargetState, TaskState};
pub use tasks::*;
pub use workspace::*;

mod shell_history;
pub use shell_history::{ShellCommand, ShellHistory};
mod connection_diagnostics;
pub use connection_diagnostics::*;
