use prost::{Enumeration, Message};
use zeroize::Zeroize;

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct QuickCommandRequest {
    #[prost(enumeration = "QuickCommandOperation", tag = "1")]
    pub operation: i32,
    #[prost(uint64, tag = "2")]
    pub expected_revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum QuickCommandOperation {
    Load = 0,
    Save = 1,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct QuickCommandResponse {
    #[prost(enumeration = "QuickCommandStatus", tag = "1")]
    pub status: i32,
    #[prost(uint64, tag = "2")]
    pub revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum QuickCommandStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Unsupported = 4,
    Corrupt = 5,
}
impl std::fmt::Debug for QuickCommandRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickCommandRequest")
            .field("operation", &self.operation)
            .field("expected_revision", &self.expected_revision)
            .field("document_json", &"[REDACTED]")
            .finish()
    }
}
impl std::fmt::Debug for QuickCommandResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickCommandResponse")
            .field("status", &self.status)
            .field("revision", &self.revision)
            .field("document_json", &"[REDACTED]")
            .finish()
    }
}
impl Drop for QuickCommandRequest {
    fn drop(&mut self) {
        self.document_json.zeroize();
    }
}
impl Drop for QuickCommandResponse {
    fn drop(&mut self) {
        self.document_json.zeroize();
    }
}
