use prost::{Enumeration, Message};
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TunnelOperation {
    Get = 0,
    Create = 1,
    Update = 2,
    Start = 3,
    Stop = 4,
    Remove = 5,
    ConfigureDedicated = 6,
    StartDedicated = 7,
    ClearDedicated = 8,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TunnelStatus {
    Ok = 0,
    Unsupported = 1,
    Invalid = 2,
    Conflict = 3,
    Capacity = 4,
    Unavailable = 5,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TunnelRequest {
    #[prost(enumeration = "TunnelOperation", tag = "1")]
    pub operation: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub tunnel_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub session_id: Vec<u8>,
    #[prost(uint64, tag = "4")]
    pub expected_revision: u64,
    #[prost(bool, tag = "5")]
    pub confirmed: bool,
    #[prost(bytes = "vec", tag = "6")]
    pub document_json: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct TunnelResponse {
    #[prost(enumeration = "TunnelStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "3")]
    pub detail: String,
}
impl std::fmt::Debug for TunnelRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelRequest")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for TunnelResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
