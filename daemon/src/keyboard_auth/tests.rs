use super::*;
fn target() -> SshConnectionRecord {
    SshConnectionRecord {
        profile_id: cshell_domain::ProfileId::new(),
        host: "mfa.test".into(),
        port: 22,
        username: "operator".into(),
        auth_method: Default::default(),
        private_key_path: None,
        certificate_path: None,
        agent_backend: Default::default(),
        agent_identity: None,
        route: Default::default(),
        options: Default::default(),
    }
}
fn challenge(label: &str) -> KeyboardInteractiveChallenge {
    KeyboardInteractiveChallenge {
        name: "MFA".into(),
        instructions: String::new(),
        prompts: vec![cshell_ssh::KeyboardInteractivePrompt {
            text: label.into(),
            echo: false,
        }],
    }
}
fn poll(vault: &VaultController) -> Option<KeyboardAuthChallenge> {
    vault
        .keyboard_auth
        .handle(
            vault,
            Op::KeyboardAuthPoll,
            VaultRequest::new(Op::KeyboardAuthPoll),
        )
        .auth_challenge
}
async fn pending(vault: &VaultController) -> KeyboardAuthChallenge {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(c) = poll(vault) {
                return c;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn reply(
    vault: &VaultController,
    c: &KeyboardAuthChallenge,
    text: &str,
    cancel: bool,
) -> VaultResponse {
    let mut request = VaultRequest::new(Op::KeyboardAuthRespond);
    request.auth_answer = Some(cshell_ipc::KeyboardAuthAnswer {
        id: c.id.clone(),
        answers: if cancel { vec![] } else { vec![text.into()] },
        cancel,
        username:String::new(),save_credentials:false
    });
    vault
        .keyboard_auth
        .handle(vault, Op::KeyboardAuthRespond, request)
}
#[test]
fn password_allowlist_never_fills_otp_passcode_pin_or_echoed_prompts() {
    for label in [
        "Verification code:",
        "One-time password:",
        "OTP:",
        "Passcode:",
        "PIN:",
        "验证码：",
        "Password or OTP:",
        "New password:",
    ] {
        assert!(!password_prompt(label, false), "{label}");
    }
    assert!(password_prompt("Password: ", false));
    assert!(password_prompt("operator's password:", false));
    assert!(!password_prompt("Password:", true));
}
#[tokio::test]
async fn otp_round_is_manual_then_saved_password_is_used_once() {
    let root = tempfile::tempdir().unwrap();
    let vault = VaultController::new(root.path().join("vault"));
    let SshAuthentication::KeyboardInteractive { mut responder } = authentication(
        vault.clone(),
        &target(),
        Some(Zeroizing::new("saved-password".into())),
    ) else {
        panic!()
    };
    let first = responder(challenge("Verification code: "));
    let job = tokio::spawn(first);
    let prompt = pending(&vault).await;
    assert_eq!(prompt.prompts[0].text, "Verification code: ");
    assert_eq!(
        reply(&vault, &prompt, "654321", false).status,
        Status::Ok as i32
    );
    assert_eq!(*job.await.unwrap().unwrap(), vec!["654321"]);
    assert_eq!(
        responder(challenge("Password: ")).await.unwrap(),
        vec!["saved-password"]
    );
    assert!(poll(&vault).is_none());
    let next = tokio::spawn(responder(challenge("Password: ")));
    let prompt = pending(&vault).await;
    assert_eq!(reply(&vault, &prompt, "", true).status, Status::Ok as i32);
    assert!(matches!(
        next.await.unwrap(),
        Err(SshError::AuthenticationCancelled)
    ));
}
#[tokio::test]
async fn cancel_drop_and_stale_answers_do_not_reuse_a_challenge() {
    let root = tempfile::tempdir().unwrap();
    let vault = VaultController::new(root.path().join("vault"));
    let SshAuthentication::KeyboardInteractive { mut responder } =
        authentication(vault.clone(), &target(), None)
    else {
        panic!()
    };
    let job = tokio::spawn(responder(challenge("OTP:")));
    let c = pending(&vault).await;
    assert_eq!(reply(&vault, &c, "", true).status, Status::Ok as i32);
    assert!(matches!(
        job.await.unwrap(),
        Err(SshError::AuthenticationCancelled)
    ));
    assert!(poll(&vault).is_none());
    assert_eq!(
        reply(&vault, &c, "123456", false).status,
        Status::Conflict as i32
    );
    let job = tokio::spawn(responder(challenge("OTP:")));
    let c = pending(&vault).await;
    job.abort();
    let _ = job.await;
    assert!(poll(&vault).is_none());
    assert_eq!(
        reply(&vault, &c, "123456", false).status,
        Status::Conflict as i32
    );
}

#[tokio::test]
async fn mixed_prompts_fill_only_password_and_vault_invalidation_cancels_waiting_round() {
    let root = tempfile::tempdir().unwrap();
    let vault = VaultController::new(root.path().join("vault"));
    let SshAuthentication::KeyboardInteractive { mut responder } = authentication(
        vault.clone(),
        &target(),
        Some(Zeroizing::new("saved".into())),
    ) else {
        panic!()
    };
    let mut mixed = challenge("Verification code:");
    mixed.prompts.push(cshell_ssh::KeyboardInteractivePrompt {
        text: "Password:".into(),
        echo: false,
    });
    let job = tokio::spawn(responder(mixed));
    let c = pending(&vault).await;
    assert_eq!(c.prompts.len(), 1);
    reply(&vault, &c, "654321", false);
    assert_eq!(job.await.unwrap().unwrap(), vec!["654321", "saved"]);
    let job = tokio::spawn(responder(challenge("OTP:")));
    let _ = pending(&vault).await;
    vault.invalidate();
    assert!(matches!(
        job.await.unwrap(),
        Err(SshError::AuthenticationCancelled)
    ));
    assert!(poll(&vault).is_none());
}

#[tokio::test]
async fn closing_sftp_cancels_only_its_challenge_and_fences_late_challenges() {
    let root = tempfile::tempdir().unwrap();
    let vault = VaultController::new(root.path().join("vault"));
    let a = SessionId::new();
    let b = SessionId::new();
    let SshAuthentication::KeyboardInteractive {
        responder: mut first,
    } = for_sftp(a, async { authentication(vault.clone(), &target(), None) }).await
    else {
        panic!()
    };
    let SshAuthentication::KeyboardInteractive {
        responder: mut second,
    } = for_sftp(b, async { authentication(vault.clone(), &target(), None) }).await
    else {
        panic!()
    };
    let a_job = tokio::spawn(first(challenge("OTP:")));
    let a_prompt = pending(&vault).await;
    let b_job = tokio::spawn(second(challenge("OTP:")));
    tokio::task::yield_now().await;
    vault.keyboard_auth.cancel_sftp(a);
    assert!(matches!(
        a_job.await.unwrap(),
        Err(SshError::AuthenticationCancelled)
    ));
    let b_prompt = pending(&vault).await;
    assert_ne!(a_prompt.id, b_prompt.id);
    assert_eq!(
        reply(&vault, &b_prompt, "654321", false).status,
        Status::Ok as i32
    );
    assert_eq!(b_job.await.unwrap().unwrap(), vec!["654321"]);
    assert!(matches!(
        first(challenge("OTP:")).await,
        Err(SshError::AuthenticationCancelled)
    ));
    assert!(poll(&vault).is_none());
}

mod connection_tests;
