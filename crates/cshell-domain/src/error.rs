use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ErrorCode {
    InvalidStateTransition,
    InvalidInput,
    DeadlineExceeded,
    Cancelled,
    TransportClosed,
    ProtocolViolation,
    ResourceExhausted,
    IntegrityFailure,
    PermissionDenied,
    Internal,
}

#[derive(Debug, Error, Eq, PartialEq)]
#[error("{code:?}: {message}")]
pub struct DomainError {
    pub code: ErrorCode,
    pub message: String,
}

impl DomainError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
