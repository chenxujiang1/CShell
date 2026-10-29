//! Non-secret, signed routing index. Missing/stale metadata requires one unlock;
//! it never means that a profile should fall back to native credentials.
use super::*;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    path::Path,
};

const MAX_INDEX_BYTES: u64 = 32 * 1024;
const DOMAIN: &[u8] = b"CSHELL-VAULT-ROUTING-v1\0";
// The public anchor lives in the daemon-private directory, under the same OS
// ownership boundary as activation markers. Replacing the entire private data
// directory is not authenticated by a self-contained local index.

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    profile: [u8; 16],
    portable: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u8,
    vault: [u8; 16],
    revision: u64,
    fingerprint: [u8; 32],
    default_portable: bool,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: String,
    signature: Vec<u8>,
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

pub(super) fn evidence_exists(path: &Path) -> Result<bool, std::io::Error> {
    Ok(sidecar(path, ".routes").try_exists()? || sidecar(path, ".routes.pub").try_exists()?)
}

fn read(path: &Path, maximum: u64) -> Result<Vec<u8>, VaultError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum {
        return Err(VaultError::Corrupt);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(VaultError::Corrupt);
        }
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(VaultError::Capacity);
    }
    Ok(bytes)
}

fn persist(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    let parent = path.parent().ok_or(VaultError::InvalidInput)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| VaultError::from(e.error))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) fn publish(state: &State) -> Result<(), VaultError> {
    let Backend::Portable(vault) = &state.backend else {
        return Ok(());
    };
    if !vault.status().unlocked {
        return Ok(());
    }
    let (id, revision, fingerprint) = vault.metadata_binding()?;
    let default_portable =
        match vault.read(rebuild::default_mode_ref(), rebuild::DEFAULT_MODE_BINDING) {
            Ok(value) if value.expose() == [1] => true,
            Err(VaultError::Missing) => false,
            Ok(_) => return Err(VaultError::Corrupt),
            Err(error) => return Err(error),
        };
    let entries = vault
        .references(Kind::ProfileMode)?
        .into_iter()
        .map(|reference| {
            Ok(Entry {
                profile: reference.profile_id,
                portable: mode_value(vault, ProfileId::from_bytes(reference.profile_id))?,
            })
        })
        .collect::<Result<Vec<_>, VaultError>>()?;
    let payload = serde_json::to_string(&Payload {
        version: 1,
        vault: id,
        revision,
        fingerprint,
        default_portable,
        entries,
    })
    .map_err(|_| VaultError::InvalidInput)?;
    let mut message = DOMAIN.to_vec();
    message.extend_from_slice(payload.as_bytes());
    let (public, signature) = vault.sign_metadata(&message)?;
    let encoded = serde_json::to_vec(&Envelope { payload, signature })
        .map_err(|_| VaultError::InvalidInput)?;
    if encoded.len() as u64 > MAX_INDEX_BYTES {
        return Err(VaultError::Capacity);
    }
    let index = sidecar(&state.path, ".routes");
    let anchor = sidecar(&state.path, ".routes.pub");
    // If interrupted between replacements, signatures/bindings cannot match;
    // only an authenticated unlock may repair the index and its public anchor.
    if read(&index, MAX_INDEX_BYTES).is_ok_and(|bytes| bytes == encoded)
        && read(&anchor, 32).is_ok_and(|bytes| bytes == public)
    {
        return Ok(());
    }
    persist(&index, &encoded)?;
    persist(&anchor, &public)
}

pub(super) fn mode(state: &State, id: ProfileId) -> Result<bool, VaultError> {
    let Backend::Portable(vault) = &state.backend else {
        return Err(VaultError::Locked);
    };
    // Disk loss/corruption is not repaired or treated as native storage.
    let binding = vault.metadata_binding()?;
    let read_index = || -> Result<bool, VaultError> {
        let public = read(&sidecar(&state.path, ".routes.pub"), 32)?;
        if public.len() != 32 {
            return Err(VaultError::Corrupt);
        }
        let envelope: Envelope =
            serde_json::from_slice(&read(&sidecar(&state.path, ".routes"), MAX_INDEX_BYTES)?)
                .map_err(|_| VaultError::Corrupt)?;
        if envelope.signature.len() != 64 {
            return Err(VaultError::Corrupt);
        }
        let mut message = DOMAIN.to_vec();
        message.extend_from_slice(envelope.payload.as_bytes());
        UnparsedPublicKey::new(&ED25519, &public)
            .verify(&message, &envelope.signature)
            .map_err(|_| VaultError::AuthenticationFailed)?;
        let payload: Payload =
            serde_json::from_str(&envelope.payload).map_err(|_| VaultError::Corrupt)?;
        if payload.version != 1
            || (payload.vault, payload.revision, payload.fingerprint) != binding
            || payload.entries.len() > 128
        {
            return Err(VaultError::ReloadRequired);
        }
        let mut seen = BTreeSet::new();
        if payload
            .entries
            .iter()
            .any(|entry| entry.profile == [0; 16] || !seen.insert(entry.profile))
        {
            return Err(VaultError::Corrupt);
        }
        Ok(payload
            .entries
            .iter()
            .find(|entry| entry.profile == *id.as_uuid().as_bytes())
            .map_or(payload.default_portable, |entry| entry.portable))
    };
    // Legacy, stale, missing, or modified indexes require an actual unlock.
    // They never authorize a native credential read.
    read_index().map_err(|_| VaultError::Locked)
}

#[cfg(test)]
mod tests;
