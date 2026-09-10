//! Use-case ports. Infrastructure crates implement these interfaces.

use async_trait::async_trait;
use cshell_domain::{InputAction, SessionId, TerminalSize};
use std::fmt::Debug;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseMode {
    DetachView,
    Graceful,
    Force,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportError {
    pub retryable: bool,
    pub message: String,
}

#[async_trait]
pub trait TerminalTransport: Debug + Send + Sync {
    fn session_id(&self) -> SessionId;

    async fn write_input(&self, action: InputAction) -> Result<(), TransportError>;

    async fn resize(&self, size: TerminalSize) -> Result<(), TransportError>;

    async fn close(&self, mode: CloseMode) -> Result<(), TransportError>;
}
