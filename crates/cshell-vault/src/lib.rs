//! Phase 0 system-keychain probe, not the Phase 1 Vault.
//! Only the daemon links this crate. IDs are unique and use a dedicated service.

use std::fmt;
use std::sync::Mutex;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const PROBE_SERVICE: &str = "org.cshell.phase0.keychain-probe";
const PROFILE_PASSWORD_SERVICE: &str = "org.cshell.profile.password";
const PROFILE_KEY_PASSPHRASE_SERVICE: &str = "org.cshell.profile.key-passphrase";
static PROBE_OPERATIONS: Mutex<()> = Mutex::new(());

/// Opaque account name for one temporary probe entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeId(String);

impl ProbeId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7().to_string())
    }
}

impl Default for ProbeId {
    fn default() -> Self {
        Self::new()
    }
}

/// Bytes are redacted from Debug and wiped on drop.
pub struct Secret(Vec<u8>);

impl Secret {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Never exposes platform error strings, which may include sensitive data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeychainError {
    Missing,
    AlreadyExists,
    AccessDenied,
    Unavailable,
    OperationFailed,
    /// A failed write may have created an entry and cleanup failed too.
    /// Retain the ProbeId and retry delete once access is restored.
    CleanupRequired,
    BindingMismatch,
    InvalidCredential,
}

trait Backend {
    fn get(&self, id: &ProbeId) -> Result<Secret, KeychainError>;
    fn set(&self, id: &ProbeId, secret: &Secret) -> Result<(), KeychainError>;
    fn delete(&self, id: &ProbeId) -> Result<(), KeychainError>;
}

struct ProbeStore<B>(B);

impl<B: Backend> ProbeStore<B> {
    fn write_new(&self, id: &ProbeId, secret: &Secret) -> Result<(), KeychainError> {
        match self.0.get(id) {
            Err(KeychainError::Missing) => {}
            Ok(_) => return Err(KeychainError::AlreadyExists),
            Err(error) => return Err(error),
        }
        if let Err(error) = self.0.set(id, secret) {
            return match self.0.delete(id) {
                Ok(()) | Err(KeychainError::Missing) => Err(error),
                Err(_) => Err(KeychainError::CleanupRequired),
            };
        }
        Ok(())
    }

    fn read(&self, id: &ProbeId) -> Result<Secret, KeychainError> {
        self.0.get(id)
    }

