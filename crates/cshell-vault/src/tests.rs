use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct FakeBackend {
    values: Mutex<HashMap<String, Vec<u8>>>,
    get_failure: Option<KeychainError>,
    set_failure: Option<KeychainError>,
    delete_failure: Option<KeychainError>,
    commit_before_set_failure: bool,
}

impl Backend for FakeBackend {
    fn get(&self, id: &ProbeId) -> Result<Secret, KeychainError> {
        if let Some(error) = self.get_failure {
            return Err(error);
        }
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id.0)
            .cloned()
            .map(Secret::new)
            .ok_or(KeychainError::Missing)
    }

    fn set(&self, id: &ProbeId, secret: &Secret) -> Result<(), KeychainError> {
        if self.set_failure.is_none() || self.commit_before_set_failure {
            self.values
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id.0.clone(), secret.expose().to_vec());
        }
        self.set_failure.map_or(Ok(()), Err)
    }

    fn delete(&self, id: &ProbeId) -> Result<(), KeychainError> {
        if let Some(error) = self.delete_failure {
            return Err(error);
        }
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id.0)
            .map(|mut bytes| bytes.zeroize())
            .ok_or(KeychainError::Missing)
    }
}

#[test]
fn round_trip_and_idempotent_delete() {
    let store = ProbeStore(FakeBackend::default());
    let id = ProbeId::new();
    let secret = Secret::new(b"transient-probe".to_vec());
    assert_eq!(store.write_new(&id, &secret), Ok(()));
    assert_eq!(
        store.read(&id).map(|value| value.expose().to_vec()),
        Ok(secret.expose().to_vec())
    );
    assert_eq!(store.delete(&id), Ok(()));
    assert_eq!(store.delete(&id), Ok(()));
    assert_eq!(store.read(&id).err(), Some(KeychainError::Missing));
    assert!(!format!("{secret:?}").contains("transient-probe"));
}

#[test]
fn existing_entry_cannot_be_overwritten() {
    let store = ProbeStore(FakeBackend::default());
    let id = ProbeId::new();
    assert_eq!(
        store.write_new(&id, &Secret::new(b"first".to_vec())),
        Ok(())
    );
    assert_eq!(
        store.write_new(&id, &Secret::new(b"second".to_vec())),
        Err(KeychainError::AlreadyExists)
    );
    assert_eq!(
        store.read(&id).map(|value| value.expose().to_vec()),
        Ok(b"first".to_vec())
    );
}

#[test]
fn unavailable_or_denied_preflight_never_writes() {
    for failure in [KeychainError::Unavailable, KeychainError::AccessDenied] {
        let store = ProbeStore(FakeBackend {
            get_failure: Some(failure),
            ..FakeBackend::default()
        });
        let id = ProbeId::new();
        assert_eq!(
            store.write_new(&id, &Secret::new(b"secret".to_vec())),
            Err(failure)
        );
        assert!(
            store
                .0
                .values
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }
}

#[test]
fn ambiguous_failed_write_is_cleaned_and_other_entries_survive() {
    let id = ProbeId::new();
    let untouched = ProbeId::new();
    let mut backend = FakeBackend {
        set_failure: Some(KeychainError::Unavailable),
        commit_before_set_failure: true,
        ..FakeBackend::default()
    };
    backend
        .values
        .get_mut()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(untouched.0.clone(), b"keep".to_vec());
    let store = ProbeStore(backend);
    assert_eq!(
        store.write_new(&id, &Secret::new(b"new".to_vec())),
        Err(KeychainError::Unavailable)
    );
    assert_eq!(store.read(&id).err(), Some(KeychainError::Missing));
    assert_eq!(
        store.read(&untouched).map(|value| value.expose().to_vec()),
        Ok(b"keep".to_vec())
    );
}

#[test]
fn failed_cleanup_is_reported_and_can_be_retried() {
    let id = ProbeId::new();
    let store = ProbeStore(FakeBackend {
        set_failure: Some(KeychainError::Unavailable),
        delete_failure: Some(KeychainError::AccessDenied),
        commit_before_set_failure: true,
        ..FakeBackend::default()
    });
    assert_eq!(
        store.write_new(&id, &Secret::new(b"new".to_vec())),
        Err(KeychainError::CleanupRequired)
    );
    assert_eq!(store.delete(&id), Err(KeychainError::AccessDenied));
    let recovered = ProbeStore(FakeBackend {
        values: store.0.values,
        ..FakeBackend::default()
    });
    assert_eq!(recovered.delete(&id), Ok(()));
    assert_eq!(recovered.read(&id).err(), Some(KeychainError::Missing));
}

#[test]
fn native_roundtrip() {
    if std::env::var_os("CSHELL_KEYCHAIN_NATIVE_TEST").is_none() {
        return;
    }
    let keychain = SystemKeychain::new();
    let id = ProbeId::new();
    let secret = Secret::new(format!("cshell-native-{}", Uuid::now_v7()).into_bytes());
    let result = (|| {
        keychain.write_new(&id, &secret)?;
        if keychain.write_new(&id, &Secret::new(b"replacement".to_vec()))
            != Err(KeychainError::AlreadyExists)
        {
            return Err(KeychainError::OperationFailed);
        }
        let read = keychain.read(&id)?;
        if read.expose() != secret.expose() {
            return Err(KeychainError::OperationFailed);
        }
        keychain.delete(&id)?;
        if keychain.read(&id).err() != Some(KeychainError::Missing) {
            return Err(KeychainError::OperationFailed);
        }
        Ok(())
    })();
    let _ = keychain.delete(&id);
    assert_eq!(result, Ok(()));
}
