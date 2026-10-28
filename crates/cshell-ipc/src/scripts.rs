use prost::{Enumeration, Message};
use zeroize::Zeroize;
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ScriptControlOperation {
    Preview = 0,
    Start = 1,
    Get = 2,
    Revoke = 3,
    List = 4,
    Recover = 5,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ScriptControlStatus {
    Ok = 0,
    Unsupported = 1,
    Invalid = 2,
    Unavailable = 3,
    Conflict = 4,
    Busy = 5,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ScriptControlRequest {
    #[prost(enumeration = "ScriptControlOperation", tag = "1")]
    pub operation: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
}
impl Drop for ScriptControlRequest {
    fn drop(&mut self) {
        self.document_json.zeroize();
    }
}
impl std::fmt::Debug for ScriptControlRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptControlRequest(..)")
    }
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ScriptControlResponse {
    #[prost(enumeration = "ScriptControlStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub document_json: Vec<u8>,
}
impl std::fmt::Debug for ScriptControlResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptControlResponse")
            .field("status", &self.status)
            .finish()
    }
}
