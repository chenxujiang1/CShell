use prost::{Enumeration, Message};
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileBatchOperation {
    Preview = 0,
    Apply = 1,
    Inspect = 2,
    Reload = 3,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileBatchStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Unsupported = 4,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ProfileBatchRequest {
    #[prost(enumeration = "ProfileBatchOperation", tag = "1")]
    pub operation: i32,
    #[prost(uint64, tag = "2")]
    pub expected_revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub preview_token: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    pub profile_id: Vec<u8>,
    #[prost(bool, tag = "6")]
    pub confirmed: bool,
    #[prost(bytes = "vec", tag = "7")]
    pub review_sha256: Vec<u8>,
}
impl std::fmt::Debug for ProfileBatchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileBatchRequest")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ProfileBatchResponse {
    #[prost(enumeration = "ProfileBatchStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "3")]
    pub detail: String,
}
impl std::fmt::Debug for ProfileBatchResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileBatchResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
