#![allow(clippy::unwrap_used)]

use super::*;

fn password() -> Secret {
    Secret::new(b"test-master-password".to_vec())
}

#[test]
fn auxiliary_metadata_signature_requires_unlock_and_binding_detects_disk_changes() {
    use ring::signature::{ED25519, UnparsedPublicKey};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("metadata.vault");
    let mut vault = PortableVault::create(&path, &password()).unwrap();
    let binding = vault.metadata_binding().unwrap();
    let (public, signature) = vault.sign_metadata(b"routing metadata").unwrap();
    UnparsedPublicKey::new(&ED25519, public).verify(b"routing metadata", &signature).unwrap();
    assert!(UnparsedPublicKey::new(&ED25519, public).verify(b"modified metadata", &signature).is_err());
    assert_eq!(vault.status().record_count, 0);
    vault.lock();
    assert_eq!(vault.metadata_binding().unwrap(), binding);
    assert_eq!(vault.sign_metadata(b"anything").unwrap_err(), VaultError::Locked);
    std::fs::write(&path, b"damaged").unwrap();
    assert!(vault.metadata_binding().is_err());
}

#[test]
fn ciphertext_publication_retry_is_bounded_preserves_target_and_never_replays_creation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("destination");
    std::fs::write(&path, b"prior-ciphertext").unwrap();
    let mut temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
    temporary.write_all(b"new-ciphertext").unwrap();
    temporary.as_file().sync_all().unwrap();
    let staged = temporary.path().to_owned();
    let mut calls = 0;
    let mut delays = Vec::new();
    publish_with(
        temporary,
        false,
        true,
        |file| {
            calls += 1;
            assert_eq!(file.path(), staged);
            assert_eq!(std::fs::read(file.path()).unwrap(), b"new-ciphertext");
            if calls < 3 {
                assert_eq!(std::fs::read(&path).unwrap(), b"prior-ciphertext");
                Err(tempfile::PersistError {
                    error: std::io::Error::from_raw_os_error(5),
                    file,
                })
            } else {
                file.persist(&path)
            }
        },
        |delay| delays.push(delay),
    )
    .unwrap();
    assert!((3..=8).contains(&calls));
    assert_eq!(delays.len(), calls - 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"new-ciphertext");
    for (creating, windows, code, expected) in [
        (false, true, 5, 8),
        (true, true, 5, 1),
        (false, false, 5, 1),
        (false, true, 22, 1),
    ] {
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let staging = temporary.path().to_owned();
        let mut calls = 0;
        let mut elapsed = std::time::Duration::ZERO;
        assert!(
            publish_with(
                temporary,
                creating,
                windows,
                |file| {
                    calls += 1;
                    Err(tempfile::PersistError {
                        error: std::io::Error::from_raw_os_error(code),
                        file,
                    })
                },
                |delay| elapsed += delay
            )
            .is_err()
        );
        assert_eq!(calls, expected);
        assert!(elapsed <= std::time::Duration::from_millis(127));
        assert!(!staging.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"new-ciphertext");
    }
    let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
    let missing = directory.path().join("moved-staging");
    let result = publish_with(
        temporary,
        false,
        true,
        |file| {
            std::fs::rename(file.path(), &missing).unwrap();
            Err(tempfile::PersistError {
                error: std::io::Error::from_raw_os_error(5),
                file,
            })
        },
        |_| panic!("uncertain publication must not retry"),
    );
    assert_eq!(result, Err(VaultError::WriteUncertain));
    assert_eq!(std::fs::read(&path).unwrap(), b"new-ciphertext");
}

