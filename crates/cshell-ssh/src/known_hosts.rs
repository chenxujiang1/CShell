//! Strict OpenSSH known_hosts verification for direct SSH connections.

use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{PublicKeyOrCertificate, ssh_key};
use sha1::Sha1;
use ssh_key::known_hosts::{HostPatterns, Marker};
use ssh_key::{HashAlg, PublicKey};
use std::path::Path;
use thiserror::Error;

const MAX_KNOWN_HOSTS_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostKeyCheck {
    Trusted,
    Unknown,
    Changed,
    Revoked,
}

#[derive(Debug, Error)]
pub enum KnownHostsError {
    #[error("cannot read known_hosts: {0}")]
    Io(#[from] std::io::Error),
    #[error("known_hosts exceeds the 8 MiB limit")]
    TooLarge,
    #[error("invalid known_hosts entry at line {line}")]
    InvalidEntry { line: usize },
    #[error("invalid SSH port 0")]
    InvalidPort,
}

#[derive(Clone, Debug)]
struct HostEntry {
    marker: Option<Marker>,
    key: PublicKey,
}

/// A snapshot of matching known_hosts entries. Unknown and changed keys are rejected.
#[derive(Clone, Debug)]
pub struct KnownHostsVerifier {
    host: String,
    port: u16,
    entries: Vec<HostEntry>,
}

impl KnownHostsVerifier {
    pub fn load(path: impl AsRef<Path>, host: &str, port: u16) -> Result<Self, KnownHostsError> {
        if port == 0 {
            return Err(KnownHostsError::InvalidPort);
        }
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() > MAX_KNOWN_HOSTS_BYTES {
            return Err(KnownHostsError::TooLarge);
        }
        let bytes = std::fs::read_to_string(path)?;
        Self::parse(&bytes, host, port)
    }

    pub fn parse(contents: &str, host: &str, port: u16) -> Result<Self, KnownHostsError> {
        if port == 0 {
            return Err(KnownHostsError::InvalidPort);
        }
        if contents.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
            return Err(KnownHostsError::TooLarge);
        }
        let host = host.to_ascii_lowercase();
        let lookup = if port == 22 {
            host.clone()
        } else {
            format!("[{host}]:{port}")
        };
        let mut entries = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split_whitespace();
            let first = fields
                .next()
                .ok_or(KnownHostsError::InvalidEntry { line: index + 1 })?;
            let (marker, patterns) = if first.starts_with('@') {
                let marker = first
                    .parse::<Marker>()
                    .map_err(|_| KnownHostsError::InvalidEntry { line: index + 1 })?;
                let patterns = fields
                    .next()
                    .ok_or(KnownHostsError::InvalidEntry { line: index + 1 })?;
                (Some(marker), patterns)
            } else {
                (None, first)
            };
            let key_type = fields
                .next()
                .ok_or(KnownHostsError::InvalidEntry { line: index + 1 })?;
            let encoded = fields
                .next()
                .ok_or(KnownHostsError::InvalidEntry { line: index + 1 })?;
            let patterns = patterns
                .parse::<HostPatterns>()
                .map_err(|_| KnownHostsError::InvalidEntry { line: index + 1 })?;
            if !host_matches(&patterns, &lookup) {
                continue;
            }
            let key = format!("{key_type} {encoded}")
                .parse::<PublicKey>()
                .map_err(|_| KnownHostsError::InvalidEntry { line: index + 1 })?;
            entries.push(HostEntry { marker, key });
        }
        Ok(Self {
            host,
            port,
            entries,
        })
    }

    #[must_use]
    pub fn target(&self) -> (&str, u16) {
        (&self.host, self.port)
    }

    #[must_use]
    pub fn check(&self, server_key: &PublicKeyOrCertificate) -> HostKeyCheck {
        let raw = server_key.public_key();
        let certificate = server_key.certificate();
        if self.entries.iter().any(|entry| {
            entry.marker == Some(Marker::Revoked)
                && (entry.key == raw
                    || certificate.is_some_and(|cert| entry.key.key_data() == cert.signature_key()))
        }) {
            return HostKeyCheck::Revoked;
        }
        if let Some(cert) = certificate {
            if cert.cert_type() != ssh_key::certificate::CertType::Host
                || !cert.critical_options().is_empty()
                || (!cert.valid_principals().is_empty()
                    && !cert
                        .valid_principals()
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&self.host)))
            {
                return HostKeyCheck::Changed;
            }
            let mut ca_found = false;
            for entry in &self.entries {
                if entry.marker == Some(Marker::CertAuthority)
                    && entry.key.key_data() == cert.signature_key()
                {
                    ca_found = true;
                    let fingerprint = entry.key.fingerprint(HashAlg::Sha256);
                    if cert.validate([&fingerprint]).is_ok() {
                        return HostKeyCheck::Trusted;
                    }
                }
            }
            return if ca_found {
                HostKeyCheck::Changed
            } else {
                HostKeyCheck::Unknown
            };
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.marker.is_none() && entry.key == raw)
        {
            return HostKeyCheck::Trusted;
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.marker.is_none() && entry.key.algorithm() == raw.algorithm())
        {
            HostKeyCheck::Changed
        } else {
            HostKeyCheck::Unknown
        }
    }
}

