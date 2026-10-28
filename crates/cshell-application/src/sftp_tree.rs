//! Bounded recursive transfers bound to an existing authenticated SFTP session.
use crate::{SftpQueueDirection, TerminalUploadState};
use cshell_domain::{SessionId, SftpVerification};
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpTreeStart {
    pub direction: SftpQueueDirection,
    pub paths: Vec<String>,
    pub destination: String,
    pub verification: SftpVerification,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpTreeSnapshot {
    pub id: SessionId,
    pub session: SessionId,
    pub direction: SftpQueueDirection,
    pub state: TerminalUploadState,
    pub completed: u32,
    pub total_files: u32,
    pub bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_second: u64,
    pub detail: String,
    pub retryable: bool,
    pub label: String,
    pub destination:String,
}
impl std::fmt::Debug for SftpTreeStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpTreeStart")
            .field("direction", &self.direction)
            .field("count", &self.paths.len())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for SftpTreeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpTreeSnapshot")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}
