use super::*;
use crate::ssh_route::SshConnectionPlan;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_connection_does_not_prompt_or_unlock_an_existing_library() {
    let (_root, c, _) = fixture();
    c.lock_manually();
    let target = crate::vault_control::tests::target();
    let plan = SshConnectionPlan {
        title: String::new(),
        target,
        jumps: vec![],
    };
    tokio::time::timeout(Duration::from_secs(5), c.prepare_plan(&plan))
        .await
        .unwrap()
        .unwrap();
    assert!(!c.response(None).unlocked);
    let poll = c.keyboard_auth.handle(
        &c,
        cshell_ipc::VaultOperation::KeyboardAuthPoll,
        cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthPoll),
    );
    assert!(poll.auth_challenge.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vault_jump_requires_one_unlock_even_when_the_leaf_uses_native_credentials() {
    let (_root, c, jump) = fixture();
    c.lock();
    let _ = c.keyboard_auth.handle(
        &c,
        cshell_ipc::VaultOperation::KeyboardAuthPoll,
        cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthPoll),
    );
    let plan = SshConnectionPlan {
        title: String::new(),
        target: crate::vault_control::tests::target(),
        jumps: vec![jump],
    };
    let owner = c.clone();
    let work = tokio::spawn(async move { owner.prepare_plan(&plan).await });
    let p = prompt(&c, &work).await;
    assert!(p.vault_unlock);
    answer(&c, p.id, "original-test-password", false);
    work.await.unwrap().unwrap();
    assert!(c.response(None).unlocked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_only_connection_needs_no_library_secret_even_after_index_loss() {
    let (_root, c, mut target) = fixture();
    c.lock();
    std::fs::remove_file(c.guard().path.with_extension("vault.routes")).unwrap();
    target.auth_method = cshell_domain::SshAuthMethod::Agent;
    target.options.authentication_order.clear();
    let plan = SshConnectionPlan {
        title: String::new(),
        target,
        jumps: vec![],
    };
    c.prepare_plan(&plan).await.unwrap();
    assert!(matches!(
        crate::ssh_route::load_authentication(&plan.target, &c)
            .await
            .unwrap(),
        cshell_ssh::SshAuthentication::Agent { .. }
    ));
    assert!(!c.response(None).unlocked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_vault_does_not_silently_allow_a_native_password_connection() {
    let (_root, c, _) = fixture();
    c.lock();
    std::fs::remove_file(c.guard().path.clone()).unwrap();
    let plan = SshConnectionPlan {
        title: String::new(),
        target: crate::vault_control::tests::target(),
        jumps: vec![],
    };
    assert!(matches!(
        c.prepare_plan(&plan).await,
        Err(cshell_ssh::SshError::CredentialUnavailable(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_native_route_is_confirmed_once_then_skips_unlock_after_lock() {
    let (_root, c, _) = fixture();
    c.lock();
    std::fs::remove_file(c.guard().path.with_extension("vault.routes")).unwrap();
    let _ = c.keyboard_auth.handle(
        &c,
        cshell_ipc::VaultOperation::KeyboardAuthPoll,
        cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::KeyboardAuthPoll),
    );
    let target = crate::vault_control::tests::target();
    let plan = SshConnectionPlan {
        title: String::new(),
        target,
        jumps: vec![],
    };
    let owner = c.clone();
    let work_plan = plan.clone();
    let work = tokio::spawn(async move { owner.prepare_plan(&work_plan).await });
    let p = prompt(&c, &work).await;
    assert!(p.vault_unlock && p.instructions.contains("存储位置"));
    answer(&c, p.id, "original-test-password", false);
    work.await.unwrap().unwrap();
    c.lock_manually();
    c.prepare_plan(&plan).await.unwrap();
    assert!(!c.response(None).unlocked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit isolated native credential round-trip"]
async fn native_password_can_be_saved_and_loaded_while_master_vault_stays_locked() {
    use cshell_vault::{KeychainError, ProfilePasswordRef, SystemProfilePasswordVault};
    let (_root, c, _) = fixture();
    c.lock_manually();
    let mut target = crate::vault_control::tests::target();
    target.options.keyboard_interactive = false;
    let id = ProfilePasswordRef::from_profile_bytes(*target.profile_id.as_uuid().as_bytes());
    let binding = cshell_vault::ProfilePasswordBinding {
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
    };
    assert!(matches!(
        SystemProfilePasswordVault::new().read(&id, &binding),
        Err(KeychainError::Missing)
    ));
    struct Cleanup(ProfilePasswordRef);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = SystemProfilePasswordVault::new().delete(&self.0);
        }
    }
    let _cleanup = Cleanup(id);
    c.edit(
        &target,
        Kind::Password,
        Some(&Secret::new(b"native-routing-password".to_vec())),
    )
    .unwrap();
    let plan = SshConnectionPlan {
        title: String::new(),
        target,
        jumps: vec![],
    };
    c.prepare_plan(&plan).await.unwrap();
    let authentication = crate::ssh_route::load_single_authentication(
        &plan.target,
        &c,
        cshell_domain::SshAuthMethod::Password,
    )
    .await
    .unwrap();
    assert!(
        matches!(authentication, cshell_ssh::SshAuthentication::Password(ref value) if value == "native-routing-password")
    );
    assert!(!c.response(None).unlocked);
}
