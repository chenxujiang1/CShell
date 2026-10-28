use crate::{KeychainError, Secret, map_native_error};
use rand::TryRng;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Clone, Copy)]
pub struct VaultRootRef(Uuid);
impl VaultRootRef {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }
    pub fn bytes(&self) -> [u8; 16] {
        *self.0.as_bytes()
    }
}
impl Default for VaultRootRef {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for VaultRootRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultRootRef(..)")
    }
}
#[derive(Debug)]
pub struct SystemVaultRootKeys;
impl SystemVaultRootKeys {
    fn entry(id: VaultRootRef) -> Result<keyring::Entry, KeychainError> {
        keyring::Entry::new("org.cshell.vault.root.v1", &id.0.to_string()).map_err(map_native_error)
    }
    pub fn create(id: VaultRootRef) -> Result<Secret, KeychainError> {
        let entry = Self::entry(id)?;
        match entry
            .get_secret()
            .map(Zeroizing::new)
            .map_err(map_native_error)
        {
            Err(KeychainError::Missing) => {}
            Ok(_) => return Err(KeychainError::AlreadyExists),
            Err(error) => return Err(error),
        }
        let mut bytes = Zeroizing::new(vec![0; 32]);
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| KeychainError::OperationFailed)?;
        if let Err(error) = entry.set_secret(&bytes).map_err(map_native_error) {
            return match entry.delete_credential().map_err(map_native_error) {
                Ok(()) | Err(KeychainError::Missing) => Err(error),
                Err(_) => Err(KeychainError::CleanupRequired),
            };
        }
        Ok(Secret::new(std::mem::take(&mut *bytes)))
    }
    pub fn read(id: VaultRootRef) -> Result<Secret, KeychainError> {
        let mut bytes = Zeroizing::new(Self::entry(id)?.get_secret().map_err(map_native_error)?);
        if bytes.len() != 32 {
            return Err(KeychainError::InvalidCredential);
        }
        Ok(Secret::new(std::mem::take(&mut *bytes)))
    }
    pub fn delete(id: VaultRootRef) -> Result<(), KeychainError> {
        match Self::entry(id)?
            .delete_credential()
            .map_err(map_native_error)
        {
            Err(KeychainError::Missing) => Ok(()),
            other => other,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_vault_root_roundtrip_is_bounded_opaque_and_never_overwrites()
    -> Result<(), KeychainError> {
        if std::env::var_os("CSHELL_KEYCHAIN_NATIVE_TEST").is_none() {
            return Ok(());
        }
        let id = VaultRootRef::new();
        let key = SystemVaultRootKeys::create(id)?;
        let result = (|| {
            assert_eq!(key.expose().len(), 32);
            assert_eq!(SystemVaultRootKeys::read(id)?.expose(), key.expose());
            assert_eq!(
                SystemVaultRootKeys::create(id).err(),
                Some(KeychainError::AlreadyExists)
            );
            assert!(!format!("{id:?}").contains(&id.0.to_string()));
            Ok(())
        })();
        SystemVaultRootKeys::delete(id)?;
        result
    }
}
