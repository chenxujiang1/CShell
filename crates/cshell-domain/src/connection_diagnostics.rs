//! Bounded, credential-free observations of one connection attempt.
use crate::SessionId;
use serde::{Deserialize, Serialize};
pub const MAX_CONNECTION_STEPS: usize = 128;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ConnectionStage {
    Preparing,
    Credentials,
    Resolving,
    Tcp,
    Proxy,
    JumpChannel,
    Handshake,
    HostKey,
    Authentication,
    WaitingForInput,
    TerminalChannel,
    Pty,
    Shell,
    SftpChannel,
    SftpSubsystem,
    InitialDirectory,
    ReusingTransport,
    Ready,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ConnectionOutcome {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Uncertain,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionStep {
    pub stage: ConnectionStage,
    pub hop: u8,
    pub endpoint: String,
    pub started_ms: u64,
    pub elapsed_ms: u64,
    pub outcome: ConnectionOutcome,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionDiagnostic {
    pub attempt: SessionId,
    pub elapsed_ms: u64,
    pub outcome: ConnectionOutcome,
    pub failure_code: i32,
    pub truncated: bool,
    pub steps: Vec<ConnectionStep>,
}
impl ConnectionDiagnostic {
    pub fn validate(&self) -> bool {
        !self.attempt.as_uuid().is_nil()
            && !self.steps.is_empty()
            && self.steps.len() <= MAX_CONNECTION_STEPS
            && self.elapsed_ms <= 86_400_000
            && self.steps.last().is_some_and(|s| {
                s.outcome == self.outcome
                    && s.started_ms.saturating_add(s.elapsed_ms) == self.elapsed_ms
                    && (self.outcome != ConnectionOutcome::Succeeded
                        || s.stage == ConnectionStage::Ready)
            })
            && self.steps.iter().enumerate().all(|(i, s)| {
                s.hop <= 16
                    && s.endpoint.len() <= 320
                    && !s.endpoint.chars().any(char::is_control)
                    && s.started_ms.saturating_add(s.elapsed_ms) <= self.elapsed_ms
                    && (i == 0 || s.started_ms >= self.steps[i - 1].started_ms)
                    && (i + 1 == self.steps.len() || s.outcome == ConnectionOutcome::Succeeded)
            })
    }
}
