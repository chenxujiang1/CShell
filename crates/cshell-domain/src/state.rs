use crate::{DomainError, ErrorCode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ConnectionState {
    Idle,
    Resolving,
    Connecting,
    Negotiating,
    VerifyingHostKey,
    Authenticating,
    Ready,
    Reconnecting,
    Closing,
    Closed,
    Failed,
}

impl ConnectionState {
    pub fn transition(self, next: Self) -> Result<Self, DomainError> {
        let valid = matches!(
            (self, next),
            (Self::Idle, Self::Resolving)
                | (Self::Resolving, Self::Connecting)
                | (Self::Connecting, Self::Negotiating)
                | (Self::Negotiating, Self::VerifyingHostKey)
                | (Self::VerifyingHostKey, Self::Authenticating)
                | (Self::Authenticating, Self::Ready)
                | (Self::Ready, Self::Reconnecting)
                | (Self::Reconnecting, Self::Resolving)
                | (Self::Ready | Self::Reconnecting, Self::Closing)
                | (Self::Closing, Self::Closed)
                | (_, Self::Failed)
        );
        valid.then_some(next).ok_or_else(|| {
            DomainError::new(
                ErrorCode::InvalidStateTransition,
                format!("connection {self:?} -> {next:?}"),
            )
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskState {
    Draft,
    Validating,
    AwaitingConfirmation,
    Queued,
    Running,
    Cancelling,
    Completed,
    PartialFailed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn transition(self, next: Self) -> Result<Self, DomainError> {
        let valid = matches!(
            (self, next),
            (Self::Draft, Self::Validating)
                | (
                    Self::Validating,
                    Self::AwaitingConfirmation | Self::Queued | Self::Failed
                )
                | (Self::AwaitingConfirmation, Self::Queued | Self::Cancelled)
                | (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Cancelling | Self::Completed | Self::PartialFailed | Self::Failed
                )
                | (Self::Cancelling, Self::Cancelled | Self::PartialFailed)
        );
        valid.then_some(next).ok_or_else(|| {
            DomainError::new(
                ErrorCode::InvalidStateTransition,
                format!("task {self:?} -> {next:?}"),
            )
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TargetState {
    Pending,
    Connecting,
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Unknown,
}

impl TargetState {
    #[must_use]
    pub const fn safe_to_retry_automatically(self) -> bool {
        matches!(self, Self::Pending | Self::Connecting)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionState, TargetState, TaskState};

    #[test]
    fn connection_happy_path_is_explicit() {
        let state = ConnectionState::Idle
            .transition(ConnectionState::Resolving)
            .and_then(|value| value.transition(ConnectionState::Connecting))
            .and_then(|value| value.transition(ConnectionState::Negotiating));
        assert_eq!(state, Ok(ConnectionState::Negotiating));
    }

    #[test]
    fn invalid_transition_is_rejected() {
        assert!(TaskState::Draft.transition(TaskState::Completed).is_err());
    }

    #[test]
    fn unknown_is_never_automatically_retried() {
        assert!(!TargetState::Unknown.safe_to_retry_automatically());
    }
}
