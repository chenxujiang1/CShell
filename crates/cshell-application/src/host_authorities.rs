//! Managed public host CA policy and bounded, explicitly reviewed changes.
use cshell_domain::{ClipboardHost, HostAuthorityId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
pub const MAX_HOST_CA_DOCUMENT: usize = 2 * 1024 * 1024;
pub const MAX_HOST_AUTHORITIES: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCaState {
    Trusted,
    Disabled,
    Revoked,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCaDraft {
    pub name: String,
    pub patterns: String,
    pub public_key: String,
    pub state: HostCaState,
}
impl Default for HostCaDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            patterns: String::new(),
            public_key: String::new(),
            state: HostCaState::Disabled,
        }
    }
}
impl HostCaDraft {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
            || self.patterns.is_empty()
            || self.patterns.len() > 2048
            || self
                .patterns
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || self.public_key.is_empty()
            || self.public_key.len() > 16384
            || self.public_key.contains(['\r', '\n', '\0'])
        {
            return Err("invalid host CA draft");
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCaRecord {
    pub id: HostAuthorityId,
    pub definition: HostCaDraft,
    pub fingerprint: String,
}
fn fingerprint(value: &str) -> bool {
    value.len() == 50
        && value.starts_with("SHA256:")
        && value[7..]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCaCatalog {
    pub format: String,
    pub version: u32,
    pub revision: u64,
    pub items: Vec<HostCaRecord>,
}
impl Default for HostCaCatalog {
    fn default() -> Self {
        Self {
            format: "cshell.host-authorities".into(),
            version: 1,
            revision: 1,
            items: Vec::new(),
        }
    }
}
impl HostCaCatalog {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "cshell.host-authorities"
            || self.version != 1
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.items.len() > MAX_HOST_AUTHORITIES
        {
            return Err("invalid host CA catalog");
        }
        let mut ids = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for row in &self.items {
            row.definition.validate()?;
            if row.id.as_uuid().is_nil()
                || !ids.insert(row.id)
                || !fingerprint(&row.fingerprint)
                || !keys.insert(&row.fingerprint)
            {
                return Err("duplicate or invalid host CA identity");
            }
        }
        Ok(())
    }
    pub fn changed(&mut self) -> Result<(), &'static str> {
        self.revision = self
            .revision
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or("host CA revision exhausted")?;
        self.validate()
    }
}
pub fn decode_host_ca_catalog(bytes: &[u8]) -> Result<HostCaCatalog, &'static str> {
    if bytes.len() > MAX_HOST_CA_DOCUMENT {
        return Err("host CA catalog limit");
    }
    let value: HostCaCatalog = serde_json::from_slice(bytes).map_err(|_| "invalid host CA JSON")?;
    value.validate()?;
    Ok(value)
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCaPreview {
    pub format: String,
    pub version: u32,
    pub catalog_revision: u64,
    pub id: HostAuthorityId,
    pub record: Option<HostCaRecord>,
    pub fingerprint: String,
    pub token: [u8; 32],
    pub expires_unix: u64,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCertificateRequest {
    pub certificate: String,
    pub target: Option<ClipboardHost>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateHostDecision {
    Trusted,
    Unknown,
    Changed,
    Revoked,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCertificateReport {
    pub certificate_sha256: [u8; 32],
    pub catalog_revision: Option<u64>,
    pub host_certificate: bool,
    pub serial: u64,
    pub key_id: String,
    pub principals: Vec<String>,
    pub valid_after: u64,
    pub valid_before: u64,
    pub checked_unix: u64,
    pub signature_valid: bool,
    pub time_valid: bool,
    pub subject_fingerprint: String,
    pub signer_fingerprint: String,
    pub critical_options: Vec<String>,
    pub extensions: Vec<String>,
    pub target: Option<ClipboardHost>,
    pub host_decision: Option<CertificateHostDecision>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HostCaReply {
    Catalog(HostCaCatalog),
    Preview(HostCaPreview),
    Certificate(HostCertificateReport),
}
#[derive(Debug)]
pub enum HostCaReplyExpectation<'a> {
    Catalog,
    PreviewSave(HostAuthorityId, u64, &'a HostCaDraft),
    PreviewRemove(HostAuthorityId, u64),
    Applied(&'a HostCaPreview),
    Certificate(&'a HostCertificateRequest),
}
impl HostCaReply {
    pub fn confirm(&self, expected: HostCaReplyExpectation<'_>) -> Result<(), &'static str> {
        let valid = match (self, expected) {
            (Self::Catalog(_), HostCaReplyExpectation::Catalog) => true,
            (Self::Preview(p), HostCaReplyExpectation::PreviewSave(id, revision, draft)) => {
                p.id == id
                    && p.catalog_revision == revision
                    && p.record.as_ref().is_some_and(|r| {
                        r.definition.name == draft.name
                            && r.definition.patterns == draft.patterns
                            && r.definition.state == draft.state
                            && r.definition
                                .public_key
                                .split_whitespace()
                                .take(2)
                                .eq(draft.public_key.split_whitespace().take(2))
                    })
            }
            (Self::Preview(p), HostCaReplyExpectation::PreviewRemove(id, revision)) => {
                p.id == id && p.catalog_revision == revision && p.record.is_none()
            }
            (Self::Catalog(c), HostCaReplyExpectation::Applied(p)) => {
                p.catalog_revision.checked_add(1) == Some(c.revision)
                    && match &p.record {
                        Some(record) => c.items.iter().find(|r| r.id == p.id) == Some(record),
                        None => !c.items.iter().any(|r| r.id == p.id),
                    }
            }
            (Self::Certificate(report), HostCaReplyExpectation::Certificate(request)) => {
                use sha2::Digest;
                let digest: [u8; 32] =
                    sha2::Sha256::digest(request.certificate.trim().as_bytes()).into();
                report.certificate_sha256 == digest && report.target == request.target
            }
            _ => false,
        };
        if !valid {
            return Err("host CA reply scope or effect mismatch");
        }
        Ok(())
    }
}
pub fn decode_host_ca_reply(bytes: &[u8]) -> Result<HostCaReply, &'static str> {
    if bytes.len() > MAX_HOST_CA_DOCUMENT {
        return Err("host CA reply limit");
    }
    let value: HostCaReply = serde_json::from_slice(bytes).map_err(|_| "invalid host CA reply")?;
    match &value {
        HostCaReply::Catalog(c) => c.validate()?,
        HostCaReply::Preview(p) => {
            if p.format != "cshell.host-ca-preview"
                || p.version != 1
                || p.id.as_uuid().is_nil()
                || p.catalog_revision == 0
                || p.catalog_revision > i64::MAX as u64
                || p.token == [0; 32]
                || !fingerprint(&p.fingerprint)
            {
                return Err("invalid CA preview binding");
            }
            if let Some(r) = &p.record {
                r.definition.validate()?;
                if r.id != p.id || r.fingerprint != p.fingerprint {
                    return Err("CA preview identity mismatch");
                }
            }
        }
        HostCaReply::Certificate(r) => {
            if r.key_id.len() > 1024
                || r.key_id.chars().any(char::is_control)
                || r.principals.len() > 256
                || r.principals
                    .iter()
                    .any(|p| p.len() > 256 || p.chars().any(char::is_control))
                || !fingerprint(&r.subject_fingerprint)
                || !fingerprint(&r.signer_fingerprint)
                || r.critical_options.len() > 64
                || r.extensions.len() > 64
                || r.critical_options
                    .iter()
                    .chain(&r.extensions)
                    .any(|n| n.len() > 128 || n.chars().any(char::is_control))
                || r.target.is_some() != r.host_decision.is_some()
                || r.target.is_some() != r.catalog_revision.is_some()
                || r.catalog_revision
                    .is_some_and(|n| n == 0 || n > i64::MAX as u64)
            {
                return Err("certificate report limit or scope mismatch");
            }
        }
    }
    Ok(value)
}
macro_rules! redact{($($t:ty),*)=>{$(impl std::fmt::Debug for $t{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str(concat!(stringify!($t)," { [REDACTED] }"))}})*};}
redact!(
    HostCaDraft,
    HostCaRecord,
    HostCaCatalog,
    HostCaPreview,
    HostCertificateRequest,
    HostCertificateReport,
    HostCaReply
);
