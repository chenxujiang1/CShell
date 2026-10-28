use super::*;
use cshell_domain::{ConnectionDiagnostic, ConnectionOutcome as Outcome, ConnectionStage as Stage};
use tokio::sync::watch;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diagnostics_heartbeat_detects_closed_client_and_releases_pending_mfa() {
    let f = Fixture::new(None).await;
    let (mut client, mut server) = tokio::io::duplex(65536);
    let service = f.service.clone();
    let job = tokio::spawn(async move { service.serve_one(&mut server).await });
    write_envelope(
        &mut client,
        &Envelope {
            request_id: 2,
            payload: Some(envelope::Payload::SessionCreateRequest(
                SessionCreateRequest {
                    connection_diagnostics: true,
                    profile_id: Some(f.profile.as_uuid().as_bytes().to_vec()),
                    rows: 24,
                    cols: 80,
                    ..Default::default()
                },
            )),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _prompt = challenge(&f).await;
    let first = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let reply = read_envelope(&mut client).await.unwrap();
            if let Some(envelope::Payload::ConnectionDiagnosticUpdate(update)) = reply.payload {
                let trace: ConnectionDiagnostic =
                    serde_json::from_slice(&update.document_json).unwrap();
                if trace
                    .steps
                    .last()
                    .is_some_and(|s| s.stage == Stage::WaitingForInput)
                {
                    return trace;
                }
            }
        }
    })
    .await
    .unwrap();
    let heartbeat = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let reply = read_envelope(&mut client).await.unwrap();
            if let Some(envelope::Payload::ConnectionDiagnosticUpdate(update)) = reply.payload {
                let trace: ConnectionDiagnostic =
                    serde_json::from_slice(&update.document_json).unwrap();
                if trace.elapsed_ms > first.elapsed_ms {
                    return trace;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        heartbeat.steps.last().unwrap().stage,
        Stage::WaitingForInput
    );
    assert!(heartbeat.validate());
    drop(client);
    assert!(
        tokio::time::timeout(Duration::from_secs(4), job)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let envelope::Payload::VaultResponse(reply) = f
        .call(envelope::Payload::VaultRequest(VaultRequest::new(
            VaultOperation::KeyboardAuthPoll,
        )))
        .await
    else {
        panic!()
    };
    assert!(reply.auth_challenge.is_none());
    let envelope::Payload::SessionListResponse(reply) = f
        .call(envelope::Payload::SessionListRequest(Default::default()))
        .await
    else {
        panic!()
    };
    assert!(reply.sessions.is_empty());
}

async fn observed(
    service: SessionIpcService,
    payload: envelope::Payload,
    updates: watch::Sender<Option<ConnectionDiagnostic>>,
) -> (envelope::Payload, ConnectionDiagnostic) {
    let (mut client, mut server) = tokio::io::duplex(65536);
    let job = tokio::spawn(async move {
        server_handshake(
            &mut server,
            &HandshakePolicy::with_instance_id([61; 32], [62; 16], u64::MAX),
        )
        .await
        .unwrap();
        service.serve_one(&mut server).await.unwrap();
    });
    let mut hello = Handshake::new(vec![62; 16], vec![61; 32]);
    hello.feature_bits = u64::MAX;
    client_handshake(&mut client, 1, hello).await.unwrap();
    write_envelope(
        &mut client,
        &Envelope {
            request_id: 2,
            payload: Some(payload),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut latest = None;
    loop {
        let reply = tokio::time::timeout(Duration::from_secs(12), read_envelope(&mut client))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.request_id, 2);
        match reply.payload.unwrap() {
            envelope::Payload::ConnectionDiagnosticUpdate(update) => {
                let value: ConnectionDiagnostic =
                    serde_json::from_slice(&update.document_json).unwrap();
                assert!(value.validate());
                let text = String::from_utf8(update.document_json).unwrap();
                for secret in [
                    "phase1",
                    "654321",
                    "Verification code",
                    "shared-ssh-test-master",
                ] {
                    assert!(
                        !text.contains(secret),
                        "diagnostics must not contain authentication material"
                    );
                }
                latest = Some(value.clone());
                updates.send_replace(Some(value));
            }
            payload => {
                job.await.unwrap();
                return (payload, latest.expect("live diagnostics"));
            }
        }
    }
}
async fn challenge(f: &Fixture) -> cshell_ipc::KeyboardAuthChallenge {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let envelope::Payload::VaultResponse(reply) = f
                .call(envelope::Payload::VaultRequest(VaultRequest::new(
                    VaultOperation::KeyboardAuthPoll,
                )))
                .await
            else {
                panic!()
            };
            if let Some(challenge) = reply.auth_challenge {
                return challenge;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
async fn answer(f: &Fixture, prompt: cshell_ipc::KeyboardAuthChallenge, cancel: bool) {
    let mut request = VaultRequest::new(VaultOperation::KeyboardAuthRespond);
    request.auth_answer = Some(KeyboardAuthAnswer {
        id: prompt.id,
        answers: if cancel {
            vec![]
        } else {
            vec!["654321".into()]
        },
        cancel,
        username: String::new(),
        save_credentials: false,
    });
    let envelope::Payload::VaultResponse(reply) =
        f.call(envelope::Payload::VaultRequest(request)).await
    else {
        panic!()
    };
    assert_eq!(reply.status, VaultControlStatus::Ok as i32);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diagnostics_stream_before_mfa_completion_and_shared_sftp_never_reauthenticates() {
    let f = Fixture::new(None).await;
    let (tx, mut rx) = watch::channel(None);
    let job = tokio::spawn(observed(
        f.service.clone(),
        envelope::Payload::SessionCreateRequest(SessionCreateRequest {
            connection_diagnostics: true,
            profile_id: Some(f.profile.as_uuid().as_bytes().to_vec()),
            rows: 24,
            cols: 80,
            ..Default::default()
        }),
        tx,
    ));
    let prompt = challenge(&f).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rx.borrow().as_ref().is_some_and(|d| {
                d.steps
                    .last()
                    .is_some_and(|s| s.stage == Stage::WaitingForInput)
            }) {
                break;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        !job.is_finished(),
        "progress must be visible while waiting for dynamic code"
    );
    answer(&f, prompt, false).await;
    let (payload, trace) = job.await.unwrap();
    let envelope::Payload::SessionCreateResponse(reply) = payload else {
        panic!()
    };
    let original = id(&reply.session.unwrap());
    assert_eq!(trace.outcome, Outcome::Succeeded);
    for stage in [
        Stage::Tcp,
        Stage::Handshake,
        Stage::HostKey,
        Stage::Authentication,
        Stage::WaitingForInput,
        Stage::TerminalChannel,
        Stage::Pty,
        Stage::Shell,
        Stage::Ready,
    ] {
        assert!(
            trace.steps.iter().any(|s| s.stage == stage),
            "missing {stage:?}"
        );
    }
    let session = SessionId::new();
    let (tx, _) = watch::channel(None);
    let (payload, trace) = observed(
        f.service.clone(),
        envelope::Payload::SftpRequest(SftpRequest {
            connection_diagnostics: true,
            operation: SftpOperation::ConnectProfile as i32,
            session_id: session.as_uuid().as_bytes().to_vec(),
            document_json: serde_json::to_vec(&cshell_application::SftpConnectRequest {
                profile: f.profile,
                source_ssh_session: Some(original),
                discover_directory: true,
            })
            .unwrap(),
            ..Default::default()
        }),
        tx,
    )
    .await;
    let envelope::Payload::SftpResponse(reply) = payload else {
        panic!()
    };
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    assert_eq!(trace.outcome, Outcome::Succeeded);
    for stage in [
        Stage::ReusingTransport,
        Stage::SftpChannel,
        Stage::SftpSubsystem,
        Stage::InitialDirectory,
        Stage::Ready,
    ] {
        assert!(
            trace.steps.iter().any(|s| s.stage == stage),
            "missing {stage:?}"
        );
    }
    assert!(!trace.steps.iter().any(|s| matches!(
        s.stage,
        Stage::Authentication | Stage::Tcp | Stage::WaitingForInput
    )));
    f.counts(1);
    let (tx, _) = watch::channel(None);
    let (payload, trace) = observed(
        f.service.clone(),
        envelope::Payload::SessionCreateRequest(SessionCreateRequest {
            connection_diagnostics: true,
            source_ssh_session_id: Some(original.as_uuid().as_bytes().to_vec()),
            rows: 24,
            cols: 80,
            ..Default::default()
        }),
        tx,
    )
    .await;
    let envelope::Payload::SessionCreateResponse(reply) = payload else {
        panic!()
    };
    let duplicate = id(&reply.session.unwrap());
    assert!(
        trace
            .steps
            .iter()
            .any(|s| s.stage == Stage::ReusingTransport)
    );
    assert!(!trace.steps.iter().any(|s| s.stage == Stage::Authentication));
    f.counts(1);
    f.close(duplicate).await;
    f.sftp(SftpOperation::Disconnect, session, None).await;
    f.close(original).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diagnostics_cancel_mfa_and_host_key_failure_keep_the_actual_stage() {
    let f = Fixture::new(None).await;
    for sftp in [false, true] {
        let (tx, _) = watch::channel(None);
        let session = SessionId::new();
        let payload = if sftp {
            envelope::Payload::SftpRequest(SftpRequest {
                connection_diagnostics: true,
                operation: SftpOperation::ConnectProfile as i32,
                session_id: session.as_uuid().as_bytes().to_vec(),
                document_json: serde_json::to_vec(&cshell_application::SftpConnectRequest {
                    profile: f.profile,
                    source_ssh_session: None,
                    discover_directory: true,
                })
                .unwrap(),
                ..Default::default()
            })
        } else {
            envelope::Payload::SessionCreateRequest(SessionCreateRequest {
                connection_diagnostics: true,
                profile_id: Some(f.profile.as_uuid().as_bytes().to_vec()),
                rows: 24,
                cols: 80,
                ..Default::default()
            })
        };
        let job = tokio::spawn(observed(f.service.clone(), payload, tx));
        let prompt = challenge(&f).await;
        answer(&f, prompt, true).await;
        let (_, trace) = job.await.unwrap();
        assert_eq!(trace.outcome, Outcome::Cancelled);
        assert_eq!(trace.steps.last().unwrap().stage, Stage::WaitingForInput);
    }
    let known = f._root.path().join("unknown-hosts");
    std::fs::write(&known, []).unwrap();
    let mut service = f.service.clone();
    service.known_hosts_path = Some(known);
    let (tx, _) = watch::channel(None);
    let (payload, trace) = observed(
        service,
        envelope::Payload::SessionCreateRequest(SessionCreateRequest {
            connection_diagnostics: true,
            profile_id: Some(f.profile.as_uuid().as_bytes().to_vec()),
            rows: 24,
            cols: 80,
            ..Default::default()
        }),
        tx,
    )
    .await;
    let envelope::Payload::SessionCreateResponse(reply) = payload else {
        panic!()
    };
    assert!(reply.session.is_none());
    assert_eq!(
        reply.failure_code,
        cshell_ipc::SessionFailureCode::HostKeyUnknown as i32
    );
    assert_eq!(trace.outcome, Outcome::Failed);
    assert_eq!(trace.steps.last().unwrap().stage, Stage::HostKey);
}
