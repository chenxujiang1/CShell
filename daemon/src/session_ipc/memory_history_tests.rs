use super::*;
use crate::{LocalSessionRegistry, SshSession};
use crate::ssh_session::SshConnect;
use cshell_domain::{InputAction, SessionId, TerminalSize};
use cshell_ipc::{Envelope, LogPageRequest, envelope, features, read_envelope, write_envelope};
use cshell_ssh::{KnownHostsVerifier, SshAuthentication};
use rand::rng;
use russh::keys::ssh_key::{Algorithm, PrivateKey};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_history_crosses_ipc_with_commands_and_client_anchors_do_not_move_live_cursor() {
    let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let public = key.public_key().to_openssh().unwrap();
    let mut config = russh::server::Config::default();
    config.keys.push(key);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let session = russh::server::run_stream(
            Arc::new(config),
            stream,
            crate::ssh_session::tests::EchoServer::default(),
        )
        .await
        .unwrap();
        let _ = session.await;
    });
    let directory = tempfile::tempdir().unwrap();
    let verifier = KnownHostsVerifier::parse(
        &format!("[127.0.0.1]:{} {public}\n", address.port()),
        "127.0.0.1",
        address.port(),
    )
    .unwrap();
    let session = SshSession::connect(
        SessionId::new(),
        "history".into(),
        SshConnect {
            username: "cshell".into(),
            verifier,
            authentication: SshAuthentication::Password("phase1".into()),
        },
        TerminalSize::cells(4, 80),
        &directory.path().join("ssh.csjr"),
        16,
    )
    .await
    .unwrap();
    let service = SessionIpcService::new(Arc::new(
        LocalSessionRegistry::new(directory.path().join("local"), 16).unwrap(),
    ));
    let session = service.ssh_sessions.insert(session);
    let text = (0..100)
        .map(|i| format!("command-{i:03}\r\n"))
        .collect::<String>();
    session.send_input(&InputAction::Text(text)).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let live = session
                .full_frame()
                .unwrap()
                .decode_terminal_snapshot()
                .unwrap();
            if live
                .cells
                .iter()
                .map(|c| c.character)
                .collect::<String>()
                .contains("command-099")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let before = session.full_frame().unwrap();
    let (mut client, daemon) = tokio::io::duplex(256 * 1024);
    let background = service.clone();
    let task = tokio::spawn(async move {
        background
            .serve_connection_with_features(
                daemon,
                features::LOG_PAGING | features::TERMINAL_SCROLLBACK,
            )
            .await
    });
    for anchor in [1, 70] {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: anchor,
                payload: Some(envelope::Payload::LogPageRequest(LogPageRequest {
                    session_id: session.id().as_uuid().as_bytes().to_vec(),
                    terminal_history: true,
                    anchor_line_id: Some(anchor),
                    rows_before: 0,
                    rows_after: 20,
                    cell_offset: 0,
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let reply = read_envelope(&mut client).await.unwrap();
        assert_eq!(reply.request_id, anchor);
        let Some(envelope::Payload::LogPage(page)) = reply.payload else {
            panic!("history page")
        };
        page.validate().unwrap();
        assert_eq!(page.anchor_line_id, anchor);
        if anchor == 1 {
            assert!(page.rows.iter().any(|r| r.text.contains("command-000")));
        }
    }
    let after = session.full_frame().unwrap();
    assert_eq!(before.payload, after.payload);
    drop(client);
    task.await.unwrap().unwrap();
    let (mut client, daemon) = tokio::io::duplex(65536);
    let background = service.clone();
    let task = tokio::spawn(async move {
        background
            .serve_connection_with_features(daemon, features::LOG_PAGING)
            .await
    });
    write_envelope(
        &mut client,
        &Envelope {
            request_id: 1,
            payload: Some(envelope::Payload::LogPageRequest(LogPageRequest {
                terminal_history: true,
                session_id: session.id().as_uuid().as_bytes().to_vec(),
                rows_before: 1,
                ..Default::default()
            })),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(task.await.unwrap().is_err());
    session.close().await.unwrap();
    server.abort();
}
