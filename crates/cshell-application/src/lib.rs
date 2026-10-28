//! Use-case ports. Infrastructure crates implement these interfaces.

mod clipboard;
mod scripts;
mod terminal_tools;
pub use scripts::*;
pub use terminal_tools::*;
mod ssh_providers;
pub use ssh_providers::*;
mod openssh_export;
mod openssh_import;
pub use openssh_export::{
    MAX_OPENSSH_EXPORT_HOSTS, OpenSshExportError, export_openssh_config, openssh_export_alias,
};
mod profile_import;
mod profile_transfer;
pub use clipboard::*;
pub use openssh_import::{OpenSshImportError, OpenSshImportOptions, import_openssh_config};
mod foreign_batch;
mod foreign_import;
pub use foreign_batch::*;
mod mobaxterm_import;
mod putty_import;
mod securecrt_import;
mod xshell_import;
pub use foreign_import::{ForeignProfileImportResult, ForeignProfileImportSummary};
pub use mobaxterm_import::{
    MobaXtermEncoding, MobaXtermImportError, MobaXtermImportOptions, MobaXtermImportSummary,
    import_mobaxterm_sessions,
};
pub use profile_transfer::{LocalProfileTransfer, ProfileExportOptions, export_profile_catalog};
pub use putty_import::{
    PuttyImportError, PuttyImportOptions, PuttyImportResult, PuttyImportSummary,
    import_putty_registry,
};
pub use securecrt_import::{
    SecureCrtImportError, SecureCrtImportOptions, SecureCrtImportSummary, import_securecrt_ini,
};
pub use xshell_import::{
    XshellImportError, XshellImportOptions, XshellImportSummary, import_xshell_session,
};
mod profile_batch;
mod terminal_policies;
pub use terminal_policies::*;
mod profiles;
pub use profile_batch::*;
mod theme_toml;
mod themes;
pub use themes::*;
mod workspace;
pub use workspace::*;
mod workspace_recovery;
pub use workspace_recovery::*;
mod sync_input;
pub use sync_input::*;
mod quick_commands;
pub use quick_commands::*;
mod hosts;
pub use hosts::*;
mod host_authorities;
mod sftp_queue;
mod tunnels;
pub use host_authorities::*;
pub use tunnels::*;
mod sftp_workspace;
mod terminal_upload;
mod sftp_tree;
pub use sftp_tree::{SftpTreeStart,SftpTreeSnapshot};
pub use terminal_upload::{TerminalUploadProtocol, TerminalUploadSnapshot, TerminalUploadStart, TerminalUploadState};
pub use sftp_workspace::*;
mod tasks;
pub use sftp_queue::*;
pub use tasks::*;

pub use profile_import::{
    ImportAction, ImportConflictPolicy, ImportItemKind, ImportItemPreview,
    MAX_PROFILE_IMPORT_BYTES, PROFILE_CONNECTION_IMPORT_VERSION, PROFILE_IMPORT_FORMAT,
    PROFILE_IMPORT_VERSION, PROFILE_TRANSFER_VERSION, ProfileImportDocument, ProfileImportError,
    ProfileImportPreview, plan_profile_import,
};

pub use profiles::{
    CatalogChange, CatalogError, CatalogPreview, CatalogSnapshot, ChangeOutcome, ProfileCatalog,
    ProfileQuery, ProfileRepository, ProfileRepositoryError, ProfileService, ProfileServiceError,
    validate_local_connection,
};

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

mod sftp_sync;
pub use sftp_sync::*;

mod sftp_edit_review;
pub use sftp_edit_review::*;
