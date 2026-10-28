//! Reviewed SFTP workspace documents. Paths and file bodies are ordinary private data.
use crate::{SftpQueueDraft, SftpQueueSnapshot, SftpVerifiedContent};
use cshell_domain::{ClipboardTrust, SessionId, SftpQueueItemId};
use serde::{Deserialize, Serialize};
pub const MAX_SFTP_EDIT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SFTP_INLINE_EDIT_BYTES: usize = 256 * 1024;
pub const MAX_SFTP_WORKSPACE_DOCUMENT: usize = 12 * 1024 * 1024;
pub fn sftp_remote_path(path: &str, root: bool) -> bool {
    (root && path == "/")
        || path.starts_with('/')
            && path.len() <= 4000
            && !path.ends_with('/')
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .skip(1)
                .all(|p| !p.is_empty() && p != "." && p != "..")
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpLinkPolicy {
    #[default]
    NoFollow,
    Follow,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpBrowseOptions {
    pub local: bool,
    pub path: String,
    pub links: SftpLinkPolicy,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpQueueSettings {
    pub concurrency: u32,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpQueueMigration {
    pub target: SessionId,
    pub target_revision: u64,
    pub peer: ClipboardTrust,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpFileAction {
    Inspect,
    Mkdir,
    Rename,
    Delete,
    Chmod,
    Symlink,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpFileDraft {
    pub local: bool,
    pub action: SftpFileAction,
    pub path: String,
    pub target: String,
    pub permissions: Option<u32>,
}
impl SftpFileDraft {
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid = |p: &str| {
            if self.local {
                !p.is_empty()
                    && p.len() <= 4096
                    && std::path::Path::new(p).is_absolute()
                    && !p.chars().any(char::is_control)
                    && !std::path::Path::new(p)
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
            } else {
                sftp_remote_path(p, self.action == SftpFileAction::Inspect)
            }
        };
        if !valid(&self.path)
            || matches!(
                self.action,
                SftpFileAction::Rename | SftpFileAction::Symlink
            ) && (!valid(&self.target) || self.path == self.target)
            || !matches!(
                self.action,
                SftpFileAction::Rename | SftpFileAction::Symlink
            ) && !self.target.is_empty()
            || matches!(self.action, SftpFileAction::Chmod | SftpFileAction::Mkdir)
                != self.permissions.is_some()
            || self.permissions.is_some_and(|p| p > 0o7777)
            || self.local && self.action == SftpFileAction::Chmod && cfg!(windows)
            || self.action != SftpFileAction::Inspect
                && (self.path == "/"
                    || self.local && std::path::Path::new(&self.path).parent().is_none())
        {
            return Err("invalid filesystem action");
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEntryStamp {
    pub kind: u32,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub permissions: Option<u32>,
    pub content: Option<SftpVerifiedContent>,
    pub link_target: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpFileState {
    Inspecting,
    Ready,
    Applying,
    Succeeded,
    Unknown,
    Cancelled,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpFileJob {
    pub peer: ClipboardTrust,
    pub native_transfer: SessionId,
    pub format: String,
    pub version: u32,
    pub id: SessionId,
    pub session: SessionId,
    pub revision: u64,
    pub draft: SftpFileDraft,
    pub state: SftpFileState,
    pub source: Option<SftpEntryStamp>,
    pub target: Option<SftpEntryStamp>,
    pub intent: bool,
    pub acknowledged: bool,
    pub detail: String,
}
impl SftpFileJob {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.draft.validate()?;
        if self.format != "cshell.sftp-file-job"
            || self.native_transfer.as_uuid().is_nil()
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || self.session.as_uuid().is_nil()
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.detail.len() > 1024
            || self.detail.chars().any(char::is_control)
            || self.acknowledged && !self.intent
            || self.state == SftpFileState::Succeeded && !self.acknowledged
            || [self.source.as_ref(), self.target.as_ref()]
                .into_iter()
                .flatten()
                .any(|s| {
                    !(1..=4).contains(&s.kind)
                        || s.link_target.as_ref().is_some_and(|p| p.len() > 4096)
                        || s.content
                            .is_some_and(|c| c.size > crate::MAX_SFTP_VERIFIED_BYTES)
                })
        {
            return Err("invalid filesystem job");
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEditDocument {
    #[serde(default)]
    pub native_transfer: Option<SessionId>,
    #[serde(default)]
    pub base_permissions: Option<u32>,
    pub peer: ClipboardTrust,
    pub state: SftpEditState,
    pub queued_draft: Option<SftpQueueDraft>,
    pub format: String,
    pub version: u32,
    pub id: SessionId,
    pub session: SessionId,
    pub revision: u64,
    pub remote_path: String,
    pub local_path: String,
    pub base: SftpVerifiedContent,
    pub current: SftpVerifiedContent,
    pub queued_item: Option<SftpQueueItemId>,
    pub closed: bool,
    pub detail: String,
}
impl SftpEditDocument {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.queued_item.is_some() != self.queued_draft.is_some() {
            return Err("invalid editor queue intent");
        }
        if let Some(draft) = &self.queued_draft {
            draft.validate()?;
            if !draft.verified
                || draft.expected_source.is_none()
                || draft.expected_destination.is_none()
            {
                return Err("unbound editor upload");
            }
        }
        if self.format != "cshell.sftp-edit"
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || self.session.as_uuid().is_nil()
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || !sftp_remote_path(&self.remote_path, false)
            || !std::path::Path::new(&self.local_path).is_absolute()
            || self.local_path.len() > 4096
            || self.base.size > MAX_SFTP_EDIT_BYTES as u64
            || self.current.size > MAX_SFTP_EDIT_BYTES as u64
            || self.closed != (self.state == SftpEditState::Closed)
            || self.native_transfer.is_some_and(|i| i.as_uuid().is_nil())
            || self.base_permissions.is_some_and(|p| p > 0o7777)
            || self.detail.len() > 1024
        {
            return Err("invalid remote edit");
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpEditState {
    Preparing,
    Ready,
    Unknown,
    Closed,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEditorLaunch {
    pub executable: String,
    pub arguments: Vec<String>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEditSave {
    pub content: Option<String>,
    pub queue_revision: u64,
    pub accept_exclusive_replace: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpWorkspacePage {
    pub offset: u32,
    pub all_sessions: bool,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpWorkspaceSummary {
    pub id: SessionId,
    pub session: SessionId,
    pub revision: u64,
    pub editor: bool,
    pub state: String,
    pub path: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpWorkspaceForget {
    pub editor: bool,
    pub discard_private_files: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SftpWorkspaceReply {
    Review(crate::SftpEditReview),
    Peer {
        session: SessionId,
        peer: ClipboardTrust,
    },
    History {
        session: SessionId,
        offset: u32,
        more: bool,
        items: Vec<SftpWorkspaceSummary>,
    },
    Forgotten {
        session: SessionId,
        id: SessionId,
        retained_files: bool,
    },
    Queue(SftpQueueSnapshot),
    File(SftpFileJob),
    Edit {
        document: SftpEditDocument,
        text: Option<String>,
    },
}
macro_rules! redacted_debug { ($($t:ty),*) => { $(impl std::fmt::Debug for $t { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(concat!(stringify!($t), " { [REDACTED] }")) } })* }; }
redacted_debug!(
    SftpEntryStamp,
    SftpBrowseOptions,
    SftpFileDraft,
    SftpFileJob,
    SftpEditDocument,
    SftpEditorLaunch,
    SftpEditSave,
    SftpWorkspaceReply,
    SftpWorkspaceSummary
);
pub fn decode_sftp_workspace(bytes: &[u8]) -> Result<SftpWorkspaceReply, &'static str> {
    if bytes.len() > MAX_SFTP_WORKSPACE_DOCUMENT {
        return Err("workspace document limit");
    }
    let value: SftpWorkspaceReply =
        serde_json::from_slice(bytes).map_err(|_| "invalid workspace reply")?;
    match &value {
        SftpWorkspaceReply::Review(review)=>review.validate()?,
        SftpWorkspaceReply::Peer { session, peer } => {
            if session.as_uuid().is_nil()
                || peer.host_key_sha256.is_empty()
                || peer.route.len() > 8192
            {
                return Err("invalid peer identity");
            }
        }
        SftpWorkspaceReply::History { session, items, .. } => {
            if session.as_uuid().is_nil()
                || items.len() > 64
                || items.iter().any(|i| {
                    i.id.as_uuid().is_nil()
                        || i.session.as_uuid().is_nil()
                        || i.revision == 0
                        || i.path.len() > 4096
                        || i.state.len() > 64
                })
            {
                return Err("invalid workspace history");
            }
        }
        SftpWorkspaceReply::Forgotten { session, id, .. } => {
            if session.as_uuid().is_nil() || id.as_uuid().is_nil() {
                return Err("invalid forgotten identity");
            }
        }
        SftpWorkspaceReply::Queue(q) => q.validate()?,
        SftpWorkspaceReply::File(j) => j.validate()?,
        SftpWorkspaceReply::Edit { document, text } => {
            document.validate()?;
            if text.as_ref().is_some_and(|t| {
                t.len() > MAX_SFTP_INLINE_EDIT_BYTES
                    || sftp_digest(t.as_bytes()) != document.current
            }) {
                return Err("edit text limit");
            }
        }
    }
    Ok(value)
}
pub fn sftp_digest(bytes: &[u8]) -> SftpVerifiedContent {
    use sha2::Digest;
    SftpVerifiedContent {
        size: bytes.len() as u64,
        sha256: sha2::Sha256::digest(bytes).into(),
    }
}

/// Receipt for a managed SFTP view. No extra terminal channel is opened.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpConnectionInfo {
    pub session: cshell_domain::SessionId,
    pub profile: cshell_domain::ProfileId,
    pub title: String,
    pub peer: cshell_domain::ClipboardTrust,
    pub running: bool,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub shared_from: Option<cshell_domain::SessionId>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub initial_directory: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpConnectRequest {
    pub profile: cshell_domain::ProfileId,
    #[serde(default)]
    pub source_ssh_session: Option<cshell_domain::SessionId>,
    #[serde(default)]
    pub discover_directory: bool,
}
