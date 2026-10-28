use prost::{Enumeration, Message};

/// Empty JSON requests the catalog; nonempty JSON evaluates typed requirements.
/// This operation never opens a connection or discovers executables.
#[derive(Clone, PartialEq, Message)]
pub struct SshProviderRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub requirements_json: Vec<u8>,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SshProviderStatus {
    Ok = 0,
    Unsupported = 1,
    Invalid = 2,
    Unavailable = 3,
}
#[derive(Clone, PartialEq, Message)]
pub struct SshProviderResponse {
    #[prost(enumeration = "SshProviderStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "3")]
    pub detail: String,
}
