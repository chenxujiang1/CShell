use async_trait::async_trait;
use cshell_domain::{ClipboardHost, ClipboardTrust};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

pub const MAX_CLIPBOARD_POLICY_BYTES: usize = 64 * 1024;
pub const MAX_CLIPBOARD_POLICY_HOSTS: usize = 256;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardPolicyDocument {
    pub format: String,
    pub version: u32,
    pub blocked_hosts: Vec<ClipboardHost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automatic: Vec<ClipboardTrust>,
}
impl Default for ClipboardPolicyDocument {
    fn default() -> Self {
        Self {
            format: "cshell.clipboard-policy".into(),
            version: 1,
            blocked_hosts: Vec::new(),
            automatic: Vec::new(),
        }
    }
}
impl std::fmt::Debug for ClipboardPolicyDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardPolicyDocument")
            .field("version", &self.version)
            .field("host_count", &self.blocked_hosts.len())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct ClipboardPolicySnapshot {
    pub revision: u64,
    pub document: ClipboardPolicyDocument,
}
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ClipboardPolicyError {
    #[error("clipboard policy is unavailable")]
    Unavailable,
    #[error("clipboard policy document is invalid")]
    Invalid,
    #[error("clipboard policy revision changed; refresh before saving")]
    Conflict,
}

pub fn decode_clipboard_policy(
    bytes: &[u8],
) -> Result<ClipboardPolicyDocument, ClipboardPolicyError> {
    if bytes.len() > MAX_CLIPBOARD_POLICY_BYTES {
        return Err(ClipboardPolicyError::Invalid);
    }
    let document: ClipboardPolicyDocument =
        serde_json::from_slice(bytes).map_err(|_| ClipboardPolicyError::Invalid)?;
    validate_clipboard_policy(&document)?;
    Ok(document)
}
pub fn validate_clipboard_policy(
    document: &ClipboardPolicyDocument,
) -> Result<(), ClipboardPolicyError> {
    if document.format != "cshell.clipboard-policy"
        || !matches!(document.version, 1 | 2)
        || (document.version == 1 && !document.automatic.is_empty())
        || document.blocked_hosts.len() > MAX_CLIPBOARD_POLICY_HOSTS
        || document.automatic.len() > MAX_CLIPBOARD_POLICY_HOSTS
    {
        return Err(ClipboardPolicyError::Invalid);
    }
    let mut hosts = BTreeSet::new();
    for host in &document.blocked_hosts {
        if ClipboardHost::new(&host.host, host.port).as_ref() != Some(host) || !hosts.insert(host) {
            return Err(ClipboardPolicyError::Invalid);
        }
    }
    let mut grants = BTreeSet::new();
    for grant in &document.automatic {
        if ClipboardHost::new(&grant.host.host, grant.host.port).as_ref() != Some(&grant.host)
            || grant.username.is_empty()
            || grant.username.len() > 255
            || grant.username.chars().any(char::is_control)
            || !grant.host_key_sha256.starts_with("SHA256:")
            || grant.host_key_sha256.len() != 50
            || !grant.host_key_sha256[7..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
            || grant.route.is_empty()
            || grant.route.len() > 4096
            || grant.route.chars().any(char::is_control)
            || document.blocked_hosts.contains(&grant.host)
            || !grants.insert(grant)
        {
            return Err(ClipboardPolicyError::Invalid);
        }
    }
    Ok(())
}
pub fn encode_clipboard_policy(
    document: &ClipboardPolicyDocument,
) -> Result<Vec<u8>, ClipboardPolicyError> {
    validate_clipboard_policy(document)?;
    let bytes = serde_json::to_vec(document).map_err(|_| ClipboardPolicyError::Invalid)?;
    if bytes.len() > MAX_CLIPBOARD_POLICY_BYTES {
        return Err(ClipboardPolicyError::Invalid);
    }
    Ok(bytes)
}
#[async_trait]
pub trait ClipboardPolicyRepository: std::fmt::Debug + Send + Sync {
    async fn load_clipboard_policy(&self) -> Result<ClipboardPolicySnapshot, ClipboardPolicyError>;
    async fn save_clipboard_policy(
        &self,
        expected_revision: u64,
        document: &ClipboardPolicyDocument,
    ) -> Result<ClipboardPolicySnapshot, ClipboardPolicyError>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn automatic_policy_requires_version_two_canonical_bounded_and_unblocked_identity() {
        let identity = ClipboardTrust {
            host: ClipboardHost::new("example.test", 22).unwrap(),
            username: "alice".into(),
            host_key_sha256: format!("SHA256:{}", "a".repeat(43)),
            route: "[1,\"Direct\",null]".into(),
        };
        let mut document = ClipboardPolicyDocument {
            version: 2,
            automatic: vec![identity.clone()],
            ..Default::default()
        };
        assert_eq!(
            decode_clipboard_policy(&encode_clipboard_policy(&document).unwrap()).unwrap(),
            document
        );
        document.version = 1;
        assert!(validate_clipboard_policy(&document).is_err());
        document.version = 2;
        document.blocked_hosts.push(identity.host.clone());
        assert!(validate_clipboard_policy(&document).is_err());
        document.blocked_hosts.clear();
        document.automatic.push(identity);
        assert!(validate_clipboard_policy(&document).is_err());
        document.automatic.pop();
        document.automatic[0]
            .host_key_sha256
            .replace_range(7..8, "!");
        assert!(validate_clipboard_policy(&document).is_err());
    }
    #[test]
    fn policy_rejects_duplicates_noncanonical_versions_unknown_fields_and_limits() {
        let host = ClipboardHost::new("example.test", 22).unwrap();
        let mut document = ClipboardPolicyDocument {
            blocked_hosts: vec![host.clone()],
            ..Default::default()
        };
        assert!(validate_clipboard_policy(&document).is_ok());
        document.blocked_hosts.push(host.clone());
        assert!(validate_clipboard_policy(&document).is_err());
        document.blocked_hosts = vec![ClipboardHost {
            host: "EXAMPLE.test".into(),
            port: 22,
        }];
        assert!(validate_clipboard_policy(&document).is_err());
        document.blocked_hosts.clear();
        document.version = 3;
        assert!(validate_clipboard_policy(&document).is_err());
        assert!(decode_clipboard_policy(br#"{"format":"cshell.clipboard-policy","version":1,"blocked_hosts":[],"password":"secret"}"#).is_err());
        assert!(decode_clipboard_policy(&vec![b' '; MAX_CLIPBOARD_POLICY_BYTES + 1]).is_err());
        document.version = 1;
        document.blocked_hosts = (0..=MAX_CLIPBOARD_POLICY_HOSTS)
            .map(|i| ClipboardHost::new(&format!("host-{i}"), 22).unwrap())
            .collect();
        assert!(validate_clipboard_policy(&document).is_err());
        assert!(!format!("{document:?}").contains("host-0"));
    }
}
