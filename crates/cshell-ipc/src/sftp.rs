//! Bounded control messages for daemon-owned SFTP operations.

use prost::{Enumeration, Message};

pub const MAX_SFTP_DIRECTORY_ENTRIES: u32 = 512;
pub const MAX_SFTP_PATH_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SftpOperation {
    List = 0,
    Upload = 1,
    Download = 2,
    TransferStatus = 3,
    CancelTransfer = 4,
    CurrentTransfer = 5,
    LocalList = 6,
    QueueGet = 7,
    QueueAdd = 8,
    QueueStart = 9,
    QueuePause = 10,
    QueueCancelPending = 11,
    QueueRemove = 12,
    RemoteBrowse = 13,
    LocalBrowse = 14,
    BrowseNext = 15,
    BrowseClose = 16,
    QueueAddVerified = 17,
    QueueStartVerified = 18,
    QueueResumeVerified = 19,
    QueueAdoptVerified = 20,
    QueueCleanupVerified = 21,
    QueueEdit = 22,
    QueueMigrate = 23,
    FileInspect = 24,
    FileApply = 25,
    FileJobGet = 26,
    FileJobCancel = 27,
    EditOpen = 28,
    EditGet = 29,
    EditLaunch = 30,
    EditSave = 31,
    EditClose = 32,
    QueueAddManaged = 33,
    BrowseManaged = 34,
    QueueStartOne = 35,
    QueueStartManaged = 36,
    QueueConfigure = 37,
    WorkspaceList = 38,
    WorkspaceForget = 39,
    PeerGet = 40,
    EditRebind = 41,
    QueueAddMany = 42,
    PauseTransfer = 43,
    ResumeTransfer = 44,
    QueueEnqueueAndStart = 45,
    QueueRetryMany = 46,
    ConnectProfile = 47,
    Disconnect = 48,
    ConnectionStatus = 49,
    TerminalUploadStart = 50,
    TerminalUploadGet = 51,
    TerminalUploadPause = 52,
    TerminalUploadContinue = 53,
    TerminalUploadCancel = 54,
    TreeStart = 55,
    TreeGet = 56,
    TreePause = 57,
    TreeContinue = 58,
    TreeCancel = 59,
    TreeRetry = 60,
    SyncScan = 61,
    SyncApply = 62,
    EditReview = 63,
    EditRebase = 64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SftpStatus {
    Ok = 0,
    InvalidRequest = 1,
    Unavailable = 2,
    Unsupported = 3,
    Failed = 4,
    CapacityReached = 5,
    Conflict = 6,
    StaleRevision = 7,
    Cancelled = 8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SftpTransferState {
    Running = 0,
    Succeeded = 1,
    Failed = 2,
    Cancelled = 3,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SftpCleanupLocation {
    Unspecified = 0,
    Remote = 1,
    DaemonLocal = 2,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct SftpRequest {
    #[prost(bool,tag="10")]
    pub connection_diagnostics:bool,
    #[prost(enumeration = "SftpOperation", tag = "1")]
    pub operation: i32,
    /// Daemon SSH or dedicated SFTP session; local terminal sessions are not accepted.
    #[prost(bytes = "vec", tag = "2")]
    pub session_id: Vec<u8>,
    /// Remote directory for List, source for Download, destination for Upload.
    #[prost(string, tag = "3")]
    pub remote_path: String,
    /// Source for Upload, destination for Download; interpreted on the daemon host.
    #[prost(string, tag = "4")]
    pub local_path: String,
    #[prost(uint32, tag = "5")]
    pub max_entries: u32,
    #[prost(bytes = "vec", tag = "6")]
    pub transfer_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "7")]
    pub document_json: Vec<u8>,
    #[prost(uint64, tag = "8")]
    pub expected_revision: u64,
    #[prost(bool, tag = "9")]
    pub confirmed: bool,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct SftpDirectoryEntry {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub path: String,
    /// 1=file, 2=directory, 3=symlink, 4=other.
    #[prost(uint32, tag = "3")]
    pub kind: u32,
    #[prost(uint64, optional, tag = "4")]
    pub size: Option<u64>,
    #[prost(uint32, optional, tag = "5")]
    pub modified_unix_seconds: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub permissions: Option<u32>,
    #[prost(uint32, optional, tag = "7")]
    pub uid: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    pub gid: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum SftpTransferPhase {
    Unknown=0, Preparing=1, Transferring=2, Verifying=3, Publishing=4, CheckingPublished=5, Cleaning=6, ServerChecksum=7,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct SftpTransfer {
    #[prost(bytes = "vec", tag = "1")]
    pub transfer_id: Vec<u8>,
    #[prost(enumeration = "SftpTransferState", tag = "2")]
    pub state: i32,
    #[prost(uint64, tag = "3")]
    pub bytes_transferred: u64,
    #[prost(uint64, optional, tag = "4")]
    pub total_bytes: Option<u64>,
    #[prost(string, tag = "5")]
    pub detail: String,
    #[prost(string, optional, tag = "6")]
    pub cleanup_path: Option<String>,
    #[prost(bool, tag = "7")]
    pub destination_committed: bool,
    #[prost(enumeration = "SftpCleanupLocation", tag = "8")]
    pub cleanup_location: i32,
    #[prost(bool, tag = "9")]
    pub paused: bool,
    #[prost(enumeration="SftpTransferPhase",tag="10")]
    pub phase:i32,
    #[prost(uint64,tag="11")]
    pub phase_bytes:u64,
    #[prost(uint64,optional,tag="12")]
    pub phase_total:Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct SftpResponse {
    #[prost(enumeration = "SftpStatus", tag = "1")]
    pub status: i32,
    #[prost(string, tag = "2")]
    pub detail: String,
    #[prost(message, repeated, tag = "3")]
    pub entries: Vec<SftpDirectoryEntry>,
    #[prost(bool, tag = "4")]
    pub truncated: bool,
    #[prost(message, optional, tag = "5")]
    pub transfer: Option<SftpTransfer>,
    #[prost(bytes = "vec", tag = "6")]
    pub document_json: Vec<u8>,
    /// Ephemeral directory cursor, scoped to session_id; never a transfer ID.
    #[prost(bytes = "vec", tag = "7")]
    pub browse_id: Vec<u8>,
    #[prost(uint64, tag = "8")]
    pub browse_revision: u64,
    #[prost(string, tag = "9")]
    pub browse_directory: String,
    #[prost(bool, tag = "10")]
    pub browse_local: bool,
}

impl std::fmt::Debug for SftpRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpRequest")
            .field("operation", &self.operation)
            .field("max_entries", &self.max_entries)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for SftpDirectoryEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpDirectoryEntry")
            .field("kind", &self.kind)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for SftpTransfer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpTransfer")
            .field("state", &self.state)
            .field("bytes_transferred", &self.bytes_transferred)
            .field("total_bytes", &self.total_bytes)
            .field("cleanup_required", &self.cleanup_path.is_some())
            .field("destination_committed", &self.destination_committed)
            .field("cleanup_location", &self.cleanup_location)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for SftpResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpResponse")
            .field("status", &self.status)
            .field("entry_count", &self.entries.len())
            .field("truncated", &self.truncated)
            .field("transfer", &self.transfer)
            .finish_non_exhaustive()
    }
}

impl SftpResponse {
    #[must_use]
    pub fn with_status(status: SftpStatus, detail: impl Into<String>) -> Self {
        Self {
            status: status as i32,
            detail: detail.into(),
            entries: Vec::new(),
            truncated: false,
            transfer: None,
            document_json: Vec::new(),
            browse_id: Vec::new(),
            browse_revision: 0,
            browse_directory: String::new(),
            browse_local: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, envelope};

    #[test]
    fn directory_attributes_round_trip_and_legacy_entries_remain_valid() {
        let legacy = SftpDirectoryEntry::decode(&[10, 1, b'a'][..]).unwrap();
        assert_eq!(legacy.name, "a");
        assert_eq!(
            (legacy.permissions, legacy.uid, legacy.gid),
            (None, None, None)
        );
        let entry = SftpDirectoryEntry {
            name: "folder".into(),
            kind: 2,
            permissions: Some(0o40755),
            uid: Some(1000),
            gid: Some(100),
            modified_unix_seconds: Some(1_700_000_000),
            ..Default::default()
        };
        assert_eq!(
            SftpDirectoryEntry::decode(entry.encode_to_vec().as_slice()).unwrap(),
            entry
        );
    }

    #[test]
    fn nested_diagnostics_hide_paths_and_server_text_but_wire_retains_recovery() {
        let marker = "PRIVATE_SFTP_SENTINEL";
        let request = SftpRequest {
            remote_path: marker.into(),
            local_path: marker.into(),
            ..Default::default()
        };
        let transfer = SftpTransfer {
            state: SftpTransferState::Failed as i32,
            detail: marker.into(),
            cleanup_path: Some(marker.into()),
            destination_committed: true,
            cleanup_location: SftpCleanupLocation::DaemonLocal as i32,
            ..Default::default()
        };
        let response = SftpResponse {
            detail: marker.into(),
            entries: vec![SftpDirectoryEntry {
                name: marker.into(),
                path: marker.into(),
                ..Default::default()
            }],
            transfer: Some(transfer),
            ..Default::default()
        };
        for payload in [
            envelope::Payload::SftpRequest(request),
            envelope::Payload::SftpResponse(response.clone()),
        ] {
            let envelope = Envelope {
                payload: Some(payload),
                ..Default::default()
            };
            assert!(!format!("{envelope:?}").contains(marker));
        }
        let decoded = SftpResponse::decode(response.encode_to_vec().as_slice())
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(decoded, response);
    }

    #[test]
    fn cleanup_metadata_is_additive_for_older_transfer_peers() {
        #[derive(Clone, PartialEq, Message)]
        struct LegacyTransfer {
            #[prost(bytes = "vec", tag = "1")]
            transfer_id: Vec<u8>,
            #[prost(enumeration = "SftpTransferState", tag = "2")]
            state: i32,
            #[prost(uint64, tag = "3")]
            bytes_transferred: u64,
            #[prost(uint64, optional, tag = "4")]
            total_bytes: Option<u64>,
            #[prost(string, tag = "5")]
            detail: String,
        }
        let legacy = LegacyTransfer {
            state: SftpTransferState::Failed as i32,
            detail: "inspect destination before recovery".into(),
            ..Default::default()
        };
        let mut current = SftpTransfer::decode(legacy.encode_to_vec().as_slice())
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(current.cleanup_path.is_none());
        assert!(!current.destination_committed);
        current.cleanup_path = Some("manual.tmp".into());
        current.destination_committed = true;
        current.cleanup_location = SftpCleanupLocation::Remote as i32;
        let old_peer = LegacyTransfer::decode(current.encode_to_vec().as_slice())
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(old_peer, legacy);
    }
}
