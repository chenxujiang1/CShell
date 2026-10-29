#![allow(clippy::unwrap_used)]
use super::*;

fn password() -> Secret {
    Secret::new(b"routing-test-master".to_vec())
}
fn fixture() -> (tempfile::TempDir, Arc<VaultController>, SshConnectionRecord) {
    let root = tempfile::tempdir().unwrap();
    let c = VaultController::new(root.path().join("master.vault"));
    c.create(&password(), 0).unwrap();
    let target = crate::vault_control::tests::target();
    c.set_mode(target.profile_id, 0, true).unwrap();
    c.edit(
        &target,
        Kind::Password,
        Some(&Secret::new(b"routing-server-secret".to_vec())),
    )
    .unwrap();
    c.lock();
    (root, c, target)
}

#[test]
fn locked_and_reopened_indexes_distinguish_native_and_portable_without_exposing_secrets() {
    let (root, c, target) = fixture();
    let native = ProfileId::new();
    assert!(c.mode(target.profile_id).unwrap());
    assert!(!c.mode(native).unwrap());
    assert_eq!(
        c.read(&target, Kind::Password).unwrap_err(),
        VaultError::Locked
    );
    let encoded = std::fs::read(sidecar(&c.guard().path, ".routes")).unwrap();
    assert!(
        !encoded
            .windows(b"routing-server-secret".len())
            .any(|w| w == b"routing-server-secret")
    );
    drop(c);
    let reopened = VaultController::new(root.path().join("master.vault"));
    assert!(!reopened.response(None).unlocked);
    assert!(reopened.mode(target.profile_id).unwrap());
    assert!(!reopened.mode(native).unwrap());
}

#[test]
fn unsigned_modified_and_foreign_indexes_never_authorize_native_fallback() {
    let (_root, c, target) = fixture();
    let path = c.guard().path.clone();
    let index = sidecar(&path, ".routes");
    let original = std::fs::read(&index).unwrap();
    let mut envelope: Envelope = serde_json::from_slice(&original).unwrap();
    let mut payload: Payload = serde_json::from_str(&envelope.payload).unwrap();
    payload.entries[0].portable = false;
    envelope.payload = serde_json::to_string(&payload).unwrap();
    std::fs::write(&index, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert_eq!(c.mode(target.profile_id).unwrap_err(), VaultError::Locked);
    std::fs::write(&index, &original).unwrap();
    assert!(c.mode(target.profile_id).unwrap());
    let (_other_root, other, _) = fixture();
    let other_path = other.guard().path.clone();
    std::fs::copy(sidecar(&other_path, ".routes"), &index).unwrap();
    std::fs::copy(
        sidecar(&other_path, ".routes.pub"),
        sidecar(&path, ".routes.pub"),
    )
    .unwrap();
    assert_eq!(c.mode(ProfileId::new()).unwrap_err(), VaultError::Locked);
}

#[test]
fn missing_or_changed_vault_is_not_treated_as_native_even_with_a_signed_native_route() {
    let (root, c, _) = fixture();
    let native = ProfileId::new();
    let path = c.guard().path.clone();
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"damaged").unwrap();
    assert!(c.mode(native).is_err());
    std::fs::write(&path, &original).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(c.mode(native).is_err());
    drop(c);
    // Even if the old activation marker is also lost, the routing evidence
    // prevents a previously initialized library becoming a fresh native store.
    std::fs::remove_file(activation_path(&path)).unwrap();
    let reopened = VaultController::new(root.path().join("master.vault"));
    assert!(reopened.enabled());
    assert!(reopened.mode(native).is_err());
}

#[test]
fn legacy_or_interrupted_index_requires_authenticated_unlock_then_repairs() {
    let (_root, c, target) = fixture();
    let path = c.guard().path.clone();
    std::fs::remove_file(sidecar(&path, ".routes")).unwrap();
    std::fs::remove_file(sidecar(&path, ".routes.pub")).unwrap();
    assert_eq!(c.mode(ProfileId::new()).unwrap_err(), VaultError::Locked);
    c.unlock(&password(), c.response(None).lock_generation)
        .unwrap();
    assert!(sidecar(&path, ".routes").exists());
    c.set_mode(target.profile_id, c.response(None).revision, false)
        .unwrap();
    // Simulate an interrupted process before it could flush the changed mode.
    if let Backend::Portable(vault) = &mut c.guard().backend {
        vault.lock();
    }
    assert_eq!(c.mode(target.profile_id).unwrap_err(), VaultError::Locked);
    c.unlock(&password(), c.response(None).lock_generation)
        .unwrap();
    c.lock();
    assert!(!c.mode(target.profile_id).unwrap());
}

#[test]
fn password_rotation_rebinds_index_without_changing_routes_or_record_count() {
    let (_root, c, target) = fixture();
    c.unlock(&password(), c.response(None).lock_generation)
        .unwrap();
    let before = c.response(None).revision;
    c.rotate(
        before,
        &Secret::new(b"new-routing-master".to_vec()),
        c.response(None).lock_generation,
    )
    .unwrap();
    c.lock();
    assert!(c.mode(target.profile_id).unwrap());
    assert!(!c.mode(ProfileId::new()).unwrap());
    assert_eq!(
        c.read(&target, Kind::Password).unwrap_err(),
        VaultError::Locked
    );
}

#[test]
fn full_legacy_vault_does_not_need_an_extra_record_for_routing_keys() {
    let root = tempfile::tempdir().unwrap();
    let c = VaultController::new(root.path().join("master.vault"));
    c.create(&password(), 0).unwrap();
    let records: Vec<_> = (1..=128)
        .map(|i| {
            let mut profile_id = [0xff; 16];
            profile_id[0] = i;
            (
                VaultCredentialRef {
                    profile_id,
                    kind: Kind::ProfileMode,
                },
                Secret::new(vec![u8::from(i % 2 == 0)]),
            )
        })
        .collect();
    {
        let mut state = c.guard();
        portable(&mut state)
            .unwrap()
            .write_batch(
                &records
                    .iter()
                    .map(|(id, value)| (*id, MODE_BINDING, value))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
    }
    c.lock();
    for (reference, value) in &records {
        assert_eq!(
            c.mode(ProfileId::from_bytes(reference.profile_id)).unwrap(),
            value.expose() == [1]
        );
    }
}
