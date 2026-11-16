//! Saved policy is separate from an ephemeral, server-issued connection choice.
use crate::SessionId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JumpReusePolicy {
    #[default]
    AlwaysNew,
    PreferExisting,
    OnlyExisting,
}
impl JumpReusePolicy {
    pub fn is_always_new(&self) -> bool {
        *self == Self::AlwaysNew
    }
    pub const fn for_new_jump() -> Self {
        Self::PreferExisting
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JumpCandidateSelection {
    pub transport_id: SessionId,
    pub generation: u64,
    pub token: [u8; 32],
}
impl std::fmt::Debug for JumpCandidateSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JumpCandidateSelection")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JumpConnectionOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<JumpReusePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<JumpCandidateSelection>,
}
impl JumpConnectionOverride {
    pub fn is_valid(&self) -> bool {
        self.candidate.as_ref().is_none_or(|candidate| {
            self.policy != Some(JumpReusePolicy::AlwaysNew)
                && !candidate.transport_id.as_uuid().is_nil()
                && candidate.generation > 0
                && candidate.token != [0; 32]
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn legacy_default_is_independent_and_new_jump_recommendation_is_explicit() {
        let options = crate::SshConnectionOptions::default();
        assert_eq!(options.jump_reuse_policy, JumpReusePolicy::AlwaysNew);
        assert_eq!(
            JumpReusePolicy::for_new_jump(),
            JumpReusePolicy::PreferExisting
        );
    }
    #[test]
    fn ephemeral_selection_has_bounded_identity_and_conflicting_new_policy_is_invalid() {
        let mut selection = JumpConnectionOverride {
            policy: Some(JumpReusePolicy::OnlyExisting),
            candidate: Some(JumpCandidateSelection {
                transport_id: SessionId::new(),
                generation: 1,
                token: [42; 32],
            }),
        };
        assert!(selection.is_valid());
        assert!(!format!("{selection:?}").contains("42, 42"));
        selection.policy = Some(JumpReusePolicy::AlwaysNew);
        assert!(!selection.is_valid());
        selection.policy = None;
        selection.candidate.as_mut().unwrap().generation = 0;
        assert!(!selection.is_valid());
    }
}
