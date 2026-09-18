//! Phase 0 system-keychain probe, not the Phase 1 Vault.
//! Only the daemon links this crate. IDs are unique and use a dedicated service.

use std::fmt;
use std::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroize;

const PROBE_SERVICE: &str = "org.cshell.phase0.keychain-probe";
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
