//! Jump reuse schema and runtime candidate selection, negotiated independently.
use cshell_domain::{JumpCandidateSelection, JumpConnectionOverride, JumpReusePolicy, SessionId};
use prost::Message;

pub const JUMP_REUSE_SCHEMA_MINOR: u32 = 80;
pub const JUMP_REUSE_EXECUTION_MINOR: u32 = 81;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid jump reuse policy or connection selection")]
pub struct JumpReuseCodecError;

pub fn decode_jump_reuse_policy(value: u32) -> Result<JumpReusePolicy, JumpReuseCodecError> {
    match value {
        0 => Ok(JumpReusePolicy::AlwaysNew),
        1 => Ok(JumpReusePolicy::PreferExisting),
        2 => Ok(JumpReusePolicy::OnlyExisting),
        _ => Err(JumpReuseCodecError),
    }
}
pub fn encode_jump_reuse_policy(value: JumpReusePolicy) -> u32 {
    match value {
        JumpReusePolicy::AlwaysNew => 0,
        JumpReusePolicy::PreferExisting => 1,
        JumpReusePolicy::OnlyExisting => 2,
    }
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct JumpConnectionOverrideData {
    #[prost(uint32, optional, tag = "1")]
    pub policy: Option<u32>,
    #[prost(message, optional, tag = "2")]
    pub candidate: Option<JumpCandidateSelectionData>,
}
impl JumpConnectionOverrideData {
    pub fn decode_for_peer(
        self,
        protocol_minor: u32,
    ) -> Result<JumpConnectionOverride, JumpReuseCodecError> {
        if protocol_minor < JUMP_REUSE_SCHEMA_MINOR {
            return Err(JumpReuseCodecError);
        }
        self.try_into()
    }
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct JumpCandidateSelectionData {
    #[prost(bytes = "vec", tag = "1")]
    pub transport_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub generation: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub token: Vec<u8>,
}
impl std::fmt::Debug for JumpCandidateSelectionData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JumpCandidateSelectionData")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for JumpConnectionOverrideData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JumpConnectionOverrideData")
            .field("policy", &self.policy)
            .field("has_candidate", &self.candidate.is_some())
            .finish()
    }
}
impl From<&JumpConnectionOverride> for JumpConnectionOverrideData {
    fn from(value: &JumpConnectionOverride) -> Self {
        Self {
            policy: value.policy.map(encode_jump_reuse_policy),
            candidate: value
                .candidate
                .as_ref()
                .map(|candidate| JumpCandidateSelectionData {
                    transport_id: candidate.transport_id.as_uuid().as_bytes().to_vec(),
                    generation: candidate.generation,
                    token: candidate.token.to_vec(),
                }),
        }
    }
}
impl TryFrom<JumpConnectionOverrideData> for JumpConnectionOverride {
    type Error = JumpReuseCodecError;
    fn try_from(value: JumpConnectionOverrideData) -> Result<Self, Self::Error> {
        let result = Self {
            policy: value.policy.map(decode_jump_reuse_policy).transpose()?,
            candidate: value
                .candidate
                .map(|candidate| {
                    Ok::<_, JumpReuseCodecError>(JumpCandidateSelection {
                        transport_id: SessionId::from_bytes(
                            candidate
                                .transport_id
                                .try_into()
                                .map_err(|_| JumpReuseCodecError)?,
                        ),
                        generation: candidate.generation,
                        token: candidate
                            .token
                            .try_into()
                            .map_err(|_| JumpReuseCodecError)?,
                    })
                })
                .transpose()?,
        };
        result
            .is_valid()
            .then_some(result)
            .ok_or(JumpReuseCodecError)
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct JumpCandidatesRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub target_profile_id: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
pub struct JumpCandidatesResponse {
    #[prost(uint32, tag = "5")]
    pub saved_policy: u32,
    #[prost(bool, tag = "6")]
    pub is_jump: bool,
    #[prost(bool, tag = "1")]
    pub execution_supported: bool,
    #[prost(enumeration = "JumpCandidatesStatus", tag = "2")]
    pub status: i32,
    #[prost(message, repeated, tag = "3")]
    pub candidates: Vec<JumpCandidateData>,
    #[prost(string, tag = "4")]
    pub detail: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum JumpCandidatesStatus {
    Ok = 0,
    Unsupported = 1,
    InvalidRequest = 2,
}
#[derive(Clone, PartialEq, Message)]
pub struct JumpCandidateData {
    #[prost(string, tag = "7")]
    pub source_title: String,
    #[prost(uint32, tag = "8")]
    pub consumers: u32,
    #[prost(string, tag = "9")]
    pub host_key_sha256: String,
    #[prost(message, optional, tag = "1")]
    pub selection: Option<JumpCandidateSelectionData>,
    #[prost(bytes = "vec", tag = "2")]
    pub jump_profile_id: Vec<u8>,
    #[prost(string, tag = "3")]
    pub username: String,
    #[prost(string, tag = "4")]
    pub host: String,
    #[prost(uint32, tag = "5")]
    pub port: u32,
    #[prost(uint64, tag = "6")]
    pub expires_unix_seconds: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn new_choice_statuses_preserve_legacy_success_defaults_and_empty_wire() {
        assert_eq!(
            crate::SftpResponse::default().status,
            crate::SftpStatus::Ok as i32
        );
        assert_eq!(
            crate::SessionCreateResponse::default().failure_code,
            crate::SessionFailureCode::None as i32
        );
        assert_eq!(crate::SftpResponse::decode(&[][..]).unwrap().status, 0);
        assert_eq!(
            crate::SessionCreateResponse::decode(&[][..])
                .unwrap()
                .failure_code,
            0
        );
    }
    #[test]
    fn overrides_round_trip_refuse_old_peers_and_never_default_unknown_policy() {
        for policy in [
            JumpReusePolicy::AlwaysNew,
            JumpReusePolicy::PreferExisting,
            JumpReusePolicy::OnlyExisting,
        ] {
            let selection = JumpConnectionOverride {
                policy: Some(policy),
                candidate: None,
            };
            let bytes = JumpConnectionOverrideData::from(&selection).encode_to_vec();
            let wire = JumpConnectionOverrideData::decode(bytes.as_slice()).unwrap();
            assert!(wire.clone().decode_for_peer(79).is_err());
            assert_eq!(wire.decode_for_peer(80).unwrap(), selection);
        }
        assert!(
            JumpConnectionOverride::try_from(JumpConnectionOverrideData {
                policy: Some(3),
                candidate: None
            })
            .is_err()
        );
    }
    #[test]
    fn malformed_candidates_are_rejected_before_use_and_debug_does_not_leak_token() {
        let selection = JumpConnectionOverride {
            policy: Some(JumpReusePolicy::OnlyExisting),
            candidate: Some(JumpCandidateSelection {
                transport_id: SessionId::new(),
                generation: 5,
                token: [77; 32],
            }),
        };
        let mut wire = JumpConnectionOverrideData::from(&selection);
        assert_eq!(
            JumpConnectionOverride::try_from(wire.clone()).unwrap(),
            selection
        );
        assert!(!format!("{wire:?}").contains("77"));
        wire.candidate.as_mut().unwrap().token.pop();
        assert!(JumpConnectionOverride::try_from(wire).is_err());
    }
}
