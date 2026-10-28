use super::*;
use cshell_ipc::{
    Envelope, Handshake, HandshakePolicy, client_handshake, envelope, read_envelope,
    server_handshake, write_envelope,
};
use russh::server::{Auth, Response};
use std::borrow::Cow;
struct Mfa {
    stage: u8,
    switch: bool,
}
impl russh::server::Handler for Mfa {
    type Error = russh::Error;
    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(
            if self.stage == 1 && self.switch && password == "saved-password" {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if let Some(mut response) = response {
            let answer = response.next().unwrap_or_default();
            if self.stage == 0 {
                if answer.as_ref() != b"654321" {
                    return Ok(Auth::reject());
                }
                self.stage = 1;
                if self.switch {
                    return Ok(Auth::Reject {
                        proceed_with_methods: Some((&[russh::MethodKind::Password][..]).into()),
                        partial_success: true,
                    });
                }
            } else {
                return Ok(if answer.as_ref() == b"saved-password" {
                    Auth::Accept
                } else {
                    Auth::reject()
                });
            }
        }
        Ok(Auth::Partial {
            name: Cow::Borrowed("MFA"),
            instructions: Cow::Borrowed(""),
            prompts: Cow::Owned(vec![(
                Cow::Borrowed(if self.stage == 0 {
                    "Verification code: "
                } else {
                    "Password: "
                }),
                false,
            )]),
        })
    }
}
async fn ipc(service: &crate::SessionIpcService, request: VaultRequest) -> VaultResponse {
    let (mut client, mut server) = tokio::io::duplex(65536);
    let service = service.clone();
    let job = tokio::spawn(async move {
        server_handshake(
            &mut server,
            &HandshakePolicy::with_instance_id([7; 32], [8; 16], u64::MAX),
        )
        .await
        .unwrap();
        service.serve_one(&mut server).await.unwrap();
    });
    let mut hello = Handshake::new(vec![8; 16], vec![7; 32]);
    hello.feature_bits = cshell_ipc::features::VAULT_CONTROL;
    client_handshake(&mut client, 1, hello).await.unwrap();
    write_envelope(
        &mut client,
        &Envelope {
            request_id: 2,
            payload: Some(envelope::Payload::VaultRequest(request)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let reply = read_envelope(&mut client).await.unwrap();
    job.await.unwrap();
    let Some(envelope::Payload::VaultResponse(response)) = reply.payload else {
        panic!()
    };
    response
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyboard_saved_vault_password_after_code_authenticates_through_real_ssh_and_authenticated_ipc()
 {
    for (switch, saved) in [(false, true), (true, true), (false, false)] {
        let root = tempfile::tempdir().unwrap();
        let hostkey = russh::keys::ssh_key::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let public = hostkey.public_key().to_openssh().unwrap();
        let config = russh::server::Config {
            keys: vec![hostkey],
            auth_rejection_time: Duration::from_millis(1),
            ..Default::default()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let handle =
                russh::server::run_stream(Arc::new(config), stream, Mfa { stage: 0, switch })
                    .await
                    .unwrap();
            let _ = handle.await;
        });
        let mut target = target();
        target.host = "127.0.0.1".into();
        target.port = address.port();
        target.options.keyboard_interactive = true;
        let profiles = Arc::new(crate::ProfileIpcService::new(
            cshell_storage::SqliteProfileRepository::open(root.path().join("profiles.db"))
                .await
                .unwrap(),
        ));
        let vault = &profiles.vault;
        let master = cshell_vault::Secret::new(b"master-password-for-test".to_vec());
        let generation = vault.response(None).lock_generation;
        vault.create(&master, generation).unwrap();
        vault
            .unlock(&master, vault.response(None).lock_generation)
            .unwrap();
        vault.set_mode(target.profile_id, 0, true).unwrap();
        if saved {
            vault
                .edit(
                    &target,
                    cshell_vault::VaultCredentialKind::Password,
                    Some(&cshell_vault::Secret::new(b"saved-password".to_vec())),
                )
                .unwrap();
        }
        let service = crate::SessionIpcService::new(Arc::new(
            crate::LocalSessionRegistry::new(root.path().join("journals"), 16).unwrap(),
        ))
        .with_profiles(profiles.clone());
        let known = root.path().join("known_hosts");
        std::fs::write(&known, format!("[127.0.0.1]:{} {public}\n", address.port())).unwrap();
        let plan = crate::ssh_route::SshConnectionPlan {
            title: "MFA".into(),
            target,
            jumps: vec![],
        };
        let vault = profiles.vault.clone();
        let job =
            tokio::spawn(
                async move { crate::ssh_route::connect_profile(&plan, &known, &vault).await },
            );
        let mut prompts = 0;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !job.is_finished() {
                let response = ipc(&service, VaultRequest::new(Op::KeyboardAuthPoll)).await;
                assert_eq!(response.status, Status::Ok as i32);
                if let Some(challenge) = response.auth_challenge {
                    prompts += 1;
                    assert_eq!(challenge.prompts.len(), 1);
                    let text = &challenge.prompts[0].text;
                    let answer = if text.starts_with("Verification code") {
                        "654321"
                    } else {
                        assert!(!saved, "saved password must never be requested over IPC");
                        assert_eq!(text, "密码");
                        "saved-password"
                    };
                    let mut request = VaultRequest::new(Op::KeyboardAuthRespond);
                    request.auth_answer = Some(cshell_ipc::KeyboardAuthAnswer {
                        id: challenge.id,
                        answers: vec![answer.into()],
                        cancel: false,
                        username:challenge.username,
                        ..Default::default()
                    });
                    assert_eq!(ipc(&service, request).await.status, Status::Ok as i32);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let connection = job
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("{}", e.detail));
        assert_eq!(prompts, if saved { 1 } else { 2 });
        connection.client.disconnect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