#[test]
fn credential_batch_is_atomic_and_profile_mode_survives_reopen() {
    let (directory, mut vault) = fixture();
    let mode = reference(VaultCredentialKind::ProfileMode);
    let key = VaultCredentialRef {
        kind: VaultCredentialKind::PrivateKey,
        ..mode
    };
    let before = std::fs::read(directory.path().join("vault.bin")).unwrap();
    assert_eq!(
        vault
            .write_batch(&[
                (mode, "mode", &Secret::new(vec![1])),
                (key, "", &Secret::new(b"key".to_vec()))
            ])
            .unwrap_err(),
        VaultError::InvalidInput
    );
    assert_eq!(
        std::fs::read(directory.path().join("vault.bin")).unwrap(),
        before
    );
    assert!(!vault.contains(mode).unwrap());
    vault
        .write_batch(&[
            (mode, "mode", &Secret::new(vec![1])),
            (key, "target", &Secret::new(b"key".to_vec())),
        ])
        .unwrap();
    assert_eq!(vault.status().revision, 1);
    drop(vault);
    let mut vault = PortableVault::open(directory.path().join("vault.bin")).unwrap();
    assert_eq!(vault.contains(mode).unwrap_err(), VaultError::Locked);
    vault.unlock(&password()).unwrap();
    assert_eq!(vault.read(mode, "mode").unwrap().expose(), &[1]);
    assert_eq!(vault.read(key, "target").unwrap().expose(), b"key");
}
fn reference(kind: VaultCredentialKind) -> VaultCredentialRef {
    VaultCredentialRef {
        profile_id: *uuid::Uuid::now_v7().as_bytes(),
        kind,
    }
}
fn fixture() -> (tempfile::TempDir, PortableVault) {
    let directory = tempfile::tempdir().unwrap();
    let vault = PortableVault::create(directory.path().join("vault.bin"), &password()).unwrap();
    (directory, vault)
}

#[test]
fn portable_vault_reopens_locked_and_failed_unlock_discards_previous_keys() {
    let (directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::Password);
    vault
        .write(id, "ssh-target", &Secret::new(b"stored-secret".to_vec()))
        .unwrap();
    vault.lock();
    assert_eq!(
        vault.read(id, "ssh-target").unwrap_err(),
        VaultError::Locked
    );
    assert_eq!(vault.delete(id).unwrap_err(), VaultError::Locked);
    assert_eq!(
        vault.change_master_password(&password()).unwrap_err(),
        VaultError::Locked
    );
    vault.unlock(&password()).unwrap();
    assert_eq!(
        vault.read(id, "ssh-target").unwrap().expose(),
        b"stored-secret"
    );
    assert_eq!(
        vault
            .unlock(&Secret::new(b"incorrect-password".to_vec()))
            .unwrap_err(),
        VaultError::AuthenticationFailed
    );
    assert!(!vault.status().unlocked);
    drop(vault);
    let mut reopened = PortableVault::open(directory.path().join("vault.bin")).unwrap();
    assert!(!reopened.status().unlocked);
    reopened.unlock(&password()).unwrap();
    assert_eq!(
        reopened.read(id, "ssh-target").unwrap().expose(),
        b"stored-secret"
    );
}

#[test]
fn portable_records_bind_identity_kind_and_target_without_plaintext_on_disk_or_debug() {
    let (directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::Password);
    let marker = b"PRIVATE_VAULT_SENTINEL";
    let secret = Secret::new(marker.to_vec());
    for kind in [
        VaultCredentialKind::Password,
        VaultCredentialKind::KeyPassphrase,
        VaultCredentialKind::PrivateKey,
    ] {
        vault
            .write(
                VaultCredentialRef { kind, ..id },
                "PRIVATE_BINDING_SENTINEL",
                &secret,
            )
            .unwrap();
    }
    let bytes = std::fs::read(directory.path().join("vault.bin")).unwrap();
    assert!(!bytes.windows(marker.len()).any(|window| window == marker));
    let binding_marker = b"PRIVATE_BINDING_SENTINEL";
    assert!(
        !bytes
            .windows(binding_marker.len())
            .any(|window| window == binding_marker)
    );
    assert_eq!(
        vault.read(id, "other-target").unwrap_err(),
        VaultError::BindingMismatch
    );
    assert_eq!(
        vault
            .read(
                reference(VaultCredentialKind::Password),
                "PRIVATE_BINDING_SENTINEL"
            )
            .unwrap_err(),
        VaultError::Missing
    );
    assert!(!format!("{vault:?} {secret:?}").contains("SENTINEL"));
    let error = VaultError::from(std::io::Error::other("PRIVATE_BACKEND_SENTINEL"));
    assert!(!format!("{error} {error:?}").contains("SENTINEL"));
    assert!(std::error::Error::source(&error).is_none());
    for (label, reference) in [
        (
            "identity",
            VaultCredentialRef {
                profile_id: [0x55; 16],
                ..id
            },
        ),
        (
            "kind",
            VaultCredentialRef {
                kind: VaultCredentialKind::PrivateKey,
                ..id
            },
        ),
    ] {
        let record = vault.document.records.get(&id).unwrap();
        let keys = vault.active_keys().unwrap();
        assert!(
            matches!(
                decrypt(
                    &keys[..32],
                    record,
                    &record_aad(&vault.document.header, reference)
                ),
                Err(VaultError::AuthenticationFailed)
            ),
            "{label}"
        );
    }
    let original = vault.document.records.get(&id).unwrap().nonce;
    vault
        .write(id, "PRIVATE_BINDING_SENTINEL", &secret)
        .unwrap();
    assert_ne!(vault.document.records.get(&id).unwrap().nonce, original);
}

