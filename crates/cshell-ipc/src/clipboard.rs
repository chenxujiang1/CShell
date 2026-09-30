use prost::{Enumeration, Message};
use zeroize::Zeroize;

pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum ClipboardOperation {
    Poll = 0,
    Approve = 1,
    Reject = 2,
    Block = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum ClipboardStatus {
    Ok = 0,
    InvalidRequest = 1,
    Unavailable = 2,
    Stale = 3,
    Unsupported = 4,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ClipboardRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(enumeration = "ClipboardOperation", tag = "2")]
    pub operation: i32,
    #[prost(bytes = "vec", tag = "3")]
    pub token: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ClipboardResponse {
    #[prost(enumeration = "ClipboardStatus", tag = "1")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub token: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub byte_count: u32,
    #[prost(string, tag = "4")]
    pub text: String,
}

impl ClipboardResponse {
    pub fn with_status(status: ClipboardStatus) -> Self {
        Self {
            status: status as i32,
            token: Vec::new(),
            byte_count: 0,
            text: String::new(),
        }
    }
}

impl Drop for ClipboardRequest {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}
impl Drop for ClipboardResponse {
    fn drop(&mut self) {
        self.token.zeroize();
        self.text.zeroize();
    }
}
impl std::fmt::Debug for ClipboardRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardRequest")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for ClipboardResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardResponse")
            .field("status", &self.status)
            .field("byte_count", &self.byte_count)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_debug_hides_contents_and_token() {
        let response = crate::Envelope {
            payload: Some(crate::envelope::Payload::ClipboardResponse(
                ClipboardResponse {
                    text: "secret clipboard".into(),
                    token: b"secret token".to_vec(),
                    status: 0,
                    byte_count: 0,
                },
            )),
            ..Default::default()
        };
        let debug = format!("{response:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("115, 101"));
    }
}
