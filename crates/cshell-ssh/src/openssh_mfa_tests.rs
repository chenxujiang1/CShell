//! Explicit external-server test: the fixture must enforce publickey followed
//! by PAM keyboard-interactive, with 654321 as its isolated test-only code.
#![allow(clippy::unwrap_used)]
use super::*;
use futures::stream;
use russh::keys::ssh_key::{Algorithm, PrivateKey};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated OpenSSH publickey plus PAM keyboard-interactive fixture"]
async fn real_openssh_agent_partial_success_then_keyboard_interactive() {
    let root = std::path::PathBuf::from(std::env::var_os("CSHELL_OPENSSH_MFA_FIXTURE").unwrap());
    let environment = std::fs::read_to_string(root.join("environment.txt")).unwrap();
    let mut fields = environment.lines();
    let port = fields.next().unwrap().parse::<u16>().unwrap();
    let fingerprint = fields.next().unwrap();
    let username = fields.next().unwrap();
    let key = russh::keys::decode_secret_key(
        &std::fs::read_to_string(root.join("client_key")).unwrap(),
        None,
    )
    .unwrap();
    let selected = key
        .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
        .to_string();

    for select_identity in [false, true] {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        russh::keys::agent::server::serve(
            stream::iter([Ok::<_, std::io::Error>(server_stream)]),
            (),
        )
        .await
        .unwrap();
        let mut agent = russh::keys::agent::client::AgentClient::connect(client_stream);
        let rejected = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        agent.add_identity(&rejected, &[]).await.unwrap();
        agent.add_identity(&key, &[]).await.unwrap();
        let (mut session, forward_routes, verified_fingerprint) =
            connect_verified(("127.0.0.1", port), PinnedHostKey::sha256(fingerprint))
                .await
                .unwrap();
        let probe = session.authenticate_none(username).await.unwrap();
        let russh::client::AuthResult::Failure {
            remaining_methods,
            partial_success: false,
        } = probe
        else {
            panic!("isolated OpenSSH must initially require the public-key factor");
        };
        assert_eq!(remaining_methods.as_ref(), &[russh::MethodKind::PublicKey]);

        let result = authenticate_with_agent_selected(
            &mut session,
            username.into(),
            &mut agent,
            select_identity.then_some(selected.as_str()),
        )
        .await;
        let Err(SshError::AuthenticationPartial(methods)) = result else {
            panic!("Agent must preserve OpenSSH's accepted first factor");
        };
        assert_eq!(methods, ["keyboard-interactive"]);

        if !select_identity {
            let mut wrong =
                |_: KeyboardInteractiveChallenge| async { Ok(vec!["incorrect".into()]) };
            assert!(matches!(
                authenticate_keyboard_interactive_step(&mut session, username.into(), &mut wrong)
                    .await,
                Err(SshError::AuthenticationRejected)
            ));
        }
        let mut prompts = 0;
        let mut answer = |challenge: KeyboardInteractiveChallenge| {
            let answers = challenge
                .prompts
                .iter()
                .map(|p| {
                    assert!(!p.echo);
                    "654321".to_string()
                })
                .collect();
            prompts += challenge.prompts.len();
            async move { Ok(answers) }
        };
        authenticate_keyboard_interactive_step(&mut session, username.into(), &mut answer)
            .await
            .unwrap();
        assert!(prompts > 0);
        let client = RusshClient {
            session,
            forward_routes,
            verified_fingerprint,
        };
        let output = client
            .exec_with_pty(b"printf CSHELL_MFA_OK", 24, 80)
            .await
            .unwrap();
        assert_eq!(output.exit_status, 0);
        assert!(
            output
                .stdout
                .windows(b"CSHELL_MFA_OK".len())
                .any(|w| w == b"CSHELL_MFA_OK")
        );
        client.disconnect().await.unwrap();
    }
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated OpenSSH MFA fixture and an isolated Pageant process"]
async fn real_openssh_ordered_pageant_then_keyboard_interactive() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = std::path::PathBuf::from(std::env::var_os("CSHELL_OPENSSH_MFA_FIXTURE").unwrap());
    let environment = std::fs::read_to_string(root.join("environment.txt")).unwrap();
    let mut fields = environment.lines();
    let port = fields.next().unwrap().parse::<u16>().unwrap();
    let fingerprint = fields.next().unwrap();
    let username = fields.next().unwrap();
    let key = russh::keys::decode_secret_key(
        &std::fs::read_to_string(root.join("client_key")).unwrap(),
        None,
    )
    .unwrap();
    let selected = key
        .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
        .to_string();
    let mut agent = connect_agent_backend(AgentBackend::Pageant).await.unwrap();
    agent.add_identity(&key, &[]).await.unwrap();
    let prompts = Arc::new(AtomicUsize::new(0));
    let observed_prompts = prompts.clone();
    let successes = Arc::new(AtomicUsize::new(0));
    let observed_successes = successes.clone();
    let mut ordered = OrderedAuthentication {
        // This preference is initially unavailable. It must be reconsidered
        // after the later Pageant factor receives OpenSSH's partial success.
        methods: vec![
            OrderedAuthStep::KeyboardInteractive(Box::new(move |challenge| {
                observed_prompts.fetch_add(challenge.prompts.len(), Ordering::SeqCst);
                let answers = challenge
                    .prompts
                    .iter()
                    .map(|p| {
                        assert!(!p.echo);
                        "654321".to_string()
                    })
                    .collect();
                Box::pin(async move { Ok(answers) })
            })),
            OrderedAuthStep::Agent {
                backend: AgentBackend::Pageant,
                identity: Some(selected),
            },
        ],
        login: Box::new(|username| Box::pin(async { Ok(username) })),
        identity: Arc::new(std::sync::Mutex::new(String::new())),
        on_rejected: Box::new(|| {}),
        on_authenticated: Box::new(move || {
            observed_successes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }),
    };
    let (mut session, forward_routes, verified_fingerprint) =
        connect_verified(("127.0.0.1", port), PinnedHostKey::sha256(fingerprint))
            .await
            .unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        ordered_auth::authenticate(&mut session, username.into(), &mut ordered),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(prompts.load(Ordering::SeqCst) > 0);
    assert_eq!(successes.load(Ordering::SeqCst), 1);
    assert_eq!(*ordered.identity.lock().unwrap(), username);
    let client = RusshClient {
        session,
        forward_routes,
        verified_fingerprint,
    };
    let output = client
        .exec_with_pty(b"printf CSHELL_PAGEANT_MFA_OK", 24, 80)
        .await
        .unwrap();
    assert_eq!(output.exit_status, 0);
    assert!(
        output
            .stdout
            .windows(b"CSHELL_PAGEANT_MFA_OK".len())
            .any(|w| w == b"CSHELL_PAGEANT_MFA_OK")
    );
    client.disconnect().await.unwrap();
    agent.remove_identity(key.public_key()).await.unwrap();
}
