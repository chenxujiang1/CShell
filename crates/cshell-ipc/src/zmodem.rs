//! Authenticated control plane; file bytes remain on the SSH terminal channel.
use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct ZmodemRequest {
    /// 0: poll, 1: send selected local files, 2: receive to directory, 3: cancel.
    #[prost(uint32, tag = "1")]
    pub operation: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub session_id: Vec<u8>,
    #[prost(uint64, tag = "3")]
    pub transfer_id: u64,
    #[prost(string, repeated, tag = "4")]
    pub paths: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ZmodemTransfer {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub transfer_id: u64,
    /// 1 upload (remote rz), 2 download (remote sz).
    #[prost(uint32, tag = "3")]
    pub direction: u32,
    /// 1 awaiting selection, 2 active, 3 completed, 4 cancelled, 5 failed.
    #[prost(uint32, tag = "4")]
    pub state: u32,
    #[prost(string, tag = "5")]
    pub name: String,
    #[prost(uint64, tag = "6")]
    pub bytes: u64,
    #[prost(uint64, tag = "7")]
    pub size: u64,
    #[prost(uint32, tag = "8")]
    pub files_completed: u32,
    #[prost(string, tag = "9")]
    pub detail: String,
    #[prost(bytes = "vec", optional, tag = "10")]
    pub profile_id: Option<Vec<u8>>,
    #[prost(string, tag = "11")]
    pub session_title: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ZmodemResponse {
    #[prost(message, repeated, tag = "1")]
    pub transfers: Vec<ZmodemTransfer>,
    #[prost(string, tag = "2")]
    pub error: String,
}
