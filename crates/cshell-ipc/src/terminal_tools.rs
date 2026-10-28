use prost::{Enumeration, Message};
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalToolsOperation {
    Get = 0,
    Save = 1,
    Arm = 2,
    Disarm = 3,
    Poll = 4,
    RawJournal = 5,
    ShellHistory = 6,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalToolsStatus {
    Ok = 0,
    Unsupported = 1,
    Invalid = 2,
    Conflict = 3,
    Unavailable = 4,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TerminalToolsRequest {
    #[prost(enumeration = "TerminalToolsOperation", tag = "1")]
    pub operation: i32,
    #[prost(uint64, tag = "2")]
    pub expected_revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub session_id: Vec<u8>,
    #[prost(uint64, tag = "5")]
    pub cursor: u64,
    #[prost(uint64, tag = "6")]
    pub offset: u64,
    #[prost(bool, tag = "7")]
    pub allow_text: bool,
    #[prost(bool, tag = "8")]
    pub confirmed: bool,
}
impl std::fmt::Debug for TerminalToolsRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalToolsRequest")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TerminalToolsResponse {
    #[prost(enumeration = "TerminalToolsStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "3")]
    pub detail: String,
}
impl std::fmt::Debug for TerminalToolsResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalToolsResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
