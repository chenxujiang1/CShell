//! Stable domain types shared by CShell use cases and adapters.

mod error;
mod id;
mod input;
mod state;

pub use error::{DomainError, ErrorCode};
pub use id::{ConnectionId, SessionId, TargetId, TaskId, TaskRunId};
pub use input::{ControlAction, InputAction, KeyCode, KeyEvent, Modifiers, TerminalSize};
pub use state::{ConnectionState, TargetState, TaskState};
