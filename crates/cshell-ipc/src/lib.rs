//! Versioned local IPC protocol and bounded frame codec.

mod codec;
mod discovery;
mod handshake;
mod message;
mod profile;
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
    ColorKind, DeltaCodecError, Envelope, FrameDelta, FullFrame, Handshake, HandshakeAck,
    HistorySearchCodecError, HistorySearchDirection, HistorySearchMatch, HistorySearchRequest,
    HistorySearchResult, LogPage, LogPageCodecError, LogPageRequest, LogRow, LogStyleSpan,
    MAX_HISTORY_SEARCH_MATCHES, MAX_HISTORY_SEARCH_QUERY_BYTES, MAX_HISTORY_SEARCH_SCAN_LINES,
    MAX_LOG_PAGE_ROWS, MAX_LOG_PAGE_STYLE_SPANS, MAX_LOG_PAGE_TEXT_BYTES, MAX_TERMINAL_INPUT_BYTES,
    PROTOCOL_MAJOR, PROTOCOL_MINOR, SessionCloseRequest, SessionCloseResponse,
    SessionCreateRequest, SessionCreateResponse, SessionListRequest, SessionListResponse,
    SessionSummary, SnapshotCodecError, SnapshotRequest, TerminalCell, TerminalCellWidth,
    TerminalColor, TerminalControlAction, TerminalControlCodecError, TerminalControlResponse,
    TerminalControlStatus, TerminalCursorAppearance, TerminalCursorShape, TerminalDeltaPayload,
    TerminalFramePayload, TerminalInputRequest, TerminalKeyEvent, TerminalKeyKind, TerminalPaste,
    TerminalResizeRequest, TerminalRowPatch, TerminalStyle, envelope, features,
};
pub use replica::{
    ApplyResult, ReplicaError, ReplicaState, TerminalReplicaError, TerminalReplicaState,
};
pub use subscription::{ClientFrameUpdate, SubscriptionClientError, TerminalSubscriptionReplica};

pub use profile::{
    MAX_PROFILE_CONTROL_CHANGES, ProfileCatalogData, ProfileChange, ProfileCodecError,
    ProfileDefaultsData, ProfileFolderData, ProfileImportAction, ProfileImportItemData,
    ProfileImportItemKind, ProfileImportPolicy, ProfileImportPreviewData, ProfileKindData,
    ProfileOperation, ProfileRecordData, ProfileRequest, ProfileResponse, ProfileStatus,
    ProfileTerminalOverridesData, SshConnectionData, decode_id, profile_change,
};
