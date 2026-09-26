//! Use-case ports. Infrastructure crates implement these interfaces.

mod profile_import;
mod profiles;

pub use profile_import::{
    ImportAction, ImportConflictPolicy, ImportItemKind, ImportItemPreview,
    MAX_PROFILE_IMPORT_BYTES, PROFILE_IMPORT_FORMAT, PROFILE_IMPORT_VERSION, ProfileImportDocument,
    ProfileImportError, ProfileImportPreview,
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
