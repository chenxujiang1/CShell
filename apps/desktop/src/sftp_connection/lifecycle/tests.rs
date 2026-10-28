use super::*;
use crate::daemon_connection::ResolvedDesktopEndpoint;
use cshell_ipc::{HandshakePolicy, server_handshake};
#[tokio::test]
async fn cancelled_creation_is_confirmed_disconnected_and_retry_clears_marker() {
    let (_root,listener,config,connection,mut receiver)=fixture();
    let profile=cshell_domain::ProfileId::new();let session=SessionId::new();
    assert!(connection.request(SftpClientCommand::ConnectProfile{session,profile,previous:None,source_ssh_session:None}));
    let command=receiver.recv().await.unwrap();
    let server=tokio::spawn(async move {
        let mut stream=listener.accept().await.unwrap();server_handshake(&mut stream,&HandshakePolicy::with_instance_id([32;32],[13;16],u64::MAX)).await.unwrap();
        let query=read_envelope(&mut stream).await.unwrap();
        write_envelope(&mut stream,&Envelope{request_id:query.request_id,payload:Some(envelope::Payload::SftpResponse(SftpResponse::with_status(SftpStatus::Cancelled,"AuthenticationCancelled: cancelled"))),..Default::default()}).await.unwrap();
    });
    run(&config,&connection.shared,command).await;server.await.unwrap();
    let view=connection.view();assert!(view.connection_cancelled);assert_eq!(view.connection_phase,ConnectionPhase::Disconnected);assert!(view.error.is_none());
    assert!(connection.take_cancelled_connection());assert!(!connection.take_cancelled_connection());
    assert!(connection.request(connect(profile,Some(session))));assert!(!connection.view().connection_cancelled);
}
fn fixture() -> (
    tempfile::TempDir,
    transport::LocalListener,
    DesktopConnectionConfig,
    DesktopSftpConnection,
    tokio::sync::mpsc::Receiver<SftpClientCommand>,
) {
    let root = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\cshell-sftp-life-{}", SessionId::new());
    #[cfg(unix)]
    let endpoint = root.path().join("sftp.sock");
    let listener = transport::LocalListener::bind(&endpoint).unwrap();
    let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
        endpoint,
        instance_token: [32; 32],
        daemon_instance_id: [13; 16],
    });
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let connection = DesktopSftpConnection {
        shared: Arc::new(Mutex::new(SftpView::default())),
        sender: Some(sender),
        shutdown,
        worker: None,
    };
    (root, listener, config, connection, receiver)
}
fn peer() -> cshell_domain::ClipboardTrust {
    cshell_domain::ClipboardTrust {
        host: cshell_domain::ClipboardHost::new("sftp.internal", 2222).unwrap(),
        username: "operator".into(),
        host_key_sha256: "verified-test-key".into(),
        route: "direct".into(),
    }
}
async fn respond(
    listener: &transport::LocalListener,
    op: SftpOperation,
    session: SessionId,
    profile: cshell_domain::ProfileId,
) -> SftpOperation {
    respond_with_source(listener, op, session, profile, None).await
}
async fn respond_with_source(
    listener: &transport::LocalListener,
    op: SftpOperation,
    session: SessionId,
    profile: cshell_domain::ProfileId,
    source_ssh_session: Option<SessionId>,
) -> SftpOperation {
    let mut stream = listener.accept().await.unwrap();
    let policy = HandshakePolicy::with_instance_id([32; 32], [13; 16], u64::MAX);
    server_handshake(&mut stream, &policy).await.unwrap();
    let envelope = read_envelope(&mut stream).await.unwrap();
    let Some(envelope::Payload::SftpRequest(query)) = envelope.payload else {
        panic!("SFTP query required")
    };
    assert_eq!(query.session_id, session.as_uuid().as_bytes());
    assert_eq!(query.operation, op as i32);
    let mut reply = SftpResponse::default();
    match op {
        SftpOperation::ConnectProfile | SftpOperation::ConnectionStatus => {
            if op == SftpOperation::ConnectProfile {
                let options: cshell_application::SftpConnectRequest = serde_json::from_slice(&query.document_json).unwrap();
                assert_eq!(options.profile,profile);
                assert_eq!(options.source_ssh_session,source_ssh_session);
                assert!(options.discover_directory);
            }
            reply.document_json = serde_json::to_vec(&cshell_application::SftpConnectionInfo {
                shared_from: source_ssh_session, initial_directory:Some(if source_ssh_session.is_some() {"/srv/work"} else {"/home/operator"}.into()),
                session,
                profile,
                title: "SFTP host".into(),
                peer: peer(),
                running: true,
            })
            .unwrap();
        }
        SftpOperation::QueueGet => {
            reply.document_json =
                serde_json::to_vec(&cshell_application::SftpQueueSnapshot::initial(session))
                    .unwrap()
        }
        SftpOperation::PeerGet => {
            reply.document_json = serde_json::to_vec(&SftpWorkspaceReply::Peer {
                session,
                peer: peer(),
            })
            .unwrap()
        }
        SftpOperation::BrowseManaged => {
            let options: SftpBrowseOptions = serde_json::from_slice(&query.document_json).unwrap();
            reply.browse_id = SessionId::new().as_uuid().as_bytes().to_vec();
            reply.browse_revision = 1;
            reply.browse_local = options.local;
            reply.browse_directory = if options.local {
                std::env::temp_dir().to_string_lossy().into_owned()
            } else {
                assert!(["/srv/work","/home/operator","/"].contains(&options.path.as_str()));
                options.path
            };
        }
        SftpOperation::QueueEnqueueAndStart => {
            assert!(query.confirmed);
            assert_eq!(query.expected_revision, 1);
            let drafts: Vec<cshell_application::SftpQueueDraft> =
                serde_json::from_slice(&query.document_json).unwrap();
            assert_eq!(drafts.len(), 1);
            assert!(drafts[0].verified);
            let mut queue = cshell_application::SftpQueueSnapshot::initial(session);
            queue.paused = false;
            queue.revision = 2;
            queue.concurrency = Some(3);
            queue.items.push(cshell_application::SftpQueueItem {
                id: cshell_domain::SftpQueueItemId::new(),
                draft: drafts[0].clone(),
                state: cshell_application::SftpQueueState::Pending,
                transfer_id: None,
                receipt: None,
                verified: None,
            });
            reply.document_json = serde_json::to_vec(&SftpWorkspaceReply::Queue(queue)).unwrap();
        }
        SftpOperation::Disconnect => {}
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
    op
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_loads_both_panes_and_reconnect_closes_old_transport_before_new_one() {
    let (_root, listener, config, connection, mut receiver) = fixture();
    let profile = cshell_domain::ProfileId::new();
    let first = SessionId::new();
    let second = SessionId::new();
    let server = tokio::spawn(async move {
        for (op, session) in [
            (SftpOperation::ConnectProfile, first),
            (SftpOperation::QueueGet, first),
            (SftpOperation::PeerGet, first),
            (SftpOperation::BrowseManaged, first),
            (SftpOperation::BrowseManaged, first),
            (SftpOperation::ConnectionStatus, first),
            (SftpOperation::Disconnect, first),
            (SftpOperation::Disconnect, first),
            (SftpOperation::ConnectProfile, second),
            (SftpOperation::QueueGet, second),
            (SftpOperation::PeerGet, second),
            (SftpOperation::BrowseManaged, second),
            (SftpOperation::BrowseManaged, second),
        ] {
            respond(&listener, op, session, profile).await;
        }
    });
    assert!(connection.request(SftpClientCommand::ConnectProfile {
        source_ssh_session: None,
        session: first,
        profile,
        previous: None
    }));
    assert_eq!(
        connection.view().connection_phase,
        ConnectionPhase::Connecting
    );
    assert!(
        !connection.request(connect(profile, Some(first))),
        "repeated connect must be rejected"
    );
    run(&config, &connection.shared, receiver.recv().await.unwrap()).await;
    let view = connection.view();
    assert_eq!(view.connection_phase, ConnectionPhase::Connected);
    assert!(view.local_directory.is_some());
    assert_eq!(view.remote_loaded_directory.as_deref(), Some("/home/operator"));
    assert_eq!(view.peer, Some(peer()));
    assert!(view.queue_writable, "initial queue load must permit uploads without a manual refresh");
    poll(&config, &connection.shared).await;
    assert_eq!(
        connection.view().connection_phase,
        ConnectionPhase::Connected
    );
    assert!(connection.request(SftpClientCommand::Disconnect(first)));
    assert!(!connection.request(SftpClientCommand::Disconnect(first)));
    assert!(
        !connection.request(SftpClientCommand::QueueGet(first)),
        "cannot browse while disconnecting"
    );
    run(&config, &connection.shared, receiver.recv().await.unwrap()).await;
    let view = connection.view();
    assert_eq!(view.connection_phase, ConnectionPhase::Disconnected);
    assert!(!view.queue_writable);
    assert!(view.remote_loaded_directory.is_none());
    assert_eq!(
        view.peer,
        Some(peer()),
        "disconnected identity remains visible"
    );
    assert!(connection.request(SftpClientCommand::ConnectProfile {
        source_ssh_session: None,
        session: second,
        profile,
        previous: Some(first)
    }));
    run(&config, &connection.shared, receiver.recv().await.unwrap()).await;
    assert_eq!(connection.view().session_id, Some(second));
    assert_eq!(
        connection.view().connection_phase,
        ConnectionPhase::Connected
    );
    server.await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_while_connecting_disconnects_immediately_after_reply_without_loading_directories()
{
    let (_root, listener, config, connection, mut receiver) = fixture();
    let profile = cshell_domain::ProfileId::new();
    let session = SessionId::new();
    assert!(connection.request(SftpClientCommand::ConnectProfile {
        source_ssh_session: None,
        session,
        profile,
        previous: None
    }));
    assert!(connection.request(SftpClientCommand::Disconnect(session)));
    assert!(connection.view().close_requested);
    let server = tokio::spawn(async move {
        respond(&listener, SftpOperation::ConnectProfile, session, profile).await;
        respond(&listener, SftpOperation::Disconnect, session, profile).await;
    });
    run(&config, &connection.shared, receiver.recv().await.unwrap()).await;
    assert_eq!(
        connection.view().connection_phase,
        ConnectionPhase::Disconnected
    );
    assert!(receiver.try_recv().is_err());
    assert!(connection.view().local_directory.is_none());
    server.await.unwrap();
}
#[test]
fn backend_unreachable_is_not_reported_as_confirmed_disconnect() {
    let shared = Arc::new(Mutex::new(SftpView::default()));
    let session = SessionId::new();
    shared.lock().unwrap().session_id = Some(session);
    closed(&shared, session, Some("IPC unavailable".into()));
    let view = shared.lock().unwrap();
    assert_eq!(view.connection_phase, ConnectionPhase::Uncertain);
    assert!(!view.status.contains("已断开"));
    assert!(!view.queue_writable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_old_disconnect_keeps_cleanup_identity_and_does_not_open_a_new_transport() {
    let (_root, listener, config, connection, mut receiver) = fixture();
    let profile = cshell_domain::ProfileId::new();
    let old = SessionId::new();
    let new = SessionId::new();
    {
        let mut view = connection.shared.lock().unwrap();
        view.session_id = Some(old);
        view.connection_profile = Some(profile);
        view.connection_phase = ConnectionPhase::Connected;
    }
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        server_handshake(
            &mut stream,
            &HandshakePolicy::with_instance_id([32; 32], [13; 16], u64::MAX),
        )
        .await
        .unwrap();
        let message = read_envelope(&mut stream).await.unwrap();
        let Some(envelope::Payload::SftpRequest(query)) = message.payload else {
            panic!("SFTP query required")
        };
        assert_eq!(query.operation, SftpOperation::Disconnect as i32);
        assert_eq!(query.session_id, old.as_uuid().as_bytes());
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: message.request_id,
                payload: Some(envelope::Payload::SftpResponse(SftpResponse::with_status(
                    SftpStatus::Failed,
                    "not acknowledged",
                ))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        respond(&listener, SftpOperation::Disconnect, old, profile).await;
        respond(&listener, SftpOperation::Disconnect, new, profile).await;
    });
    assert!(connection.request(SftpClientCommand::ConnectProfile {
        source_ssh_session: None,
        session: new,
        profile,
        previous: Some(old)
    }));
    run(&config, &connection.shared, receiver.recv().await.unwrap()).await;
    assert_eq!(
        connection.view().connection_phase,
        ConnectionPhase::Uncertain
    );
    assert_eq!(connection.view().connection_previous, Some(old));
    assert!(
        !connection.request(connect(profile, Some(new))),
        "must not forget the unconfirmed old transport"
    );
    assert!(connection.request(SftpClientCommand::Disconnect(new)));
    run(&config, &connection.shared, receiver.recv().await.unwrap()).await;
    assert_eq!(connection.view().connection_previous, None);
    assert_eq!(
        connection.view().connection_phase,
        ConnectionPhase::Disconnected
    );
    server.await.unwrap();
}

#[test]
fn late_old_queue_directory_and_error_replies_do_not_reset_a_reconnecting_session() {
    let old = SessionId::new();
    let new = SessionId::new();
    let shared = Arc::new(Mutex::new(SftpView {
        session_id: Some(new),
        connection_profile: Some(cshell_domain::ProfileId::new()),
        connection_phase: ConnectionPhase::Connecting,
        generation: 47,
        ..Default::default()
    }));
    publish_queue(
        &shared,
        old,
        Ok(SftpResponse {
            document_json: serde_json::to_vec(&cshell_application::SftpQueueSnapshot::initial(old))
                .unwrap(),
            ..Default::default()
        }),
        true,
        None,
    );
    publish_local(
        &shared,
        old,
        Ok(SftpResponse {
            document_json: serde_json::to_vec(&std::env::temp_dir().to_string_lossy()).unwrap(),
            ..Default::default()
        }),
    );
    publish(
        &shared,
        old,
        Ok(SftpResponse {
            entries: vec![SftpDirectoryEntry {
                name: "old.txt".into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        true,
    );
    publish(&shared, old, Err("stale network failure".into()), false);
    let view = shared.lock().unwrap();
    assert_eq!(view.session_id, Some(new));
    assert_eq!(view.connection_phase, ConnectionPhase::Connecting);
    assert_eq!(view.generation, 47);
    assert!(view.queue.is_none());
    assert!(view.entries.is_empty());
    assert!(view.local_directory.is_none());
    assert!(view.error.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_independent_and_shared_sftp_connections_accept_and_acknowledge_first_upload() {
    for source_ssh_session in [None, Some(SessionId::new())] {
        let (root, listener, config, connection, mut receiver) = fixture();
        connection.share_local_browser();
        let profile = cshell_domain::ProfileId::new();
        let session = SessionId::new();
        let file = root.path().join("first-upload.txt");
        std::fs::write(&file, b"first upload").unwrap();
        let server = tokio::spawn(async move {
            for op in [
                SftpOperation::ConnectProfile,
                SftpOperation::QueueGet,
                SftpOperation::PeerGet,
                SftpOperation::BrowseManaged,
                SftpOperation::QueueGet,
                SftpOperation::QueueEnqueueAndStart,
            ] {
                respond_with_source(&listener, op, session, profile, source_ssh_session).await;
            }
        });
        assert!(connection.request(SftpClientCommand::ConnectProfile {
            session, profile, previous: None, source_ssh_session,
        }));
        run(&config, &connection.shared, receiver.recv().await.unwrap()).await;
        let loaded = connection.view();
        assert_eq!(loaded.connection_phase, ConnectionPhase::Connected);
        assert_eq!(loaded.remote_loaded_directory.as_deref(),Some(if source_ssh_session.is_some() {"/srv/work"} else {"/home/operator"}));
        assert!(loaded.queue_writable, "the first browser queue must be writable: shared={source_ssh_session:?}");
        assert_eq!(loaded.connection_info.as_ref().unwrap().shared_from, source_ssh_session);
        let draft = cshell_application::SftpQueueDraft {
            verified: true,
            local_path: file.to_string_lossy().into_owned(),
            remote_path: "/first-upload.txt".into(),
            ..Default::default()
        };
        assert!(connection.request(SftpClientCommand::TransferBatch(
            session, vec![draft.clone()], loaded.queue.unwrap().revision, true,
        )), "first upload must be accepted without any queue refresh");
        assert!(connection.view().queue_busy);
        let SftpClientCommand::TransferBatch(id, drafts, revision, start) = receiver.recv().await.unwrap() else {
            panic!("expected upload batch");
        };
        crate::sftp_connection::transfer_controls::batch(
            &config, &connection.shared, id, drafts, revision, start,
        ).await;
        let acknowledged = connection.view();
        assert!(!acknowledged.queue_busy);
        assert!(acknowledged.queue_writable);
        assert!(acknowledged.error.is_none(), "{:?}", acknowledged.error);
        let queue = acknowledged.queue.unwrap();
        assert_eq!(queue.revision, 2);
        assert!(!queue.paused);
        assert_eq!(queue.items.len(), 1);
        assert_eq!(queue.items[0].draft, draft);
        server.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopening_browser_does_not_unlock_an_uncertain_existing_queue() {
    let (_root, listener, config, connection, _receiver) = fixture();
    let session = SessionId::new();
    let profile = cshell_domain::ProfileId::new();
    {
        let mut view = connection.shared.lock().unwrap();
        view.session_id = Some(session);
        view.connection_profile = Some(profile);
        view.connection_phase = ConnectionPhase::Connected;
        view.shared_local_browser = true;
        view.queue = Some(Arc::new(cshell_application::SftpQueueSnapshot::initial(session)));
        view.queue_writable = false;
    }
    let server = tokio::spawn(async move {
        for op in [SftpOperation::QueueGet, SftpOperation::PeerGet, SftpOperation::BrowseManaged] {
            respond(&listener, op, session, profile).await;
        }
    });
    load_browser(&config, &connection.shared, session).await;
    assert!(!connection.view().queue_writable, "reopening is not an explicit reconciliation of an uncertain write");
    server.await.unwrap();
}
