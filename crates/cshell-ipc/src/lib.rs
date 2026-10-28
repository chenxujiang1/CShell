//! Versioned local IPC protocol and bounded frame codec.

mod clipboard;
mod zmodem;
pub use zmodem::*;
mod clipboard_policy;
pub use clipboard_policy::*;
mod codec;
pub use clipboard::*;
mod discovery;
mod handshake;
mod host_authorities;
mod message;
pub use message::TerminalMouseEvent;
mod profile;
mod profile_batch;
mod terminal_policies;
pub use profile_batch::*;
pub use terminal_policies::*;
mod replica;
mod scripts;
mod sftp;
mod ssh_providers;
mod terminal_tools;
pub use scripts::*;
pub use terminal_tools::*;
mod tunnels;
pub use host_authorities::*;
pub use ssh_providers::*;
pub use tunnels::*;
mod subscription;
mod workspace;
pub use workspace::*;
mod quick_commands;
pub use quick_commands::*;
mod hosts;
pub use hosts::*;
mod tasks;
pub use tasks::*;
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
    HistorySearchResult, LocalLaunchOptions, LogPage, LogPageCodecError, LogPageRequest, LogRow,
    LogStyleSpan, MAX_HISTORY_SEARCH_MATCHES, MAX_HISTORY_SEARCH_QUERY_BYTES,
    MAX_HISTORY_SEARCH_SCAN_LINES, MAX_LOG_PAGE_ROWS, MAX_LOG_PAGE_STYLE_SPANS,
    MAX_LOG_PAGE_TEXT_BYTES, MAX_TERMINAL_INPUT_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    SessionCloseRequest, SessionCloseResponse, SessionCreateRequest, SessionCreateResponse,
    SessionFailureCode, SessionListRequest, SessionListResponse, SessionSummary,
    SnapshotCodecError, SnapshotRequest, TerminalCell, TerminalCellWidth, TerminalColor,
    TerminalCompose, TerminalControlAction, TerminalControlCodecError, TerminalControlResponse,
    TerminalControlStatus, TerminalCursorAppearance, TerminalCursorShape, TerminalDeltaPayload,
    TerminalFramePayload, TerminalInputRequest, TerminalKeyEvent, TerminalKeyKind, TerminalPaste,
    TerminalResizeRequest, TerminalRowPatch, TerminalStyle, envelope, features,
};
pub use replica::{
    ApplyResult, ReplicaError, ReplicaState, TerminalReplicaError, TerminalReplicaState,
};
pub use sftp::{SftpTransferPhase,
    MAX_SFTP_DIRECTORY_ENTRIES, MAX_SFTP_PATH_BYTES, SftpCleanupLocation, SftpDirectoryEntry,
    SftpOperation, SftpRequest, SftpResponse, SftpStatus, SftpTransfer, SftpTransferState,
};
pub use subscription::{ClientFrameUpdate, SubscriptionClientError, TerminalSubscriptionReplica};

pub use profile::{
    HostKeyPreviewData, LocalConnectionData, LocalShellData, MAX_PROFILE_CONTROL_CHANGES,
    ProfileCatalogData, ProfileChange, ProfileCodecError, ProfileDefaultsData, ProfileFolderData,
    ProfileImportAction, ProfileImportItemData, ProfileImportItemKind, ProfileImportPolicy,
    ProfileImportPreviewData, ProfileKindData, ProfileOperation, ProfileRecordData, ProfileRequest,
    ProfileResponse, ProfileStatus, ProfileTerminalOverridesData, SshConnectionData, SshConnectionOptionsData, SshRouteData,
    ThemeData, decode_id, profile_change,
};
mod export_file;
mod upload_source;
pub use upload_source::{UploadSourceError, UploadSourceFingerprint, fingerprint_upload_source};
mod vault;
pub use export_file::{
    ExportDirectoryStaging, ExportFileError, ExportStaging, ExportTransferControl,
    publish_export_file,
};
pub use vault::{
    ManagedKeyInfo, ManagedKeyRequest,
    KeyboardAuthChallenge, KeyboardAuthPrompt, KeyboardAuthAnswer,
    CredentialPackageItem, CredentialPackagePreview, MAX_CREDENTIAL_PACKAGE_BYTES,
    MAX_CREDENTIAL_PACKAGE_PROFILES, VaultControlStatus, VaultOperation, VaultRequest,
    VaultResponse, VaultSessionState,
};
mod connection_diagnostics;
pub use connection_diagnostics::*;