fn host_matches(patterns: &HostPatterns, host: &str) -> bool {
    match patterns {
        HostPatterns::HashedName { salt, hash } => {
            let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(salt) else {
                return false;
            };
            mac.update(host.as_bytes());
            mac.verify_slice(hash).is_ok()
        }
        HostPatterns::Patterns(patterns) => {
            let mut positive = false;
            for pattern in patterns {
                if let Some(negated) = pattern.strip_prefix('!') {
                    if glob_match(negated.as_bytes(), host.as_bytes()) {
                        return false;
                    }
                } else if glob_match(pattern.as_bytes(), host.as_bytes()) {
                    positive = true;
                }
            }
            positive
        }
    }
}

fn glob_match(pattern: &[u8], value: &[u8]) -> bool {
    let (mut p, mut v, mut star, mut after_star) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&value[v])) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            after_star = v;
        } else if let Some(saved) = star {
            after_star += 1;
            v = after_star;
            p = saved + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rand::rng;
    use ssh_key::{Algorithm, PrivateKey};

    const KEY_ONE: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_TWO: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
    const HASHED_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";

    fn server_key(encoded: &str) -> PublicKeyOrCertificate {
        PublicKeyOrCertificate::from(encoded.parse::<PublicKey>().unwrap())
    }

    #[test]
    fn exact_port_hashed_and_wildcard_names_match() {
        let exact = KnownHostsVerifier::parse(
            &format!("[example.com]:2200 {KEY_ONE}\n"),
            "example.com",
            2200,
        )
        .unwrap();
        assert_eq!(exact.check(&server_key(KEY_ONE)), HostKeyCheck::Trusted);
        assert_eq!(exact.check(&server_key(KEY_TWO)), HostKeyCheck::Changed);
        assert_eq!(
            KnownHostsVerifier::parse(&format!("example.com {KEY_ONE}\n"), "example.com", 2200)
                .unwrap()
                .check(&server_key(KEY_ONE)),
            HostKeyCheck::Unknown
        );

        let hashed = KnownHostsVerifier::parse(
            &format!("|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= {HASHED_KEY}\n"),
            "example.com",
            22,
        )
        .unwrap();
        assert_eq!(hashed.check(&server_key(HASHED_KEY)), HostKeyCheck::Trusted);

        let wildcard = format!("*.example.com,!bad.example.com\t{KEY_ONE} comment\n");
        assert_eq!(
            KnownHostsVerifier::parse(&wildcard, "good.example.com", 22)
                .unwrap()
                .check(&server_key(KEY_ONE)),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            KnownHostsVerifier::parse(&wildcard, "bad.example.com", 22)
                .unwrap()
                .check(&server_key(KEY_ONE)),
            HostKeyCheck::Unknown
        );
    }

    #[test]
    fn revoked_key_overrides_matching_trusted_entry() {
        let file = format!("example.com {KEY_ONE}\n@revoked example.com {KEY_ONE}\n");
        let verifier = KnownHostsVerifier::parse(&file, "example.com", 22).unwrap();
        assert_eq!(verifier.check(&server_key(KEY_ONE)), HostKeyCheck::Revoked);
        let ca_only = KnownHostsVerifier::parse(
            &format!("@cert-authority example.com {KEY_ONE}\n"),
            "example.com",
            22,
        )
        .unwrap();
        assert_eq!(ca_only.check(&server_key(KEY_ONE)), HostKeyCheck::Unknown);
    }

    #[test]
    fn host_certificate_requires_matching_ca_principal_and_valid_signature() {
        let ca = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let host = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut builder = ssh_key::certificate::Builder::new_with_random_nonce(
            &mut rng(),
            host.public_key(),
            now.saturating_sub(60),
            now.saturating_add(3600),
        )
        .unwrap();
        builder
            .cert_type(ssh_key::certificate::CertType::Host)
            .unwrap();
        builder.valid_principal("host.example.com").unwrap();
        let certificate = PublicKeyOrCertificate::Certificate(builder.sign(&ca).unwrap());
        let ca_entry = format!(
            "@cert-authority *.example.com {}\n",
            ca.public_key().to_openssh().unwrap()
        );
        assert_eq!(
            KnownHostsVerifier::parse(&ca_entry, "host.example.com", 22)
                .unwrap()
                .check(&certificate),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            KnownHostsVerifier::parse(&ca_entry, "other.example.com", 22)
                .unwrap()
                .check(&certificate),
            HostKeyCheck::Changed
        );
        let revoked = format!(
            "{ca_entry}@revoked *.example.com {}\n",
            ca.public_key().to_openssh().unwrap()
        );
        assert_eq!(
            KnownHostsVerifier::parse(&revoked, "host.example.com", 22)
                .unwrap()
                .check(&certificate),
            HostKeyCheck::Revoked
        );
    }
}
