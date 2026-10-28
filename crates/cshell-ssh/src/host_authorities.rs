//! Public host authority normalization and certificate inspection, with no network I/O.
use crate::{HostKeyCheck, KnownHostsVerifier};
use russh::keys::{
    PublicKeyOrCertificate,
    ssh_key::{self, HashAlg, PublicKey, known_hosts::HostPatterns},
};
#[derive(Clone)]
pub struct HostAuthorityRule {
    pub patterns: String,
    pub public_key: String,
    pub revoked: bool,
}
impl std::fmt::Debug for HostAuthorityRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostAuthorityRule { [REDACTED] }")
    }
}
pub fn normalize_host_authority(
    patterns: &str,
    line: &str,
) -> Result<(String, String), &'static str> {
    if patterns.is_empty()
        || patterns.len() > 2048
        || patterns
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || line.len() > 16384
        || line.trim().contains(['\r', '\n', '\0'])
    {
        return Err("host CA input limit");
    }
    let parsed = patterns
        .parse::<HostPatterns>()
        .map_err(|_| "invalid OpenSSH host patterns")?;
    if let HostPatterns::Patterns(values) = parsed {
        if values.len() > 64 || !values.iter().any(|v| !v.is_empty() && !v.starts_with('!')) {
            return Err("host CA patterns need a positive bounded scope");
        }
    }
    let mut key = line
        .trim()
        .parse::<PublicKey>()
        .map_err(|_| "host CA requires an OpenSSH public key")?;
    key.set_comment("");
    let canonical = key
        .to_openssh()
        .map_err(|_| "host CA key encoding failed")?;
    if !matches!(
        canonical.split_whitespace().next(),
        Some(
            "ssh-ed25519"
                | "ssh-rsa"
                | "ecdsa-sha2-nistp256"
                | "ecdsa-sha2-nistp384"
                | "ecdsa-sha2-nistp521"
        )
    ) {
        return Err("unsupported host CA public key algorithm");
    }
    Ok((canonical, key.fingerprint(HashAlg::Sha256).to_string()))
}
#[derive(Clone)]
pub struct CertificateFacts {
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
    pub host_decision: Option<HostKeyCheck>,
}
impl std::fmt::Debug for CertificateFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CertificateFacts { [REDACTED] }")
    }
}
pub fn inspect_openssh_certificate(
    line: &str,
    verifier: Option<&KnownHostsVerifier>,
) -> Result<CertificateFacts, &'static str> {
    if line.is_empty() || line.len() > 65536 {
        return Err("certificate input limit");
    }
    let cert = ssh_key::Certificate::from_openssh(line.trim())
        .map_err(|_| "invalid OpenSSH public certificate")?;
    if cert.key_id().len() > 1024
        || cert.valid_principals().len() > 256
        || cert
            .valid_principals()
            .iter()
            .any(|p| p.len() > 256 || p.chars().any(char::is_control))
        || cert.key_id().chars().any(char::is_control)
        || cert.critical_options().len() > 64
        || cert.extensions().len() > 64
        || cert
            .critical_options()
            .keys()
            .chain(cert.extensions().keys())
            .any(|name| name.len() > 128 || name.chars().any(char::is_control))
    {
        return Err("certificate metadata limit");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "certificate clock unavailable")?
        .as_secs();
    // Mathematical signature checking is reported separately from host trust.
    let mut facts = CertificateFacts {
        host_certificate: cert.cert_type() == ssh_key::certificate::CertType::Host,
        serial: cert.serial(),
        key_id: cert.key_id().into(),
        principals: cert.valid_principals().to_vec(),
        valid_after: cert.valid_after(),
        valid_before: cert.valid_before(),
        checked_unix: now,
        signature_valid: cert.verify_signature().is_ok(),
        time_valid: cert.valid_after() <= now && now < cert.valid_before(),
        subject_fingerprint: cert.public_key().fingerprint(HashAlg::Sha256).to_string(),
        signer_fingerprint: cert
            .signature_key()
            .fingerprint(HashAlg::Sha256)
            .to_string(),
        critical_options: cert.critical_options().keys().cloned().collect(),
        extensions: cert.extensions().keys().cloned().collect(),
        host_decision: None,
    };
    if let Some(verifier) = verifier {
        facts.host_decision = Some(verifier.check_at(&PublicKeyOrCertificate::from(cert), now));
    }
    Ok(facts)
}
