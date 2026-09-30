use prost::{Enumeration, Message};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum ClipboardPolicyOperation {
    Load = 0,
    Save = 1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum ClipboardPolicyStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Unsupported = 4,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ClipboardPolicyRequest {
    #[prost(enumeration = "ClipboardPolicyOperation", tag = "1")]
    pub operation: i32,
    #[prost(fixed64, tag = "2")]
    pub expected_revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub selected_session_id: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ClipboardPolicyResponse {
    #[prost(enumeration = "ClipboardPolicyStatus", tag = "1")]
    pub status: i32,
    #[prost(fixed64, tag = "2")]
    pub revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "4")]
    pub selected_host: String,
    #[prost(uint32, tag = "5")]
    pub selected_port: u32,
}
impl ClipboardPolicyResponse {
    pub fn with_status(status: ClipboardPolicyStatus) -> Self {
        Self {
            status: status as i32,
            ..Default::default()
        }
    }
}
impl std::fmt::Debug for ClipboardPolicyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardPolicyRequest")
            .field("operation", &self.operation)
            .field("expected_revision", &self.expected_revision)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for ClipboardPolicyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardPolicyResponse")
            .field("status", &self.status)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, envelope};

    #[test]
    fn host_policy_envelopes_round_trip_without_diagnostic_host_leaks()
    -> Result<(), prost::DecodeError> {
        let request = Envelope {
            request_id: 12,
            payload: Some(envelope::Payload::ClipboardPolicyRequest(
                ClipboardPolicyRequest {
                    operation: ClipboardPolicyOperation::Save as i32,
                    expected_revision: 4,
                    document_json: b"PRIVATE-POLICY".to_vec(),
                    selected_session_id: vec![9; 16],
                },
            )),
            ..Default::default()
        };
        let response = Envelope {
            request_id: 12,
            payload: Some(envelope::Payload::ClipboardPolicyResponse(
                ClipboardPolicyResponse {
                    revision: 5,
                    document_json: b"PRIVATE-POLICY".to_vec(),
                    selected_host: "private.example.test".into(),
                    selected_port: 22,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        for envelope in [request, response] {
            assert_eq!(
                Envelope::decode(envelope.encode_to_vec().as_slice())?,
                envelope
            );
            let diagnostic = format!("{envelope:?}");
            assert!(!diagnostic.contains("PRIVATE-POLICY"));
            assert!(!diagnostic.contains("private.example.test"));
            assert!(!diagnostic.contains("80, 82, 73"));
        }
        Ok(())
    }
}
