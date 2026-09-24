//! Strict OpenSSH known_hosts verification for direct SSH connections.

use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{PublicKeyOrCertificate, ssh_key};
use sha1::Sha1;
use ssh_key::known_hosts::{HostPatterns, Marker};
use ssh_key::{HashAlg, PublicKey};
use std::io::Write;
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
    pub fn load_or_empty(
        path: impl AsRef<Path>,
        host: &str,
        port: u16,
    ) -> Result<Self, KnownHostsError> {
        match Self::load(path, host, port) {
            Ok(verifier) => Ok(verifier),
            Err(KnownHostsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Self::parse("", host, port)
            }
            Err(error) => Err(error),
        }
    }

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
    pub fn has_matching_entries(&self) -> bool {
        !self.entries.is_empty()
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

/// Persist a key only for a host that has no matching known_hosts entries.
/// The comment and key form an append-only audit record. A torn final write
/// fails closed when known_hosts is parsed on the next connection.
pub fn import_confirmed_host_key(
    path: &Path,
    host: &str,
    port: u16,
    public_key_line: &str,
    confirmed_fingerprint: &str,
    profile_id: &str,
) -> Result<(), String> {
    if host.is_empty()
        || host
            .chars()
            .any(|ch| ch.is_whitespace() || "#,*!?|".contains(ch))
        || profile_id.is_empty()
        || !profile_id
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() || ch == '-')
    {
        return Err("invalid host or Profile identifier for known_hosts".into());
    }
    let key = public_key_line
        .parse::<PublicKey>()
        .map_err(|error| error.to_string())?;
    let canonical_key = key.to_openssh().map_err(|error| error.to_string())?;
    let actual_fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    if confirmed_fingerprint != actual_fingerprint {
        return Err("confirmed fingerprint does not match the scanned host key".into());
    }
    let parent = path.parent().ok_or("known_hosts has no parent directory")?;
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
        }
    }
    let lock_path = path.with_file_name(format!(
        "{}.cshell.lock",
        path.file_name()
            .ok_or("known_hosts has no file name")?
            .to_string_lossy()
    ));
    let mut lock_options = std::fs::OpenOptions::new();
    lock_options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options.mode(0o600);
    }
    let lock = lock_options
        .open(lock_path)
        .map_err(|error| error.to_string())?;
    lock.lock().map_err(|error| error.to_string())?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err("known_hosts must be a regular file".into());
    }
    let existing = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("cannot read known_hosts: {error}")),
    };
    let contents = std::str::from_utf8(&existing).map_err(|_| "known_hosts is not UTF-8")?;
    let verifier =
        KnownHostsVerifier::parse(contents, host, port).map_err(|error| error.to_string())?;
    if verifier.has_matching_entries() {
        return Err("host already has a known_hosts entry; first-key import is blocked".into());
    }
    let lookup = if port == 22 {
        host.to_ascii_lowercase()
    } else {
        format!("[{}]:{}", host.to_ascii_lowercase(), port)
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let suffix = format!(
        "{}# CShell host-key-import v1 utc_unix={timestamp} profile={profile_id} fingerprint={actual_fingerprint}\n{lookup} {canonical_key}\n",
        if existing.is_empty() || existing.ends_with(b"\n") {
            ""
        } else {
            "\n"
        }
    );
    if existing.len() + suffix.len() > MAX_KNOWN_HOSTS_BYTES as usize {
        return Err("known_hosts exceeds the 8 MiB limit".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    file.write_all(suffix.as_bytes())
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    Ok(())
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
    fn first_key_import_requires_exact_fingerprint_and_leaves_audit_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".ssh").join("known_hosts");
        assert!(KnownHostsVerifier::load(&path, "example.com", 2200).is_err());
        let empty = KnownHostsVerifier::load_or_empty(&path, "example.com", 2200).unwrap();
        assert_eq!(empty.check(&server_key(KEY_ONE)), HostKeyCheck::Unknown);
        let fingerprint = KEY_ONE
            .parse::<PublicKey>()
            .unwrap()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        assert!(
            import_confirmed_host_key(
                &path,
                "example.com",
                2200,
                KEY_ONE,
                "SHA256:incorrect",
                "11111111-1111-1111-1111-111111111111"
            )
            .is_err()
        );
        assert!(!path.exists());
        import_confirmed_host_key(
            &path,
            "example.com",
            2200,
            KEY_ONE,
            &fingerprint,
            "11111111-1111-1111-1111-111111111111",
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("CShell host-key-import v1"));
        assert!(content.contains(&format!("fingerprint={fingerprint}")));
        assert!(content.contains(&format!("[example.com]:2200 {KEY_ONE}")));
        assert_eq!(
            KnownHostsVerifier::load(&path, "example.com", 2200)
                .unwrap()
                .check(&server_key(KEY_ONE)),
            HostKeyCheck::Trusted
        );
        assert!(
            import_confirmed_host_key(
                &path,
                "example.com",
                2200,
                KEY_TWO,
                &KEY_TWO
                    .parse::<PublicKey>()
                    .unwrap()
                    .fingerprint(HashAlg::Sha256)
                    .to_string(),
                "11111111-1111-1111-1111-111111111111"
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn existing_revocation_blocks_first_key_import() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("known_hosts");
        let content = format!("@revoked example.com {KEY_ONE}\n");
        std::fs::write(&path, &content).unwrap();
        let fingerprint = KEY_TWO
            .parse::<PublicKey>()
            .unwrap()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        assert!(
            import_confirmed_host_key(
                &path,
                "example.com",
                22,
                KEY_TWO,
                &fingerprint,
                "11111111-1111-1111-1111-111111111111"
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
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
