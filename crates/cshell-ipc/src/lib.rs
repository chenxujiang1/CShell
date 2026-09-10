//! Versioned local IPC protocol and bounded frame codec.

mod codec;
mod discovery;
mod handshake;
mod message;
mod replica;
mod subscription;
pub mod transport;
#[cfg(windows)]
mod windows_security;

pub use codec::{FrameCodec, IpcError, read_envelope, write_envelope};
pub use discovery::{
    DISCOVERY_SCHEMA_VERSION, DiscoveryError, DiscoveryPublication, DiscoveryRecord, RuntimePaths,
    SingleInstanceGuard,
};
pub use handshake::{
    HandshakeError, HandshakePolicy, HandshakeProtocolError, NegotiatedHandshake, client_handshake,
    server_handshake,
};
pub use message::{
    ColorKind, DeltaCodecError, Envelope, FrameDelta, FullFrame, Handshake, HandshakeAck, LogPage,
    LogPageCodecError, LogPageRequest, LogRow, LogStyleSpan, MAX_LOG_PAGE_ROWS,
    MAX_LOG_PAGE_STYLE_SPANS, MAX_LOG_PAGE_TEXT_BYTES, MAX_TERMINAL_INPUT_BYTES, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, SessionCloseRequest, SessionCloseResponse, SessionCreateRequest,
    SessionCreateResponse, SessionListRequest, SessionListResponse, SessionSummary,
    SnapshotCodecError, SnapshotRequest, TerminalCell, TerminalCellWidth, TerminalColor,
    TerminalControlAction, TerminalControlCodecError, TerminalControlResponse,
    TerminalControlStatus, TerminalDeltaPayload, TerminalFramePayload, TerminalInputRequest,
    TerminalKeyEvent, TerminalKeyKind, TerminalPaste, TerminalResizeRequest, TerminalRowPatch,
    TerminalStyle, envelope, features,
};
pub use replica::{
    ApplyResult, ReplicaError, ReplicaState, TerminalReplicaError, TerminalReplicaState,
};
pub use subscription::{ClientFrameUpdate, SubscriptionClientError, TerminalSubscriptionReplica};