    fn delete(&self, id: &ProbeId) -> Result<(), KeychainError> {
        match self.0.delete(id) {
            Ok(()) | Err(KeychainError::Missing) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Native Keychain Services / Credential Manager / Secret Service adapter.
#[derive(Debug, Default)]
pub struct SystemKeychain;

impl SystemKeychain {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Only creates a new temporary entry. Failed preflight never writes.
    pub fn write_new(&self, id: &ProbeId, secret: &Secret) -> Result<(), KeychainError> {
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ProbeStore(NativeBackend).write_new(id, secret)
    }

    pub fn read(&self, id: &ProbeId) -> Result<Secret, KeychainError> {
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ProbeStore(NativeBackend).read(id)
    }

    /// Idempotent cleanup, safe to retry after a transient failure.
    pub fn delete(&self, id: &ProbeId) -> Result<(), KeychainError> {
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ProbeStore(NativeBackend).delete(id)
    }
}

/// The Profile UUID is the opaque keychain reference; SQLite never stores a password.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfilePasswordRef(String);

impl ProfilePasswordRef {
    #[must_use]
    pub fn from_profile_bytes(id: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(id).to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfilePasswordBinding {
    pub host: String,
    pub port: u16,
    pub username: String,
}

const PASSWORD_MAGIC: &[u8] = b"CSHELLPWD1";

fn encode_profile_password(
    binding: &ProfilePasswordBinding,
    secret: &Secret,
) -> Result<Zeroizing<Vec<u8>>, KeychainError> {
    let host_len =
        u16::try_from(binding.host.len()).map_err(|_| KeychainError::InvalidCredential)?;
    let user_len =
        u16::try_from(binding.username.len()).map_err(|_| KeychainError::InvalidCredential)?;
    let secret_len =
        u16::try_from(secret.expose().len()).map_err(|_| KeychainError::InvalidCredential)?;
    if binding.port == 0 || secret_len == 0 || secret_len > 4096 {
        return Err(KeychainError::InvalidCredential);
    }
    let mut encoded = Zeroizing::new(Vec::with_capacity(
        PASSWORD_MAGIC.len()
            + 8
            + binding.host.len()
            + binding.username.len()
            + secret.expose().len(),
    ));
    encoded.extend_from_slice(PASSWORD_MAGIC);
    encoded.extend_from_slice(&host_len.to_be_bytes());
    encoded.extend_from_slice(binding.host.as_bytes());
    encoded.extend_from_slice(&binding.port.to_be_bytes());
    encoded.extend_from_slice(&user_len.to_be_bytes());
    encoded.extend_from_slice(binding.username.as_bytes());
    encoded.extend_from_slice(&secret_len.to_be_bytes());
    encoded.extend_from_slice(secret.expose());
    Ok(encoded)
}

fn take_part<'a>(encoded: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], KeychainError> {
    let length = encoded
        .get(*cursor..*cursor + 2)
        .ok_or(KeychainError::InvalidCredential)?;
    *cursor += 2;
    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
    let part = encoded
        .get(*cursor..*cursor + length)
        .ok_or(KeychainError::InvalidCredential)?;
    *cursor += length;
    Ok(part)
}

fn decode_profile_password(
    encoded: &[u8],
    binding: &ProfilePasswordBinding,
) -> Result<Secret, KeychainError> {
    if !encoded.starts_with(PASSWORD_MAGIC) {
        return Err(KeychainError::InvalidCredential);
    }
    let mut cursor = PASSWORD_MAGIC.len();
    let host = take_part(encoded, &mut cursor)?;
    let port = encoded
        .get(cursor..cursor + 2)
        .ok_or(KeychainError::InvalidCredential)?;
    cursor += 2;
    let port = u16::from_be_bytes([port[0], port[1]]);
    let user = take_part(encoded, &mut cursor)?;
    let password = take_part(encoded, &mut cursor)?;
    if cursor != encoded.len() || password.is_empty() || password.len() > 4096 {
        return Err(KeychainError::InvalidCredential);
    }
    if host != binding.host.as_bytes()
        || port != binding.port
        || user != binding.username.as_bytes()
    {
        return Err(KeychainError::BindingMismatch);
    }
    Ok(Secret::new(password.to_vec()))
}

#[derive(Debug, Default)]
pub struct SystemProfilePasswordVault;

impl SystemProfilePasswordVault {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn entry(id: &ProfilePasswordRef) -> Result<keyring::Entry, KeychainError> {
        keyring::Entry::new(PROFILE_PASSWORD_SERVICE, &id.0).map_err(map_native_error)
    }

    pub fn write(
        &self,
        id: &ProfilePasswordRef,
        binding: &ProfilePasswordBinding,
        secret: &Secret,
    ) -> Result<(), KeychainError> {
        let encoded = encode_profile_password(binding, secret)?;
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::entry(id)?
            .set_secret(&encoded)
            .map_err(map_native_error)
    }

    pub fn read(
        &self,
        id: &ProfilePasswordRef,
        binding: &ProfilePasswordBinding,
    ) -> Result<Secret, KeychainError> {
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let encoded = Zeroizing::new(Self::entry(id)?.get_secret().map_err(map_native_error)?);
        decode_profile_password(&encoded, binding)
    }

    pub fn delete(&self, id: &ProfilePasswordRef) -> Result<(), KeychainError> {
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match Self::entry(id)?
            .delete_credential()
            .map_err(map_native_error)
        {
            Ok(()) | Err(KeychainError::Missing) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Opaque reference to a key passphrase in the system keychain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileKeyPassphraseRef(String);

impl ProfileKeyPassphraseRef {
    #[must_use]
    pub fn from_profile_bytes(id: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(id).to_string())
    }
}

const KEY_PASSPHRASE_MAGIC: &[u8] = b"CSHELLKEY1";

#[derive(Debug, Default)]
pub struct SystemProfileKeyPassphraseVault;

impl SystemProfileKeyPassphraseVault {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn entry(id: &ProfileKeyPassphraseRef) -> Result<keyring::Entry, KeychainError> {
        keyring::Entry::new(PROFILE_KEY_PASSPHRASE_SERVICE, &id.0).map_err(map_native_error)
    }

    pub fn write(
        &self,
        id: &ProfileKeyPassphraseRef,
        key_path: &str,
        secret: &Secret,
    ) -> Result<(), KeychainError> {
        let path_len =
            u16::try_from(key_path.len()).map_err(|_| KeychainError::InvalidCredential)?;
        let secret_len =
            u16::try_from(secret.expose().len()).map_err(|_| KeychainError::InvalidCredential)?;
        if path_len == 0 || secret_len == 0 || secret_len > 4096 {
            return Err(KeychainError::InvalidCredential);
        }
        let mut encoded = Zeroizing::new(Vec::with_capacity(
            KEY_PASSPHRASE_MAGIC.len() + 4 + key_path.len() + secret.expose().len(),
        ));
        encoded.extend_from_slice(KEY_PASSPHRASE_MAGIC);
        encoded.extend_from_slice(&path_len.to_be_bytes());
        encoded.extend_from_slice(key_path.as_bytes());
        encoded.extend_from_slice(&secret_len.to_be_bytes());
        encoded.extend_from_slice(secret.expose());
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::entry(id)?
            .set_secret(&encoded)
            .map_err(map_native_error)
    }

    pub fn read(
        &self,
        id: &ProfileKeyPassphraseRef,
        key_path: &str,
    ) -> Result<Secret, KeychainError> {
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let encoded = Zeroizing::new(Self::entry(id)?.get_secret().map_err(map_native_error)?);
        if !encoded.starts_with(KEY_PASSPHRASE_MAGIC) {
            return Err(KeychainError::InvalidCredential);
        }
        let mut cursor = KEY_PASSPHRASE_MAGIC.len();
        let stored_path = take_part(&encoded, &mut cursor)?;
        let passphrase = take_part(&encoded, &mut cursor)?;
        if cursor != encoded.len() || passphrase.is_empty() || passphrase.len() > 4096 {
            return Err(KeychainError::InvalidCredential);
        }
        if stored_path != key_path.as_bytes() {
            return Err(KeychainError::BindingMismatch);
        }
        Ok(Secret::new(passphrase.to_vec()))
    }

    pub fn delete(&self, id: &ProfileKeyPassphraseRef) -> Result<(), KeychainError> {
        let _guard = PROBE_OPERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match Self::entry(id)?
            .delete_credential()
            .map_err(map_native_error)
        {
            Ok(()) | Err(KeychainError::Missing) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

struct NativeBackend;

impl NativeBackend {
    fn entry(id: &ProbeId) -> Result<keyring::Entry, KeychainError> {
        keyring::Entry::new(PROBE_SERVICE, &id.0).map_err(map_native_error)
    }
}

impl Backend for NativeBackend {
    fn get(&self, id: &ProbeId) -> Result<Secret, KeychainError> {
        Self::entry(id)?
            .get_secret()
            .map(Secret::new)
            .map_err(map_native_error)
    }

    fn set(&self, id: &ProbeId, secret: &Secret) -> Result<(), KeychainError> {
        Self::entry(id)?
            .set_secret(secret.expose())
            .map_err(map_native_error)
    }

    fn delete(&self, id: &ProbeId) -> Result<(), KeychainError> {
        Self::entry(id)?
            .delete_credential()
            .map_err(map_native_error)
    }
}

fn map_native_error(error: keyring::Error) -> KeychainError {
    match error {
        keyring::Error::NoEntry => KeychainError::Missing,
        keyring::Error::NoStorageAccess(_) => KeychainError::AccessDenied,
        keyring::Error::NoDefaultStore | keyring::Error::PlatformFailure(_) => {
            KeychainError::Unavailable
        }
        _ => KeychainError::OperationFailed,
    }
}

#[cfg(test)]
mod tests;
