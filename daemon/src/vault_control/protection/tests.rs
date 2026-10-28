#![allow(clippy::unwrap_used)]
use super::*;
mod release_performance;
use std::cell::Cell;
fn password() -> Secret {
    Secret::new(b"original-test-password".to_vec())
}
fn system_key() -> Secret {
    Secret::new(vec![0x55; 32])
}
fn generation(c: &VaultController) -> u64 {
    c.response(None).lock_generation
}
fn fixture() -> (tempfile::TempDir, Arc<VaultController>, SshConnectionRecord) {
    let root = tempfile::tempdir().unwrap();
    let controller = VaultController::new(root.path().join("master.vault"));
    let target = crate::vault_control::tests::target();
    controller
        .create(&password(), generation(&controller))
        .unwrap();
    controller.set_mode(target.profile_id, 0, true).unwrap();
    controller
        .edit(
            &target,
            Kind::Password,
            Some(&Secret::new(b"saved-password".to_vec())),
        )
        .unwrap();
    (root, controller, target)
}
#[test]
fn protection_round_trip_preserves_credentials_references_and_restart_unlock() {
    let (root, c, target) = fixture();
    let original = std::fs::read(root.path().join("master.vault")).unwrap();
    let removed = Cell::new(0);
    c.protect_with(
        c.response(None).revision,
        generation(&c),
        &password(),
        true,
        |_| Ok(system_key()),
        |_| panic!(),
        rebuild::publish,
        |_| {
            removed.set(removed.get() + 1);
            Ok(())
        },
    )
    .unwrap();
    assert!(c.response(None).system);
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
    assert_eq!(
        std::fs::read(root.path().join("master.vault")).unwrap(),
        original
    );
    drop(c);
    let c = VaultController::new(root.path().join("master.vault"));
    assert!(!c.response(None).unlocked);
    c.unlock_system_with(generation(&c), |_| Ok(system_key()))
        .unwrap();
    let new = Secret::new(b"replacement-test-password".to_vec());
    c.protect_with(
        c.response(None).revision,
        generation(&c),
        &new,
        false,
        |_| panic!(),
        |_| Ok(system_key()),
        rebuild::publish,
        |_| {
            removed.set(removed.get() + 1);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(removed.get(), 1);
    assert!(!c.response(None).system);
    assert!(!c.response(None).protection_cleanup_pending);
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
    drop(c);
    let c = VaultController::new(root.path().join("master.vault"));
    assert!(c.unlock(&password(), generation(&c)).is_err());
    c.unlock(&new, generation(&c)).unwrap();
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
}
#[test]
fn wrong_password_or_stale_generation_never_creates_system_material() {
    let (_root, c, target) = fixture();
    let before = std::fs::read(&c.guard().path).unwrap();
    let called = Cell::new(0);
    for (secret, expected_generation) in [
        (Secret::new(b"incorrect-password".to_vec()), generation(&c)),
        (password(), generation(&c) + 99),
    ] {
        assert!(
            c.protect_with(
                c.response(None).revision,
                expected_generation,
                &secret,
                true,
                |_| {
                    called.set(1);
                    Ok(system_key())
                },
                |_| panic!(),
                rebuild::publish,
                |_| Ok(())
            )
            .is_err()
        );
    }
    assert_eq!(called.get(), 0);
    assert_eq!(std::fs::read(&c.guard().path).unwrap(), before);
    assert!(c.response(None).unlocked);
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
}
#[test]
fn failed_publication_keeps_old_library_and_retires_prepared_system_key() {
    let (_root, c, target) = fixture();
    let removed = Cell::new(0);
    let before = c.guard().path.clone();
    assert!(
        c.protect_with(
            c.response(None).revision,
            generation(&c),
            &password(),
            true,
            |_| Ok(system_key()),
            |_| panic!(),
            |_, _| Err(VaultError::Io(std::io::ErrorKind::PermissionDenied)),
            |_| {
                removed.set(removed.get() + 1);
                Ok(())
            }
        )
        .is_err()
    );
    assert_eq!(removed.get(), 1);
    assert_eq!(c.guard().path, before);
    assert!(!c.response(None).system);
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
}
#[test]
fn uncertain_publication_reloads_actual_generation_without_replaying_or_deleting_its_key() {
    let (_root, c, target) = fixture();
    let removed = Cell::new(0);
    assert_eq!(
        c.protect_with(
            c.response(None).revision,
            generation(&c),
            &password(),
            true,
            |_| Ok(system_key()),
            |_| panic!(),
            |base, dir| {
                rebuild::publish(base, dir)?;
                Err(VaultError::WriteUncertain)
            },
            |_| {
                removed.set(1);
                Ok(())
            }
        )
        .unwrap_err(),
        VaultError::WriteUncertain
    );
    assert_eq!(removed.get(), 0);
    assert!(!c.response(None).unlocked);
    c.reload().unwrap();
    assert!(c.response(None).system);
    c.unlock_system_with(generation(&c), |_| Ok(system_key()))
        .unwrap();
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
}
async fn prompt<T>(
    c: &VaultController,
    work: &tokio::task::JoinHandle<T>,
) -> cshell_ipc::KeyboardAuthChallenge {
    let started = Instant::now();
    // A wrong answer performs Argon2 before publishing the retry prompt. Use
    // the real connection budget under parallel KDF load, but fail immediately
    // if preparation finishes without publishing the expected challenge.
    tokio::time::timeout(CONNECTION_PREPARATION_TIMEOUT + Duration::from_secs(1), async {
        loop {
            let r = c.keyboard_auth.handle(
                c,
                cshell_ipc::VaultOperation::KeyboardAuthPoll,
                cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthPoll),
            );
            if let Some(p) = r.auth_challenge {
                eprintln!(
                    "vault prompt ready after {} ms",
                    started.elapsed().as_millis()
                );
                return p;
            }
            assert!(
                !work.is_finished(),
                "connection preparation finished before the expected prompt"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}
fn answer(c: &VaultController, id: Vec<u8>, password: &str, cancel: bool) {
    let mut request =
        cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthRespond);
    request.auth_answer = Some(cshell_ipc::KeyboardAuthAnswer {
        id,
        answers: if cancel {
            vec![]
        } else {
            vec![password.into()]
        },
        cancel,
        username: String::new(),
        save_credentials: false,
    });
    assert_eq!(
        c.keyboard_auth
            .handle(c, cshell_ipc::VaultOperation::KeyboardAuthRespond, request)
            .status,
        VaultControlStatus::Ok as i32
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn master_password_prompt_retries_locally_and_reuses_unlock_without_server_authentication() {
    let (_root, c, _) = fixture();
    c.lock();
    let _ = c.keyboard_auth.handle(
        &c,
        cshell_ipc::VaultOperation::KeyboardAuthPoll,
        cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthPoll),
    );
    let owner = c.clone();
    let work = tokio::spawn(async move { owner.prepare_connection("test-server:22".into()).await });
    let first = prompt(&c, &work).await;
    assert!(first.vault_unlock);
    assert!(!first.login);
    answer(&c, first.id.clone(), "incorrect-password", false);
    let second = prompt(&c, &work).await;
    assert_ne!(first.id, second.id);
    assert!(second.name.contains("不正确"));
    answer(&c, second.id, "original-test-password", false);
    work.await.unwrap().unwrap();
    let epoch = generation(&c);
    c.prepare_connection("another-server:22".into())
        .await
        .unwrap();
    assert_eq!(generation(&c), epoch);
    assert!(c.response(None).unlocked);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_master_unlock_leaves_library_locked_and_does_not_change_mode() {
    let (_root, c, _) = fixture();
    c.lock();
    let _ = c.keyboard_auth.handle(
        &c,
        cshell_ipc::VaultOperation::KeyboardAuthPoll,
        cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthPoll),
    );
    let owner = c.clone();
    let work = tokio::spawn(async move { owner.prepare_connection("test-server".into()).await });
    let p = prompt(&c, &work).await;
    answer(&c, p.id, "", true);
    assert!(matches!(
        work.await.unwrap(),
        Err(cshell_ssh::SshError::AuthenticationCancelled)
    ));
    assert!(!c.response(None).unlocked);
    assert!(!c.response(None).system);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
#[ignore = "explicit native Windows credential-manager fixture"]
async fn native_system_auto_unlock_manual_lock_and_lossless_switch_use_isolated_roots() {
    let (_root, c, target) = fixture();
    c.protect(c.response(None).revision, generation(&c), &password(), true)
        .unwrap();
    let root = c.guard().root.unwrap();
    struct Cleanup(VaultRootRef);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = SystemVaultRootKeys::delete(self.0);
        }
    }
    let _cleanup = Cleanup(root);
    c.lock();
    assert!(!c.response(None).unlocked);
    c.prepare_connection("native-test:22".into()).await.unwrap();
    assert!(c.response(None).unlocked);
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
    c.lock_manually();
    let _ = c.keyboard_auth.handle(
        &c,
        cshell_ipc::VaultOperation::KeyboardAuthPoll,
        cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthPoll),
    );
    let owner = c.clone();
    let work = tokio::spawn(async move { owner.prepare_connection("native-test:22".into()).await });
    let p = prompt(&c, &work).await;
    assert!(p.vault_unlock && p.prompts.is_empty());
    answer(&c, p.id, "", true);
    assert!(work.await.unwrap().is_err());
    assert!(!c.response(None).unlocked);
    c.unlock_system(generation(&c)).unwrap();
    let new = Secret::new(b"native-new-test-password".to_vec());
    c.protect_with(
        c.response(None).revision,
        generation(&c),
        &new,
        false,
        SystemVaultRootKeys::create,
        SystemVaultRootKeys::read,
        rebuild::publish,
        |_| Err(cshell_vault::KeychainError::Unavailable),
    )
    .unwrap_err();
    assert!(!c.response(None).system);
    assert!(c.response(None).protection_cleanup_pending);
    c.finish_protection_cleanup().unwrap();
    assert!(!c.response(None).protection_cleanup_pending);
    assert!(SystemVaultRootKeys::read(root).is_err());
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
}

#[test]
fn system_key_creation_refusal_never_deletes_an_existing_key() {
    let (_root, c, target) = fixture();
    let removed = Cell::new(false);
    assert!(
        c.protect_with(
            c.response(None).revision,
            generation(&c),
            &password(),
            true,
            |_| Err(cshell_vault::KeychainError::AlreadyExists),
            |_| panic!(),
            rebuild::publish,
            |_| {
                removed.set(true);
                Ok(())
            }
        )
        .is_err()
    );
    assert!(!removed.get());
    assert!(!c.response(None).system);
    assert_eq!(
        c.read(&target, Kind::Password).unwrap().expose(),
        b"saved-password"
    );
}
#[test]
fn unlock_survives_long_idle_manual_lock_still_cancels_and_restart_starts_locked() {
    let (root, c, target) = fixture();
    c.guard().unlocked_at = Some(Instant::now() - Duration::from_secs(24 * 60 * 60));
    let before = generation(&c);
    let cancellation = c.cancellation();
    for _ in 0..3 {
        assert!(c.response(None).unlocked);
        assert_eq!(c.read(&target, Kind::Password).unwrap().expose(), b"saved-password");
    }
    assert!(!cancellation.has_changed().unwrap());
    assert_eq!(c.commit_auth(before, || "connected").unwrap(), "connected");
    c.lock_manually();
    assert!(c.response(None).manually_locked);
    assert!(!c.response(None).unlocked);
    assert!(cancellation.has_changed().unwrap());
    assert_eq!(c.read(&target, Kind::Password).unwrap_err(), VaultError::Locked);
    assert_eq!(c.commit_auth(before, || ()).unwrap_err(), VaultError::Locked);
    c.unlock(&password(), generation(&c)).unwrap();
    assert!(!c.response(None).manually_locked);
    drop(cancellation);
    drop(c);
    let reopened = VaultController::new(root.path().join("master.vault"));
    assert!(!reopened.response(None).unlocked);
    reopened.unlock(&password(), generation(&reopened)).unwrap();
    assert_eq!(reopened.read(&target, Kind::Password).unwrap().expose(), b"saved-password");
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[test]
fn desktop_lock_suspend_and_monitor_failure_do_not_revoke_unlocked_credentials() {
    let (_root, c, target) = fixture();
    let before = generation(&c);
    let cancellation = c.cancellation();
    for state in [VaultSessionState::Active, VaultSessionState::Locked,
        VaultSessionState::Disconnected, VaultSessionState::Suspended, VaultSessionState::Unavailable,
        VaultSessionState::Unsupported, VaultSessionState::Active] {
        c.observe_desktop_session(state);
        assert_eq!(c.session_state(), state);
        assert!(c.response(None).unlocked);
        assert_eq!(c.read(&target, Kind::Password).unwrap().expose(), b"saved-password");
        assert_eq!(generation(&c), before);
        assert!(!cancellation.has_changed().unwrap());
        assert_eq!(c.commit_auth(before, || "connected").unwrap(), "connected");
    }
    c.protect_with(c.response(None).revision, generation(&c), &password(), true,
        |_| Ok(system_key()), |_| Ok(system_key()), rebuild::publish, |_| Ok(())).unwrap();
    let system_generation = generation(&c);
    c.guard().unlocked_at = Some(Instant::now() - Duration::from_secs(24 * 60 * 60));
    for state in [VaultSessionState::Locked, VaultSessionState::Suspended, VaultSessionState::Unavailable] {
        c.observe_desktop_session(state);
        assert!(c.response(None).system && c.response(None).unlocked);
        assert_eq!(c.read(&target, Kind::Password).unwrap().expose(), b"saved-password");
        assert_eq!(generation(&c), system_generation);
    }
    c.lock_manually();
    c.observe_desktop_session(VaultSessionState::Active);
    assert!(!c.response(None).unlocked, "unlocking the desktop must not undo a manual Vault lock");
    assert!(c.response(None).manually_locked);
}

#[tokio::test]
async fn protection_api_rejects_extra_or_stale_fields_and_distinguishes_manual_from_transient_lock()
{
    use cshell_ipc::{VaultOperation as Op, VaultRequest};
    let root = tempfile::tempdir().unwrap();
    let repo = cshell_storage::SqliteProfileRepository::open(root.path().join("profiles.db"))
        .await
        .unwrap();
    let service = crate::ProfileIpcService::new(repo);
    let locked = service.handle_vault(VaultRequest::new(Op::Lock)).await;
    assert!(locked.manually_locked);
    let transient = service
        .handle_vault(VaultRequest::new(Op::LockTransient))
        .await;
    assert!(
        transient.manually_locked,
        "internal locking must not clear an existing user lock"
    );
    let mut create = VaultRequest::new(Op::Create);
    create.secret = password().expose().to_vec();
    create.expected_lock_generation = transient.lock_generation;
    let created = service.handle_vault(create).await;
    assert_eq!(created.status, VaultControlStatus::Ok as i32);
    assert!(!created.manually_locked);
    let mut bad = VaultRequest::new(Op::ProtectWithSystem);
    bad.secret = password().expose().to_vec();
    bad.profile_id = vec![1; 16];
    bad.expected_lock_generation = created.lock_generation;
    let refused = service.handle_vault(bad).await;
    assert_eq!(refused.status, VaultControlStatus::Invalid as i32);
    let mut stale = VaultRequest::new(Op::ProtectWithSystem);
    stale.secret = password().expose().to_vec();
    stale.expected_vault_revision = created.revision + 1;
    stale.expected_lock_generation = created.lock_generation;
    let refused = service.handle_vault(stale).await;
    assert_eq!(refused.status, VaultControlStatus::Conflict as i32);
    assert!(!service.vault.response(None).system);
    let transient = service
        .handle_vault(VaultRequest::new(Op::LockTransient))
        .await;
    assert!(!transient.unlocked);
    assert!(!transient.manually_locked);
}
