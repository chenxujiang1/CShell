use prost::{Enumeration, Message};
use zeroize::Zeroize;

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct HostGroupRequest {
    #[prost(enumeration = "HostGroupOperation", tag = "1")]
    pub operation: i32,
    #[prost(uint64, tag = "2")]
    pub expected_revision: u64,
    #[prost(uint64, tag = "3")]
    pub expected_profile_revision: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub catalog_json: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    pub selection_json: Vec<u8>,
    #[prost(uint32, tag = "6")]
    pub target_limit: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum HostGroupOperation {
    Load = 0,
    Save = 1,
    Preview = 2,
    Freeze = 3,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct HostGroupResponse {
    #[prost(enumeration = "HostGroupStatus", tag = "1")]
    pub status: i32,
    #[prost(uint64, tag = "2")]
    pub revision: u64,
    #[prost(uint64, tag = "3")]
    pub profile_revision: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub document_json: Vec<u8>,
    #[prost(string, tag = "5")]
    pub detail: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum HostGroupStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Unsupported = 4,
    Corrupt = 5,
    Limit = 6,
    MissingProfile = 7,
}
impl std::fmt::Debug for HostGroupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostGroupRequest")
            .field("operation", &self.operation)
            .field("revision", &self.expected_revision)
            .field("profile_revision", &self.expected_profile_revision)
            .field("document", &"[REDACTED]")
            .finish()
    }
}
impl std::fmt::Debug for HostGroupResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostGroupResponse")
            .field("status", &self.status)
            .field("revision", &self.revision)
            .field("profile_revision", &self.profile_revision)
            .field("document", &"[REDACTED]")
            .finish()
    }
}
impl Drop for HostGroupRequest {
    fn drop(&mut self) {
        self.catalog_json.zeroize();
        self.selection_json.zeroize();
    }
}
impl Drop for HostGroupResponse {
    fn drop(&mut self) {
        self.document_json.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, envelope};

    #[test]
    fn host_envelopes_round_trip_without_debugging_documents_or_selectors() {
        let request = HostGroupRequest {
            operation: HostGroupOperation::Freeze as i32,
            expected_revision: 7,
            expected_profile_revision: 11,
            catalog_json: b"PRIVATE_HOST_CATALOG".to_vec(),
            selection_json: b"PRIVATE_SELECTOR".to_vec(),
            target_limit: 1000,
        };
        let envelope = Envelope {
            request_id: 91,
            payload: Some(envelope::Payload::HostGroupRequest(request)),
            ..Default::default()
        };
        let debug = format!("{envelope:?}");
        assert!(!debug.contains("PRIVATE_HOST_CATALOG"));
        assert!(!debug.contains("PRIVATE_SELECTOR"));
        assert_eq!(
            Envelope::decode(envelope.encode_to_vec().as_slice()).ok(),
            Some(envelope)
        );
        let response = HostGroupResponse {
            status: HostGroupStatus::Ok as i32,
            revision: 7,
            profile_revision: 11,
            document_json: b"PRIVATE_FROZEN_CONFIGURATION".to_vec(),
            detail: "PRIVATE_DETAIL".into(),
        };
        let envelope = Envelope {
            request_id: 91,
            payload: Some(envelope::Payload::HostGroupResponse(response)),
            ..Default::default()
        };
        let debug = format!("{envelope:?}");
        assert!(!debug.contains("PRIVATE_FROZEN_CONFIGURATION"));
        assert!(!debug.contains("PRIVATE_DETAIL"));
        assert_eq!(
            Envelope::decode(envelope.encode_to_vec().as_slice()).ok(),
            Some(envelope)
        );
    }
}
