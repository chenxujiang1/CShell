use prost::{Enumeration, Message};
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum CaOperation {
    Get = 0,
    PreviewSave = 1,
    ApplySave = 2,
    PreviewRemove = 3,
    ApplyRemove = 4,
    InspectCertificate = 5,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum CaStatus {
    Ok = 0,
    Unsupported = 1,
    Conflict = 2,
    Invalid = 3,
    Unavailable = 4,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct CaRequest {
    #[prost(enumeration = "CaOperation", tag = "1")]
    pub operation: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub authority_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub expected_revision: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub document_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    pub preview_token: Vec<u8>,
    #[prost(string, tag = "6")]
    pub confirmed_fingerprint: String,
    #[prost(bool, tag = "7")]
    pub confirmed: bool,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct CaResponse {
    #[prost(enumeration = "CaStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "3")]
    pub detail: String,
}
impl std::fmt::Debug for CaRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaRequest")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for CaResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
