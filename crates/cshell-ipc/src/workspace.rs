//! A capability-gated workspace control channel; layout JSON has its own version.
use prost::{Enumeration, Message};

#[derive(Clone, PartialEq, Message)]
pub struct WorkspaceRequest {
    #[prost(enumeration = "WorkspaceOperation", tag = "1")]
    pub operation: i32,
    #[prost(uint64, tag = "2")]
    pub expected_revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum WorkspaceOperation {
    Load = 0,
    Save = 1,
}

#[derive(Clone, PartialEq, Message)]
pub struct WorkspaceResponse {
    #[prost(enumeration = "WorkspaceStatus", tag = "1")]
    pub status: i32,
    #[prost(uint64, tag = "2")]
    pub revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "4")]
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum WorkspaceStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Unsupported = 4,
    Corrupt = 5,
}
