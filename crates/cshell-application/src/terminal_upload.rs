//! Terminal drag-and-drop uploads; source paths never belong in diagnostics.
use cshell_domain::{SessionId, SftpVerification};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalUploadProtocol {
    #[default]
    Sftp,
    Zmodem,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalUploadStart {
    pub paths: Vec<String>,
    pub protocol: TerminalUploadProtocol,
    pub verification: SftpVerification,
    pub confirm_before_start: bool,
}
impl std::fmt::Debug for TerminalUploadStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalUploadStart")
            .field("count", &self.paths.len())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalUploadState {
    Preparing,
    Ready,
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}
impl TerminalUploadState {
    pub fn finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalUploadSnapshot {
    pub id: SessionId,
    pub source: SessionId,
    pub host: String,
    pub directory: String,
    pub protocol: TerminalUploadProtocol,
    pub state: TerminalUploadState,
    pub bytes: u64,
    pub total_bytes: u64,
    pub completed: u32,
    pub total_files: u32,
    pub current_name: String,
    pub bytes_per_second: u64,
    pub detail: String,
}
impl std::fmt::Debug for TerminalUploadSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalUploadSnapshot")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("completed", &self.completed)
            .finish_non_exhaustive()
    }
}