#[test]
fn portable_manifest_authenticates_removal_header_and_ciphertext() {
    let (_directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::Password);
    vault
        .write(id, "target", &Secret::new(b"secret".to_vec()))
        .unwrap();
    let keys = vault.active_keys().unwrap();
    let encoded = std::fs::read(&vault.path).unwrap();
    let mut altered = encoded.clone();
    altered[60] ^= 1; // revision belongs to the authenticated header
    let mut samples = vec![altered];
    let mut altered = encoded.clone();
    altered[105] ^= 1; // record ciphertext
    samples.push(altered);
    let mut altered = encoded.clone();
    let record_bytes = 33 + vault.document.records.get(&id).unwrap().ciphertext.len();
    altered.drain(68..68 + record_bytes);
    altered[64..68].copy_from_slice(&0u32.to_be_bytes());
    samples.push(altered);
    for bytes in samples {
        let document = decode(&bytes).unwrap();
        assert!(matches!(
            decrypt(
                &keys[32..],
                &document.manifest,
                &document.authenticated_prefix
            ),
            Err(VaultError::AuthenticationFailed)
        ));
    }
    let mut altered = encoded;
    *altered.last_mut().unwrap() ^= 1;
    std::fs::write(&vault.path, altered).unwrap();
    vault.reload().unwrap();
    assert_eq!(
        vault.unlock(&password()).unwrap_err(),
        VaultError::AuthenticationFailed
    );
    assert!(!vault.status().unlocked);
}

#[test]
fn portable_rotation_reencrypts_private_key_and_rejects_old_password_after_reopen() {
    let (directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::PrivateKey);
    let private_key = Secret::new(vec![0x72; MAX_KEY_BYTES]);
    vault.write(id, "key-binding", &private_key).unwrap();
    let original = std::fs::read(&vault.path).unwrap();
    let old_salt = vault.document.header.salt;
    let new_password = Secret::new(b"replacement-master-password".to_vec());
    vault.change_master_password(&new_password).unwrap();
    assert_ne!(vault.document.header.salt, old_salt);
    assert_eq!(
        vault.read(id, "key-binding").unwrap().expose(),
        private_key.expose()
    );
    assert_ne!(std::fs::read(&vault.path).unwrap(), original);
    assert_eq!(vault.status().revision, 2);
    drop(vault);
    let mut reopened = PortableVault::open(directory.path().join("vault.bin")).unwrap();
    assert_eq!(
        reopened.unlock(&password()).unwrap_err(),
        VaultError::AuthenticationFailed
    );
    reopened.unlock(&new_password).unwrap();
    assert_eq!(
        reopened.read(id, "key-binding").unwrap().expose(),
        private_key.expose()
    );
    reopened.delete(id).unwrap();
    let revision = reopened.status().revision;
    reopened.delete(id).unwrap();
    assert_eq!(reopened.status().revision, revision);
    assert_eq!(
        reopened.read(id, "key-binding").unwrap_err(),
        VaultError::Missing
    );
}

