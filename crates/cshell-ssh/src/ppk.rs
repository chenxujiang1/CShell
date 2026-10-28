//! Bounded PPK envelope admission before the pinned SSH library performs MAC,
//! decryption and key decoding. No key material or comments leave this module.
use russh::keys::ssh_key::{HashAlg, PrivateKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;
mod rsa;
pub(super) struct DecodedKey {
    pub key: PrivateKey,
    pub rsa: Option<std::sync::Arc<crate::rsa_signer::RsaSigningKey>>,
}
impl std::fmt::Debug for DecodedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedKey").finish_non_exhaustive()
    }
}
pub const MAX_PPK_BYTES: usize = 64 * 1024;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PpkKdfInfo {
    pub algorithm: String,
    pub memory_kib: u32,
    pub passes: u32,
    pub parallelism: u32,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PpkInspection {
    pub version: u32,
    pub algorithm: String,
    pub encrypted: bool,
    pub kdf: Option<PpkKdfInfo>,
    pub sha256_fingerprint: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PpkError {
    #[error("PPK file exceeds byte, line, component or KDF limits")]
    Limit,
    #[error("PPK envelope is malformed or has unsupported headers")]
    Format,
    #[error("only PPK versions 2 and 3 are supported")]
    Version,
    #[error("PPK key algorithm is unsupported")]
    Algorithm,
    #[error("RSA PPK requires 2048–4096 bits and a bounded odd exponent of at least 65537")]
    RsaParameters,
    #[error("encrypted PPK requires an explicit passphrase")]
    PassphraseRequired,
    #[error("unencrypted PPK does not use a passphrase; leave it empty")]
    UnexpectedPassphrase,
    #[error("PPK key cannot be decoded or authenticated")]
    Authentication,
    #[error("PPK public/private key consistency could not be verified")]
    KeyConsistency,
    #[error("PPK decoder budget is busy; retry explicitly later")]
    Busy,
}
struct Header {
    version: u32,
    algorithm: String,
    encrypted: bool,
    kdf: Option<PpkKdfInfo>,
}
fn normalized(encoded: &str) -> Result<Zeroizing<String>, PpkError> {
    if encoded.len() > MAX_PPK_BYTES {
        return Err(PpkError::Limit);
    }
    let encoded = encoded.strip_prefix('\u{feff}').unwrap_or(encoded).trim();
    if encoded
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(PpkError::Format);
    }
    let mut result = Zeroizing::new(String::with_capacity(encoded.len()));
    let mut chars = encoded.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            result.push('\n');
        } else {
            result.push(c);
        }
    }
    Ok(result)
}
fn field<'a>(lines: &[&'a str], position: &mut usize, name: &str) -> Result<&'a str, PpkError> {
    let line = *lines.get(*position).ok_or(PpkError::Format)?;
    *position += 1;
    let (key, value) = line.split_once(": ").ok_or(PpkError::Format)?;
    if key != name {
        return Err(PpkError::Format);
    }
    Ok(value)
}
fn integer(text: &str) -> Result<u32, PpkError> {
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|c| c.is_ascii_digit()) {
        return Err(PpkError::Format);
    }
    text.parse().map_err(|_| PpkError::Format)
}
fn blob(lines: &[&str], position: &mut usize, name: &str, limit: usize) -> Result<(), PpkError> {
    let count =
        usize::try_from(integer(field(lines, position, name)?)?).map_err(|_| PpkError::Limit)?;
    if count == 0 || count > 2048 {
        return Err(PpkError::Limit);
    }
    let end = position
        .checked_add(count)
        .filter(|end| *end <= lines.len())
        .ok_or(PpkError::Format)?;
    let mut bytes = 0usize;
    for line in &lines[*position..end] {
        if line.is_empty()
            || line.len() > 128
            || !line
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='))
        {
            return Err(PpkError::Format);
        }
        bytes = bytes.checked_add(line.len()).ok_or(PpkError::Limit)?;
    }
    if bytes > limit {
        return Err(PpkError::Limit);
    }
    *position = end;
    Ok(())
}
fn preflight(encoded: &str) -> Result<Header, PpkError> {
    let lines = encoded.lines().collect::<Vec<_>>();
    if lines.len() > 4096 || lines.iter().any(|line| line.len() > 4096) {
        return Err(PpkError::Limit);
    }
    let first = *lines.first().ok_or(PpkError::Format)?;
    let (label, algorithm) = first.split_once(": ").ok_or(PpkError::Format)?;
    let version = match label {
        "PuTTY-User-Key-File-2" => 2,
        "PuTTY-User-Key-File-3" => 3,
        _ => return Err(PpkError::Version),
    };
    if !matches!(
        algorithm,
        "ssh-rsa"
            | "ssh-ed25519"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
    ) {
        return Err(PpkError::Algorithm);
    }
    let mut position = 1;
    let encrypted = match field(&lines, &mut position, "Encryption")? {
        "none" => false,
        "aes256-cbc" => true,
        _ => return Err(PpkError::Format),
    };
    let _comment = field(&lines, &mut position, "Comment")?;
    blob(&lines, &mut position, "Public-Lines", 8192)?;
    let kdf = if encrypted && version == 3 {
        let algorithm = field(&lines, &mut position, "Key-Derivation")?;
        if !matches!(algorithm, "Argon2d" | "Argon2i" | "Argon2id") {
            return Err(PpkError::Format);
        }
        let memory_kib = integer(field(&lines, &mut position, "Argon2-Memory")?)?;
        let passes = integer(field(&lines, &mut position, "Argon2-Passes")?)?;
        let parallelism = integer(field(&lines, &mut position, "Argon2-Parallelism")?)?;
        if !(1..=4).contains(&parallelism)
            || !(1..=8).contains(&passes)
            || memory_kib < 8 * parallelism
            || memory_kib > 65536
            || u64::from(memory_kib) * u64::from(passes) > 262144
        {
            return Err(PpkError::Limit);
        }
        let salt = field(&lines, &mut position, "Argon2-Salt")?;
        if !(16..=64).contains(&salt.len())
            || salt.len() % 2 != 0
            || !salt.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(PpkError::Format);
        }
        Some(PpkKdfInfo {
            algorithm: algorithm.into(),
            memory_kib,
            passes,
            parallelism,
        })
    } else {
        None
    };
    blob(&lines, &mut position, "Private-Lines", 32768)?;
    let mac = field(&lines, &mut position, "Private-MAC")?;
    if mac.len() != if version == 2 { 40 } else { 64 }
        || !mac.bytes().all(|c| c.is_ascii_hexdigit())
        || position != lines.len()
    {
        return Err(PpkError::Format);
    }
    Ok(Header {
        version,
        algorithm: algorithm.into(),
        encrypted,
        kdf,
    })
}
pub(super) fn decode(encoded: &str, passphrase: Option<&str>) -> Result<DecodedKey, PpkError> {
    Ok(decode_with_header(encoded, passphrase)?.0)
}
fn decode_with_header(
    encoded: &str,
    passphrase: Option<&str>,
) -> Result<(DecodedKey, Header), PpkError> {
    if passphrase.is_some_and(|p| p.len() > 16 * 1024) {
        return Err(PpkError::Limit);
    }
    let encoded = normalized(encoded)?;
    let header = preflight(&encoded)?;
    if header.encrypted && passphrase.is_none() {
        return Err(PpkError::PassphraseRequired);
    }
    if !header.encrypted && passphrase.is_some_and(|p| !p.is_empty()) {
        return Err(PpkError::UnexpectedPassphrase);
    }
    static BUDGET: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let _permit = BUDGET
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| PpkError::Busy)?;
    if header.algorithm == "ssh-rsa" {
        let (key, rsa) = rsa::decode(&encoded, &header, passphrase)?;
        return Ok((
            DecodedKey {
                key,
                rsa: Some(rsa),
            },
            header,
        ));
    }
    let key = PrivateKey::from_ppk(
        &*encoded,
        if header.encrypted {
            passphrase.map(str::to_owned)
        } else {
            None
        },
    )
    .map_err(|_| PpkError::Authentication)?;
    if key.algorithm().as_str() != header.algorithm {
        return Err(PpkError::KeyConsistency);
    }
    // Fresh local challenge. Neither the signature nor private components are
    // exported; verification checks the declared public identity against the key.
    let challenge: [u8; 32] = rand::random();
    let namespace = "cshell-ppk-key-consistency";
    let signature = key
        .sign(namespace, HashAlg::Sha256, &challenge)
        .map_err(|_| PpkError::KeyConsistency)?;
    key.public_key()
        .verify(namespace, &challenge, &signature)
        .map_err(|_| PpkError::KeyConsistency)?;
    Ok((DecodedKey { key, rsa: None }, header))
}
pub fn inspect_ppk(encoded: &str, passphrase: Option<&str>) -> Result<PpkInspection, PpkError> {
    let (key, header) = decode_with_header(encoded, passphrase)?;
    Ok(PpkInspection {
        version: header.version,
        algorithm: header.algorithm,
        encrypted: header.encrypted,
        kdf: header.kdf,
        sha256_fingerprint: key
            .key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string(),
    })
}
