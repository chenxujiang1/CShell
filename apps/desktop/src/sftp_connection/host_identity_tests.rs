use super::*;
use crate::daemon_connection::ResolvedDesktopEndpoint;
use cshell_ipc::{HandshakePolicy, server_handshake};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_browser_fetches_authenticated_peer_before_listing_both_panes() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\cshell-sftp-identity-{session}");
    #[cfg(unix)]
    let endpoint = root.path().join("sftp.sock");
    let listener = transport::LocalListener::bind(&endpoint).unwrap();
    let peer = cshell_domain::ClipboardTrust {
        host: cshell_domain::ClipboardHost::new("server.internal", 2222).unwrap(),
        username: "operator".into(),
        host_key_sha256: "verified-host-key".into(),
        route: "direct".into(),
    };
    let returned_peer = peer.clone();
    let local = root.path().to_string_lossy().into_owned();
    let server = tokio::spawn(async move {
        let policy = HandshakePolicy::with_instance_id(
            [81; 32],
            [19; 16],
            features::SFTP_CONTROL
                | features::SFTP_BROWSER_QUEUE
                | features::SFTP_DIRECTORY_CURSOR
                | features::SFTP_WORKSPACE
                | features::SFTP_VERIFIED_QUEUE
                | features::SFTP_LIVE_CONTROL,
        );
        let mut seen = Vec::new();
        for _ in 0..4 {
            let mut stream = listener.accept().await.unwrap();
            server_handshake(&mut stream, &policy).await.unwrap();
            let envelope = read_envelope(&mut stream).await.unwrap();
            let Some(envelope::Payload::SftpRequest(query)) = envelope.payload else {
                panic!("expected SFTP request")
            };
            let op = SftpOperation::try_from(query.operation).unwrap();
            seen.push(op);
            assert_eq!(query.session_id, session.as_uuid().as_bytes());
            let mut reply = SftpResponse {
                status: SftpStatus::Ok as i32,
                ..Default::default()
            };
            match op {
                SftpOperation::QueueGet => {
                    reply.document_json =
                        serde_json::to_vec(&cshell_application::SftpQueueSnapshot::initial(session))
                            .unwrap()
                }
                SftpOperation::PeerGet => {
                    reply.document_json = serde_json::to_vec(&SftpWorkspaceReply::Peer {
                        session,
                        peer: returned_peer.clone(),
                    })
                    .unwrap()
                }
                SftpOperation::BrowseManaged => {
                    let options: SftpBrowseOptions =
                        serde_json::from_slice(&query.document_json).unwrap();
                    reply.browse_id = SessionId::new().as_uuid().as_bytes().to_vec();
                    reply.browse_revision = 1;
                    reply.browse_local = options.local;
                    reply.browse_directory = if options.local {
                        if options.path.is_empty() {
                            local.clone()
                        } else {
                            options.path
                        }
                    } else {
                        "/".into()
                    };
                }
                _ => panic!("unexpected operation"),
            }
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id: envelope.request_id,
                    payload: Some(envelope::Payload::SftpResponse(reply)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(
            seen,
            [
                SftpOperation::QueueGet,
                SftpOperation::PeerGet,
                SftpOperation::BrowseManaged,
                SftpOperation::BrowseManaged
            ]
        );
    });
    let connection = DesktopSftpConnection::start(DesktopConnectionConfig::for_fixture(
        ResolvedDesktopEndpoint {
            endpoint,
            instance_token: [81; 32],
            daemon_instance_id: [19; 16],
        },
    ))
    .unwrap();
    assert!(connection.request(SftpClientCommand::OpenBrowser(session)));
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let view = connection.view();
            if view.remote_loaded_directory.as_deref() == Some("/") {
                assert!(view.peer.as_ref() == Some(&peer));
                assert!(view.local_directory.is_some());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(connection);
    server.await.unwrap();
}