#[test]
fn portable_exclusive_ownership_and_create_never_replace_existing_file() {
    let (_directory, vault) = fixture();
    let path = vault.path.clone();
    assert_eq!(PortableVault::open(&path).unwrap_err(), VaultError::Busy);
    assert_eq!(
        PortableVault::create(&path, &password()).unwrap_err(),
        VaultError::Busy
    );
    let bytes = std::fs::read(&path).unwrap();
    drop(vault);
    assert_eq!(
        PortableVault::create(&path, &password()).unwrap_err(),
        VaultError::AlreadyExists
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let reopened = PortableVault::open(&path).unwrap();
    assert!(!reopened.status().unlocked);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn portable_failed_and_uncertain_writes_preserve_or_reload_the_real_committed_state() {
    let (_directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::Password);
    vault
        .write(id, "target", &Secret::new(b"original".to_vec()))
        .unwrap();
    let original = std::fs::read(&vault.path).unwrap();
    let mut candidate = vault.document.clone();
    candidate.header.revision += 1;
    assert_eq!(
        vault
            .commit_with(candidate.clone(), None, |_, _| Err(VaultError::Io(
                std::io::ErrorKind::PermissionDenied
            )))
            .unwrap_err(),
        VaultError::Io(std::io::ErrorKind::PermissionDenied)
    );
    assert!(vault.status().unlocked);
    assert_eq!(std::fs::read(&vault.path).unwrap(), original);
    assert_eq!(vault.status().revision, 1);
    assert_eq!(
        vault
            .commit_with(candidate, None, |path, encoded| {
                persist(path, encoded, false)?;
                Err(VaultError::WriteUncertain)
            })
            .unwrap_err(),
        VaultError::WriteUncertain
    );
    assert!(!vault.status().unlocked);
    assert!(vault.status().reload_required);
    assert_eq!(
        vault.read(id, "target").unwrap_err(),
        VaultError::ReloadRequired
    );
    assert_eq!(
        vault.unlock(&password()).unwrap_err(),
        VaultError::ReloadRequired
    );
    vault.reload().unwrap();
    assert_eq!(vault.status().revision, 2);
    vault.unlock(&password()).unwrap();
    assert_eq!(vault.read(id, "target").unwrap().expose(), b"original");
    std::fs::write(&vault.path, b"corrupted").unwrap();
    assert_eq!(vault.reload().unwrap_err(), VaultError::Corrupt);
    assert!(!vault.status().unlocked);
    assert!(vault.status().reload_required);
}

#[test]
fn portable_hostile_formats_and_kdf_parameters_are_rejected_before_unlock() {
    let (_directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::Password);
    vault
        .write(id, "target", &Secret::new(b"secret".to_vec()))
        .unwrap();
    let encoded = std::fs::read(&vault.path).unwrap();
    for size in 0..encoded.len() {
        assert!(decode(&encoded[..size]).is_err(), "truncation {size}");
    }
    let mut bytes = encoded.clone();
    bytes[8..12].copy_from_slice(&2u32.to_be_bytes());
    assert!(matches!(decode(&bytes), Err(VaultError::Unsupported)));
    for offset in [44, 48, 52] {
        let mut bytes = encoded.clone();
        bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(decode(&bytes), Err(VaultError::Unsupported)));
    }
    let mut bytes = encoded.clone();
    bytes[64..68].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(decode(&bytes), Err(VaultError::Capacity)));
    let mut bytes = encoded.clone();
    bytes[84] = 255; // unknown credential kind
    assert!(matches!(decode(&bytes), Err(VaultError::Corrupt)));
    let mut bytes = encoded.clone();
    bytes[97..101].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(decode(&bytes), Err(VaultError::Capacity)));
    let record_end = 101 + vault.document.records.get(&id).unwrap().ciphertext.len();
    let mut bytes = encoded.clone();
    bytes.splice(
        record_end..record_end,
        encoded[68..record_end].iter().copied(),
    );
    bytes[64..68].copy_from_slice(&2u32.to_be_bytes());
    assert!(matches!(decode(&bytes), Err(VaultError::Corrupt)));
    let mut bytes = encoded;
    bytes.push(0);
    assert!(matches!(decode(&bytes), Err(VaultError::Corrupt)));
    assert!(matches!(
        decode(&vec![0; MAX_FILE_BYTES + 1]),
        Err(VaultError::Capacity)
    ));
    assert_eq!(
        PortableVault::create(
            vault.path.with_file_name("invalid.bin"),
            &Secret::new(b"short".to_vec())
        )
        .unwrap_err(),
        VaultError::InvalidInput
    );
    assert_eq!(
        PortableVault::open("relative.bin").unwrap_err(),
        VaultError::InvalidInput
    );
}

#[test]
fn portable_rotation_io_failure_keeps_old_password_and_file() {
    let (directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::PrivateKey);
    vault
        .write(
            id,
            "key-binding",
            &Secret::new(b"private-key-fixture".to_vec()),
        )
        .unwrap();
    let original_path = vault.path.clone();
    let original = std::fs::read(&original_path).unwrap();
    let original_salt = vault.document.header.salt;
    vault.path = directory.path().join("missing-parent").join("vault.bin");
    assert!(matches!(
        vault.change_master_password(&Secret::new(b"replacement-password".to_vec())),
        Err(VaultError::Io(std::io::ErrorKind::NotFound))
    ));
    vault.path = original_path;
    assert!(vault.status().unlocked);
    assert_eq!(vault.document.header.salt, original_salt);
    assert_eq!(std::fs::read(&vault.path).unwrap(), original);
    vault.lock();
    vault.unlock(&password()).unwrap();
    assert_eq!(
        vault.read(id, "key-binding").unwrap().expose(),
        b"private-key-fixture"
    );
}

