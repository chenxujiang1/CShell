#![allow(clippy::unwrap_used)]
use super::*;
use russh::keys::ssh_key::{Algorithm, HashAlg, PrivateKey};
use russh::server::{Auth, Response};
use std::{borrow::Cow, sync::Arc};

#[test]
fn agent_partial_success_stops_identity_iteration_and_preserves_next_factors() {
    let partial = russh::client::AuthResult::Failure {
        remaining_methods: (&[
            russh::MethodKind::KeyboardInteractive,
            russh::MethodKind::Password,
        ][..])
            .into(),
        partial_success: true,
    };
    let mut attempted = 0;
    let result = [partial, russh::client::AuthResult::Success]
        .into_iter()
        .inspect(|_| attempted += 1)
        .find_map(agent_identity_outcome)
        .unwrap();
    assert_eq!(attempted, 1);
    let Err(SshError::AuthenticationPartial(methods)) = result else {
        panic!("accepted first factor must request the remaining factors");
    };
    assert_eq!(methods, ["keyboard-interactive", "password"]);
}

#[test]
fn agent_rejected_identity_continues_to_the_next_key() {
    let rejected = russh::client::AuthResult::Failure {
        remaining_methods: (&[russh::MethodKind::PublicKey][..]).into(),
        partial_success: false,
    };
    let mut attempted = 0;
    let result = [rejected, russh::client::AuthResult::Success]
        .into_iter()
        .inspect(|_| attempted += 1)
        .find_map(agent_identity_outcome)
        .unwrap();
    assert_eq!(attempted, 2);
    assert!(result.is_ok());
}
struct MfaServer {
    method_switch: bool,
    stage: u8,
}
impl russh::server::Handler for MfaServer {
    type Error = russh::Error;
    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(
            if self.method_switch && self.stage == 1 && password == "saved-password" {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if let Some(mut response) = response {
            let answer = response.next().unwrap_or_default();
            if self.stage == 0 {
                if answer.as_ref() != b"654321" {
                    return Ok(Auth::reject());
                }
                self.stage = 1;
                if self.method_switch {
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
            name: Cow::Borrowed("Two factors"),
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
async fn server(
    method_switch: bool,
) -> (std::net::SocketAddr, String, tokio::task::JoinHandle<()>) {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let config = russh::server::Config {
        keys: vec![key],
        auth_rejection_time: std::time::Duration::from_millis(1),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let job = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let handle = russh::server::run_stream(
            Arc::new(config),
            stream,
            MfaServer {
                method_switch,
                stage: 0,
            },
        )
        .await
        .unwrap();
        let _ = handle.await;
    });
    (address, fingerprint, job)
}
#[tokio::test]
async fn keyboard_mfa_code_then_password_and_partial_method_switch_both_authenticate() {
    for switch in [false, true] {
        let (address, fingerprint, server) = server(switch).await;
        let mut rounds = 0;
        let client = RusshClient::connect_keyboard_interactive(
            address,
            "operator",
            PinnedHostKey::sha256(fingerprint),
            |challenge| {
                rounds += 1;
                assert_eq!(challenge.prompts.len(), 1);
                let label = challenge.prompts[0].text.clone();
                let round = rounds;
                async move {
                    assert!(!challenge.prompts[0].echo);
                    Ok(vec![
                        if round == 1 {
                            assert_eq!(label, "Verification code: ");
                            "654321"
                        } else {
                            assert_eq!(label, "Password: ");
                            "saved-password"
                        }
                        .into(),
                    ])
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(rounds, 2);
        client.disconnect().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
#[tokio::test]
async fn keyboard_mfa_rejected_code_does_not_request_or_send_a_password() {
    let (address, fingerprint, server) = server(true).await;
    let mut rounds = 0;
    let result = RusshClient::connect_keyboard_interactive(
        address,
        "operator",
        PinnedHostKey::sha256(fingerprint),
        |_| {
            rounds += 1;
            async { Ok(vec!["wrong-code".into()]) }
        },
    )
    .await;
    assert!(matches!(result, Err(SshError::AuthenticationRejected)));
    assert_eq!(rounds, 1);
    server.abort();
}

struct OrderedMfaServer {
    stage: u8,
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
}
fn remaining(method: russh::MethodKind, partial_success: bool) -> Auth {
    Auth::Reject {
        proceed_with_methods: Some((&[method][..]).into()),
        partial_success,
    }
}
impl russh::server::Handler for OrderedMfaServer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _: &str) -> Result<Auth, Self::Error> {
        Ok(remaining(russh::MethodKind::KeyboardInteractive, false))
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        assert_eq!(self.stage, 0);
        if let Some(mut response) = response {
            self.calls.lock().unwrap().push("keyboard-interactive");
            if response.next().unwrap_or_default().as_ref() != b"654321" {
                return Ok(remaining(russh::MethodKind::KeyboardInteractive, false));
            }
            self.stage = 1;
            return Ok(remaining(russh::MethodKind::Password, true));
        }
        Ok(Auth::Partial {
            name: Cow::Borrowed("OTP"),
            instructions: Cow::Borrowed(""),
            prompts: Cow::Owned(vec![(Cow::Borrowed("Verification code: "), false)]),
        })
    }
    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        self.calls.lock().unwrap().push("password");
        assert_eq!(self.stage, 1);
        Ok(if password == "saved-password" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
}

async fn ordered_mfa_case(password_first: bool, enable_password: bool, correct_code: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = host_key.fingerprint(HashAlg::Sha256).to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handler = OrderedMfaServer {
        stage: 0,
        calls: calls.clone(),
    };
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let config = russh::server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::from_millis(1),
            ..Default::default()
        };
        let _ = russh::server::run_stream(Arc::new(config), stream, handler)
            .await
            .unwrap()
            .await;
    });
    let keyboard_calls = Arc::new(AtomicUsize::new(0));
    let keyboard = keyboard_calls.clone();
    let mut methods = vec![OrderedAuthStep::KeyboardInteractive(Box::new(
        move |challenge| {
            keyboard.fetch_add(1, Ordering::SeqCst);
            // A password second factor must use the configured password responder.
            assert_eq!(challenge.prompts[0].text, "Verification code: ");
            Box::pin(async move {
                Ok(vec![
                    if correct_code { "654321" } else { "wrong-code" }.into(),
                ])
            })
        },
    ))];
    if enable_password {
        methods.push(OrderedAuthStep::Password(Box::new(|_| {
            Box::pin(async { Ok("saved-password".into()) })
        })));
    }
    if password_first {
        // The first configured method is initially unavailable, then becomes
        // the required second factor after this later preference succeeds.
        methods.rotate_right(1);
    }
    let successes = Arc::new(AtomicUsize::new(0));
    let success = successes.clone();
    let mut ordered = OrderedAuthentication {
        methods,
        login: Box::new(|username| Box::pin(async { Ok(username) })),
        identity: Arc::new(std::sync::Mutex::new(String::new())),
        on_rejected: Box::new(|| {}),
        on_authenticated: Box::new(move || {
            success.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }),
    };
    let (mut session, _, _) = connect_verified(address, PinnedHostKey::sha256(fingerprint))
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        ordered_auth::authenticate(&mut session, "operator".into(), &mut ordered),
    )
    .await
    .unwrap();
    let expected_success = correct_code && enable_password;
    if expected_success {
        result.unwrap();
    } else {
        assert!(matches!(result, Err(SshError::AuthenticationRejected)));
    }
    assert_eq!(
        successes.load(Ordering::SeqCst),
        usize::from(expected_success)
    );
    assert_eq!(
        keyboard_calls.load(Ordering::SeqCst),
        if correct_code { 1 } else { 3 }
    );
    let expected = if !correct_code {
        vec![
            "keyboard-interactive",
            "keyboard-interactive",
            "keyboard-interactive",
        ]
    } else if enable_password {
        vec!["keyboard-interactive", "password"]
    } else {
        vec!["keyboard-interactive"]
    };
    assert_eq!(*calls.lock().unwrap(), expected);
    session
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn ordered_mfa_revisits_a_skipped_password_factor_after_keyboard_partial_success() {
    ordered_mfa_case(true, true, true).await;
}
#[tokio::test]
async fn ordered_mfa_never_sends_a_disabled_password_factor() {
    ordered_mfa_case(false, false, true).await;
}
#[tokio::test]
async fn ordered_mfa_uses_the_enabled_password_responder_after_keyboard_partial_success() {
    ordered_mfa_case(false, true, true).await;
}
#[tokio::test]
async fn ordered_mfa_rejected_keyboard_factor_keeps_retries_bounded() {
    ordered_mfa_case(false, true, false).await;
}
