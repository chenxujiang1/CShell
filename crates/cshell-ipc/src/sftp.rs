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
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SftpTransferState {
    Running = 0,
    Succeeded = 1,
    Failed = 2,
    Cancelled = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct SftpRequest {
    #[prost(enumeration = "SftpOperation", tag = "1")]
    pub operation: i32,
    /// Existing daemon SSH session; local terminal sessions are not accepted.
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
}

#[derive(Clone, PartialEq, Message)]
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
}

#[derive(Clone, PartialEq, Message)]
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
}

#[derive(Clone, PartialEq, Message)]
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
        }
    }
}
