#![allow(clippy::unwrap_used)]
use super::*;
fn old_password() -> Secret {
    Secret::new(b"old-master-password".to_vec())
}
fn new_password() -> Secret {
    Secret::new(b"new-master-password".to_vec())
}
fn fixture() -> (tempfile::TempDir, Arc<VaultController>) {
    let directory = tempfile::tempdir().unwrap();
    let controller = VaultController::new(directory.path().join("master.vault"));
    controller.create(&old_password(), 0).unwrap();
    let target = super::super::tests::target();
    controller.set_mode(target.profile_id, 0, true).unwrap();
    controller
        .edit(
            &target,
            Kind::Password,
            Some(&Secret::new(b"old-server-secret".to_vec())),
        )
        .unwrap();
    controller.lock();
    (directory, controller)
}
#[test]
fn rebuild_without_old_password_preserves_old_bytes_and_reopens_new_library() {
    let (directory, controller) = fixture();
    let base = directory.path().join("master.vault");
    let old_bytes = std::fs::read(&base).unwrap();
    let before = controller.response(None);
    controller
        .rebuild(
            before.revision,
            before.lock_generation,
            &new_password(),
            false,
            true,
        )
        .unwrap();
    assert_eq!(old_bytes, std::fs::read(&base).unwrap());
    let response = controller.response(None);
    assert!(response.unlocked && !response.system);
    assert!(response.managed_keys.is_empty());
    let unknown_profile = ProfileId::new();
    assert!(
        controller.mode(unknown_profile).unwrap(),
        "must not fall back to old native credentials"
    );
    let active = active_path(&base).unwrap();
    assert_ne!(active, base);
    drop(controller);
    let controller = VaultController::new(base.clone());
    assert!(!controller.response(None).unlocked);
    assert!(
        controller
            .unlock(&old_password(), controller.response(None).lock_generation)
            .is_err()
    );
    controller
        .unlock(&new_password(), controller.response(None).lock_generation)
        .unwrap();
    assert!(controller.mode(unknown_profile).unwrap());
    let revision = controller.response(None).revision;
    controller
        .set_mode(ProfileId::from_bytes([0xff; 16]), revision, false)
        .unwrap();
    assert!(
        controller.mode(unknown_profile).unwrap(),
        "profile identity cannot overwrite global default settings"
    );
    let mut previous = PortableVault::open(&base).unwrap();
    previous.unlock(&old_password()).unwrap();
    assert_eq!(previous.status().record_count, 2);
    let duplicate = VaultController::new(base);
    assert_eq!(
        duplicate.response(None).status,
        VaultControlStatus::Busy as i32
    );
    assert_eq!(duplicate.reload().unwrap_err(), VaultError::Busy);
}
#[test]
fn lost_system_root_can_rebuild_to_password_and_keeps_previous_root_reference() {
    let (directory, controller) = fixture();
    let base = directory.path().join("master.vault");
    let id = VaultRootRef::new();
    persist_root(&base, id).unwrap();
    controller.guard().root = Some(id);
    let generation = controller.response(None).lock_generation;
    assert_eq!(
        controller
            .unlock_system_with(generation, |_| Err(cshell_vault::KeychainError::Missing))
            .unwrap_err(),
        VaultError::RootKeyMissing
    );
    let generation = controller.response(None).lock_generation;
    assert_eq!(
        controller
            .unlock_system_with(generation, |_| Err(
                cshell_vault::KeychainError::AccessDenied
            ))
            .unwrap_err(),
        VaultError::RootKeyUnavailable
    );
    let response = controller.response(None);
    controller
        .rebuild(
            response.revision,
            response.lock_generation,
            &new_password(),
            false,
            true,
        )
        .unwrap();
    assert!(!controller.response(None).system);
    assert_eq!(read_root(&base).unwrap().unwrap().bytes(), id.bytes());
    assert!(!root_path(&active_path(&base).unwrap()).exists());
}
#[test]
fn failed_publication_preserves_previous_library_and_does_not_replay() {
    let (directory, controller) = fixture();
    let base = directory.path().join("master.vault");
    let old_bytes = std::fs::read(&base).unwrap();
    let response = controller.response(None);
    assert_eq!(
        controller
            .rebuild_with(
                response.revision,
                response.lock_generation,
                &new_password(),
                false,
                true,
                |_, _| Err(VaultError::Io(std::io::ErrorKind::PermissionDenied))
            )
            .unwrap_err(),
        VaultError::Io(std::io::ErrorKind::PermissionDenied)
    );
    assert_eq!(active_path(&base).unwrap(), base);
    assert_eq!(std::fs::read(&base).unwrap(), old_bytes);
    assert!(!controller.response(None).unlocked);
    controller
        .unlock(&old_password(), controller.response(None).lock_generation)
        .unwrap();
}
#[test]
fn uncertain_published_generation_reloads_locked_without_losing_either_library() {
    let (directory, controller) = fixture();
    let base = directory.path().join("master.vault");
    let bytes = std::fs::read(&base).unwrap();
    let response = controller.response(None);
    assert_eq!(
        controller
            .rebuild_with(
                response.revision,
                response.lock_generation,
                &new_password(),
                false,
                true,
                |base, directory| {
                    publish(base, directory)?;
                    Err(VaultError::WriteUncertain)
                }
            )
            .unwrap_err(),
        VaultError::WriteUncertain
    );
    assert_eq!(
        controller.response(None).status,
        VaultControlStatus::ReloadRequired as i32
    );
    assert_eq!(std::fs::read(base).unwrap(), bytes);
    controller.reload().unwrap();
    assert!(!controller.response(None).unlocked);
    controller
        .unlock(&new_password(), controller.response(None).lock_generation)
        .unwrap();
}
#[test]
fn reset_requires_confirmation_current_revision_generation_and_locked_state() {
    let (directory, controller) = fixture();
    let response = controller.response(None);
    assert_eq!(
        controller
            .rebuild(
                response.revision,
                response.lock_generation,
                &new_password(),
                false,
                false
            )
            .unwrap_err(),
        VaultError::InvalidInput
    );
    assert!(
        controller
            .rebuild(
                response.revision + 1,
                response.lock_generation,
                &new_password(),
                false,
                true
            )
            .is_err()
    );
    controller.lock();
    assert!(
        controller
            .rebuild(
                response.revision,
                response.lock_generation,
                &new_password(),
                false,
                true
            )
            .is_err()
    );
    controller
        .unlock(&old_password(), controller.response(None).lock_generation)
        .unwrap();
    let response = controller.response(None);
    assert_eq!(
        controller
            .rebuild(
                response.revision,
                response.lock_generation,
                &new_password(),
                false,
                true
            )
            .unwrap_err(),
        VaultError::InvalidInput
    );
    assert!(!pointer(&directory.path().join("master.vault")).exists());
}
#[test]
fn malicious_active_pointer_cannot_escape_the_credentials_directory() {
    let (directory, controller) = fixture();
    let base = directory.path().join("master.vault");
    std::fs::write(pointer(&base), b"CSHVACT1\n../outside").unwrap();
    assert_eq!(active_path(&base).unwrap_err(), VaultError::Corrupt);
    drop(controller);
    let controller = VaultController::new(base.clone());
    assert_eq!(
        controller.response(None).status,
        VaultControlStatus::Unavailable as i32
    );
    let response = controller.response(None);
    controller
        .rebuild(
            response.revision,
            response.lock_generation,
            &new_password(),
            false,
            true,
        )
        .unwrap();
    assert!(active_path(&base).unwrap().starts_with(directory.path()));
}

#[test]
fn missing_active_pointer_never_reactivates_retained_credentials() {
    let (directory, controller) = fixture();
    let base = directory.path().join("master.vault");
    let response = controller.response(None);
    controller
        .rebuild(
            response.revision,
            response.lock_generation,
            &new_password(),
            false,
            true,
        )
        .unwrap();
    let active = active_path(&base).unwrap();
    assert!(active.exists() && base.exists());
    std::fs::remove_file(pointer(&base)).unwrap();
    assert_eq!(active_path(&base).unwrap_err(), VaultError::ReloadRequired);
    drop(controller);
    let controller = VaultController::new(base);
    assert_eq!(
        controller.response(None).status,
        VaultControlStatus::ReloadRequired as i32
    );
    assert!(
        controller
            .unlock(&old_password(), controller.response(None).lock_generation)
            .is_err()
    );
}
