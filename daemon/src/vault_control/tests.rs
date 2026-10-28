#![allow(clippy::unwrap_used)]
use super::*;
use cshell_domain::{SshAuthMethod, SshRoute};

fn password() -> Secret {
    Secret::new(b"master-password-for-test".to_vec())
}
pub(super) fn target() -> SshConnectionRecord {
    SshConnectionRecord {
        options: Default::default(),
        profile_id: ProfileId::new(),
        host: "example.test".into(),
        port: 22,
        username: "alice".into(),
        auth_method: SshAuthMethod::Password,
        private_key_path: None,
        certificate_path: None,
        agent_backend: Default::default(),
        agent_identity: None,
        route: SshRoute::Direct,
    }
}
fn generation(controller: &VaultController) -> u64 {
    controller.response(None).lock_generation
}

#[test]
fn native_system_root_vault_reopens_locked_and_unlocks_only_with_the_keychain() {
    if std::env::var_os("CSHELL_KEYCHAIN_NATIVE_TEST").is_none() {
        return;
    }
    struct Cleanup(VaultRootRef);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = SystemVaultRootKeys::delete(self.0);
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("system-vault");
    let controller = VaultController::new(path.clone());
    controller.create_system(generation(&controller)).unwrap();
    let cleanup = Cleanup(controller.guard().root.unwrap());
    assert!(controller.response(None).system);
    assert_eq!(
        controller
            .unlock(&password(), generation(&controller))
            .unwrap_err(),
        VaultError::InvalidInput
    );
    assert!(!controller.response(None).unlocked);
    controller.unlock_system(generation(&controller)).unwrap();
    let target = target();
    controller.set_mode(target.profile_id, 0, true).unwrap();
    controller
        .edit(
            &target,
            Kind::Password,
            Some(&Secret::new(b"system-secret".to_vec())),
        )
        .unwrap();
    drop(controller);
    let reopened = VaultController::new(path);
    assert!(reopened.response(None).system);
    assert!(!reopened.response(None).unlocked);
    assert_eq!(
        reopened
            .unlock(&password(), generation(&reopened))
            .unwrap_err(),
        VaultError::InvalidInput
    );
    reopened.unlock_system(generation(&reopened)).unwrap();
    assert_eq!(
        reopened.read(&target, Kind::Password).unwrap().expose(),
        b"system-secret"
    );
    SystemVaultRootKeys::delete(cleanup.0).unwrap();
    assert!(reopened.unlock_system(generation(&reopened)).is_err());
    assert!(!reopened.response(None).unlocked);
    assert_eq!(
        reopened.mode(target.profile_id).unwrap_err(),
        VaultError::Locked
    );
}

#[test]
fn mode_and_lock_gate_all_saved_authentication_without_native_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.vault");
    let controller = VaultController::new(path.clone());
    let mut target = target();
    assert!(!controller.enabled());
    assert!(!controller.mode(target.profile_id).unwrap());
    controller
        .create(&password(), generation(&controller))
        .unwrap();
    assert_eq!(
        controller
            .unlock_system(generation(&controller))
            .unwrap_err(),
        VaultError::InvalidInput
    );
    assert!(!controller.response(None).unlocked);
    controller
        .unlock(&password(), generation(&controller))
        .unwrap();
    controller.set_mode(target.profile_id, 0, true).unwrap();
    assert_eq!(
        controller.read(&target, Kind::Password).unwrap_err(),
        VaultError::Missing
    );
    controller
        .edit(
            &target,
            Kind::Password,
            Some(&Secret::new(b"saved".to_vec())),
        )
        .unwrap();
    assert_eq!(
        controller.read(&target, Kind::Password).unwrap().expose(),
        b"saved"
    );
    target.username = "other-account".into();
    assert_eq!(
        controller.read(&target, Kind::Password).unwrap_err(),
        VaultError::BindingMismatch
    );
    let previous = generation(&controller);
    controller.lock();
    assert_eq!(
        controller
            .commit_auth(previous, || "registered")
            .unwrap_err(),
        VaultError::Locked
    );
    assert_eq!(
        controller.mode(ProfileId::new()).unwrap_err(),
        VaultError::Locked
    );
    assert_eq!(
        controller.unlock(&password(), previous).unwrap_err(),
        VaultError::AlreadyExists
    );
    controller
        .unlock(&password(), generation(&controller))
        .unwrap();
    assert!(controller.mode(target.profile_id).unwrap());
    controller.guard().unlocked_at = Some(Instant::now() - Duration::from_secs(24 * 60 * 60));
    assert!(controller.response(None).unlocked);
    assert!(controller.mode(target.profile_id).unwrap());
    drop(controller);
    let reopened = VaultController::new(path.clone());
    assert!(reopened.enabled());
    assert!(!reopened.response(None).unlocked);
    reopened.unlock(&password(), generation(&reopened)).unwrap();
    assert!(reopened.mode(target.profile_id).unwrap());
    drop(reopened);
    std::fs::remove_file(path).unwrap();
    let missing = VaultController::new(directory.path().join("credentials.vault"));
    assert!(missing.enabled());
    assert!(missing.mode(target.profile_id).is_err());
}

#[test]
fn lock_during_argon2_and_failed_unlock_never_publish_usable_keys() {
    let directory = tempfile::tempdir().unwrap();
    let controller = VaultController::new(directory.path().join("vault"));
    controller
        .create(&password(), generation(&controller))
        .unwrap();
    let old = generation(&controller);
    let worker = controller.clone();
    let task = std::thread::spawn(move || worker.unlock(&password(), old));
    while controller.epoch.load(Ordering::Acquire) == old {
        std::thread::yield_now();
    }
    controller.lock();
    assert_eq!(task.join().unwrap().unwrap_err(), VaultError::Locked);
    assert!(!controller.response(None).unlocked);
    controller
        .unlock(&password(), generation(&controller))
        .unwrap();
    assert_eq!(
        controller
            .unlock(&Secret::new(b"incorrect".to_vec()), generation(&controller))
            .unwrap_err(),
        VaultError::AuthenticationFailed
    );
    assert!(!controller.response(None).unlocked);
}
