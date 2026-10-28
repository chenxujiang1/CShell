use prost::{Enumeration, Message};

/// Explicit creation using the complete reviewed resolution and exact reference
/// versions. Separate from SessionCreateRequest to preserve the legacy launch.
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TerminalContextLaunchRequest {
    #[prost(bool,tag="5")]
    pub connection_diagnostics:bool,
    #[prost(bytes = "vec", tag = "1")]
    pub report_json: Vec<u8>,
    #[prost(uint32, tag = "2")]
    pub rows: u32,
    #[prost(uint32, tag = "3")]
    pub cols: u32,
    #[prost(bool, tag = "4")]
    pub confirmed: bool,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TerminalContextLaunchResponse {
    #[prost(message, optional, tag = "1")]
    pub result: Option<crate::SessionCreateResponse>,
    #[prost(bytes = "vec", tag = "2")]
    pub report_json: Vec<u8>,
}
impl std::fmt::Debug for TerminalContextLaunchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalContextLaunchRequest")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for TerminalContextLaunchResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalContextLaunchResponse")
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalPolicyOperation {
    Get = 0,
    Preview = 1,
    Apply = 2,
    Resolve = 3,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalPolicyStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Unsupported = 4,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TerminalPolicyRequest {
    #[prost(enumeration = "TerminalPolicyOperation", tag = "1")]
    pub operation: i32,
    #[prost(uint64, tag = "2")]
    pub expected_revision: u64,
    #[prost(uint64, tag = "3")]
    pub profile_revision: u64,
    #[prost(uint64, tag = "4")]
    pub host_revision: u64,
    #[prost(bytes = "vec", tag = "5")]
    pub document_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "6")]
    pub preview_token: Vec<u8>,
    #[prost(bytes = "vec", tag = "7")]
    pub review_sha256: Vec<u8>,
    #[prost(bool, tag = "8")]
    pub confirmed: bool,
}
impl std::fmt::Debug for TerminalPolicyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalPolicyRequest")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TerminalPolicyResponse {
    #[prost(enumeration = "TerminalPolicyStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "3")]
    pub detail: String,
}
impl std::fmt::Debug for TerminalPolicyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalPolicyResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