#[test]
fn portable_record_and_file_limits_reject_changes_without_losing_previous_credentials() {
    let (_directory, mut vault) = fixture();
    let id = reference(VaultCredentialKind::Password);
    for (binding, secret) in [
        ("".into(), vec![1]),
        ("nul\0target".into(), vec![1]),
        ("x".repeat(MAX_BINDING_BYTES + 1), vec![1]),
        ("valid".into(), vec![]),
        ("valid".into(), vec![1; 4097]),
    ] {
        assert_eq!(
            vault.write(id, &binding, &Secret::new(secret)).unwrap_err(),
            VaultError::InvalidInput
        );
    }
    for _ in 0..MAX_RECORDS {
        vault
            .write(
                reference(VaultCredentialKind::Password),
                "target",
                &Secret::new(b"small".to_vec()),
            )
            .unwrap();
    }
    assert_eq!(
        vault
            .write(id, "target", &Secret::new(b"over-capacity".to_vec()))
            .unwrap_err(),
        VaultError::Capacity
    );
    assert_eq!(vault.status().record_count, MAX_RECORDS);
    let original = std::fs::read(&vault.path).unwrap();
    let mut candidate = vault.document.clone();
    candidate.records.clear();
    // Real public writes hit the aggregate ciphertext bound before replacement.
    vault.commit(candidate, None).unwrap();
    let secret = Secret::new(vec![1; MAX_KEY_BYTES]);
    loop {
        match vault.write(
            reference(VaultCredentialKind::PrivateKey),
            "target",
            &secret,
        ) {
            Ok(()) => {}
            Err(VaultError::Capacity) => break,
            Err(error) => panic!("unexpected failure: {error}"),
        }
    }
    assert!(vault.status().record_count < MAX_RECORDS);
    assert!(std::fs::read(&vault.path).unwrap().len() <= MAX_FILE_BYTES);
    assert_ne!(std::fs::read(&vault.path).unwrap(), original);
    let existing = *vault.document.records.keys().next().unwrap();
    assert_eq!(
        vault.read(existing, "target").unwrap().expose(),
        secret.expose()
    );
}

#[test]
fn protection_copy_keeps_all_record_kinds_and_bindings_and_never_replaces_existing_files() {
    let root=tempfile::tempdir().unwrap();let old=Secret::new(b"source-master-password".to_vec());let new=Secret::new(b"target-master-password".to_vec());
    let source_path=root.path().join("source.vault");let target_path=root.path().join("target.vault");let mut source=PortableVault::create(&source_path,&old).unwrap();
    let kinds=[VaultCredentialKind::Password,VaultCredentialKind::KeyPassphrase,VaultCredentialKind::PrivateKey,VaultCredentialKind::ProfileMode,VaultCredentialKind::ManagedKey,VaultCredentialKind::VaultSettings];
    for (i,kind) in kinds.into_iter().enumerate(){source.write(VaultCredentialRef{profile_id:[i as u8+1;16],kind},"kept-binding",&Secret::new(b"kept-secret".to_vec())).unwrap();}
    let before=std::fs::read(&source_path).unwrap();let target=source.copy_protected(&target_path,&new).unwrap();
    for (i,kind) in kinds.into_iter().enumerate(){assert_eq!(target.read(VaultCredentialRef{profile_id:[i as u8+1;16],kind},"kept-binding").unwrap().expose(),b"kept-secret");}
    assert_eq!(std::fs::read(&source_path).unwrap(),before);assert_eq!(source.status().record_count,target.status().record_count);
    assert!(source.verify_password(&new).is_err());assert!(source.status().unlocked);assert!(source.copy_protected(&target_path,&new).is_err());
    drop(target);let mut target=PortableVault::open(&target_path).unwrap();assert!(target.unlock(&old).is_err());target.unlock(&new).unwrap();
    source.lock();assert!(source.copy_protected(root.path().join("must-not-exist"),&new).is_err());assert!(!root.path().join("must-not-exist").exists());
}
