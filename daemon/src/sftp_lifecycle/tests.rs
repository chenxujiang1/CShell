use super::*;
use cshell_application::{CatalogChange, ProfileService};
use cshell_domain::{
    ProfileKind, ProfileRecord, SshAuthMethod, SshConnectionRecord, SshRoute, TerminalOverrides,
    TerminalSize,
};
use cshell_storage::SqliteProfileRepository;
use rand::rng;
use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::sync::atomic::AtomicUsize;
struct Fixture {
    root: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
    service: SessionIpcService,
    profile: ProfileId,
    address: std::net::SocketAddr,
    public: String,
    terminal_requests: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(deny_terminal: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let hostkey = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let public = hostkey.public_key().to_openssh().unwrap();
    let userkey = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let accepted = userkey.public_key().clone();
    let keypath = root.path().join("user-key");
    std::fs::write(
        &keypath,
        userkey.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = russh::server::Config::default();
    config.keys.push(hostkey);
    config.auth_rejection_time = std::time::Duration::from_millis(1);
    let config = Arc::new(config);
    let terminal_requests = Arc::new(AtomicUsize::new(0));
    let capture = terminal_requests.clone();
    let server = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut handler = crate::ssh_session::tests::EchoServer::default();
            handler.accepted_public_key = Some(accepted.clone());
            handler.deny_terminal = deny_terminal;
            handler.terminal_requests = Some(capture.clone());
            let config = config.clone();
            children.spawn(async move {
                if let Ok(running) = russh::server::run_stream(config, stream, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    let profile = ProfileId::new();
    let repository = SqliteProfileRepository::open(root.path().join("profiles.db"))
        .await
        .unwrap();
    let catalog = ProfileService::new(repository);
    catalog
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(ProfileRecord {
                    id: profile,
                    name: "SFTP host".into(),
                    kind: ProfileKind::Ssh,
                    folder_id: None,
                    tags: Default::default(),
                    favorite: false,
                    terminal: TerminalOverrides::default(),
                }),
                CatalogChange::UpsertSshConnection(SshConnectionRecord {
                    profile_id: profile,
                    host: "127.0.0.1".into(),
                    port: address.port(),
                    username: "cshell".into(),
                    auth_method: SshAuthMethod::PrivateKey,
                    private_key_path: Some(keypath.to_string_lossy().into_owned()),
                    certificate_path: None,
                    agent_backend: Default::default(),
                    agent_identity: None,
                    route: SshRoute::Direct,
                    options: Default::default(),
                }),
            ],
        )
        .await
        .unwrap();
    let known = root.path().join("known_hosts");
    std::fs::write(&known, format!("[127.0.0.1]:{} {public}\n", address.port())).unwrap();
    let service = SessionIpcService::new(Arc::new(
        crate::LocalSessionRegistry::new(root.path().join("journals"), 16).unwrap(),
    ))
    .with_known_hosts_path(known.clone())
    .with_profiles(Arc::new(
        crate::ProfileIpcService::new(catalog.into_repository()).with_known_hosts_path(known),
    ));
    Fixture {
        root,
        server,
        service,
        profile,
        address,
        public,
        terminal_requests,
    }
}
async fn command(f: &Fixture, op: SftpOperation, id: SessionId) -> SftpResponse {
    f.service
        .sftp_lifecycle(
            &SftpRequest {
                operation: op as i32,
                session_id: id.as_uuid().as_bytes().to_vec(),
                document_json: if op == SftpOperation::ConnectProfile {
                    serde_json::to_vec(&cshell_application::SftpConnectRequest {
                        profile: f.profile,
                        source_ssh_session: None,
                        discover_directory: true,
                    })
                    .unwrap()
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            u64::MAX,
        )
        .await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sftp_only_account_connects_lists_disconnects_and_reconnects_without_terminal() {
    let f = fixture(true).await;
    let first = SessionId::new();
    let connected = command(&f, SftpOperation::ConnectProfile, first).await;
    assert_eq!(
        connected.status,
        SftpStatus::Ok as i32,
        "{}",
        connected.detail
    );
    let info: cshell_application::SftpConnectionInfo =
        serde_json::from_slice(&connected.document_json).unwrap();
    assert_eq!(info.profile, f.profile);
    assert_eq!(info.session, first);
    assert!(info.running);
    assert_eq!(info.initial_directory.as_deref(), Some("/home/cshell"));
    assert_eq!(f.terminal_requests.load(Ordering::SeqCst), 0);
    assert!(
        f.service.ssh_sessions.list().is_empty(),
        "SFTP must not create terminal sessions"
    );
    assert!(
        !f.root
            .path()
            .join("journals")
            .join(format!("{first}.csjr"))
            .exists()
    );
    let target = f.service.ssh_sessions.get_sftp(first).unwrap();
    assert!(Arc::ptr_eq(
        &target,
        &f.service.ssh_sessions.get_sftp(first).unwrap()
    ));
    let listing = f
        .service
        .sftp
        .handle(
            SftpRequest {
                operation: SftpOperation::List as i32,
                session_id: first.as_uuid().as_bytes().to_vec(),
                remote_path: "/".into(),
                max_entries: 64,
                ..Default::default()
            },
            &f.service.ssh_sessions,
        )
        .await;
    assert_eq!(listing.status, SftpStatus::Ok as i32, "{}", listing.detail);
    assert_eq!(
        command(&f, SftpOperation::ConnectProfile, first)
            .await
            .status,
        SftpStatus::Conflict as i32
    );
    assert_eq!(
        command(&f, SftpOperation::ConnectionStatus, first)
            .await
            .status,
        SftpStatus::Ok as i32
    );
    assert_eq!(
        command(&f, SftpOperation::Disconnect, first).await.status,
        SftpStatus::Ok as i32
    );
    assert!(!target.running());
    assert!(target.open_sftp().await.is_err());
    assert!(f.service.ssh_sessions.get_sftp(first).is_err());
    assert_eq!(
        command(&f, SftpOperation::Disconnect, first).await.status,
        SftpStatus::Ok as i32,
        "disconnect is idempotent"
    );
    assert_eq!(
        command(&f, SftpOperation::ConnectionStatus, first)
            .await
            .status,
        SftpStatus::Unavailable as i32
    );
    let second = SessionId::new();
    assert_eq!(
        command(&f, SftpOperation::ConnectProfile, second)
            .await
            .status,
        SftpStatus::Ok as i32
    );
    assert_eq!(f.terminal_requests.load(Ordering::SeqCst), 0);
    f.service.shutdown_sftp().await;
    assert!(f.service.ssh_sessions.get_sftp(second).is_err());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnecting_sftp_keeps_terminal_alive_and_refuses_terminal_ids() {
    let f = fixture(false).await;
    let terminal_id = SessionId::new();
    let verifier = cshell_ssh::KnownHostsVerifier::parse(
        &format!("[127.0.0.1]:{} {}\n", f.address.port(), f.public),
        "127.0.0.1",
        f.address.port(),
    )
    .unwrap();
    let terminal = crate::SshSession::connect(
        terminal_id,
        "terminal".into(),
        crate::ssh_session::SshConnect {
            username: "cshell".into(),
            verifier,
            authentication: cshell_ssh::SshAuthentication::Password("phase1".into()),
        },
        TerminalSize::cells(24, 80),
        &f.root.path().join("terminal.csjr"),
        16,
    )
    .await
    .unwrap();
    let terminal = f.service.ssh_sessions.insert(terminal);
    let id = SessionId::new();
    assert_eq!(
        command(&f, SftpOperation::ConnectProfile, id).await.status,
        SftpStatus::Ok as i32
    );
    assert_eq!(
        command(&f, SftpOperation::Disconnect, terminal_id)
            .await
            .status,
        SftpStatus::InvalidRequest as i32
    );
    assert!(terminal.running());
    assert_eq!(
        command(&f, SftpOperation::Disconnect, id).await.status,
        SftpStatus::Ok as i32
    );
    assert!(terminal.running());
    assert_eq!(f.service.ssh_sessions.list().len(), 1);
    let subsystem = terminal.open_sftp().await.unwrap();
    let _ = subsystem.close().await;
    let next = SessionId::new();
    assert_eq!(
        command(&f, SftpOperation::ConnectProfile, next)
            .await
            .status,
        SftpStatus::Ok as i32
    );
    terminal.close().await.unwrap();
    f.service.ssh_sessions.remove(terminal_id).unwrap();
    assert_eq!(
        command(&f, SftpOperation::ConnectionStatus, next)
            .await
            .status,
        SftpStatus::Ok as i32
    );
    let subsystem = f
        .service
        .ssh_sessions
        .get_sftp(next)
        .unwrap()
        .open_sftp()
        .await
        .unwrap();
    let _ = subsystem.close().await;
    assert_eq!(
        command(&f, SftpOperation::Disconnect, next).await.status,
        SftpStatus::Ok as i32
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_host_and_missing_features_do_not_create_a_connection() {
    let f = fixture(true).await;
    std::fs::write(f.root.path().join("known_hosts"), "").unwrap();
    let id = SessionId::new();
    let query = SftpRequest {
        operation: SftpOperation::ConnectProfile as i32,
        session_id: id.as_uuid().as_bytes().to_vec(),
        document_json: serde_json::to_vec(&f.profile).unwrap(),
        ..Default::default()
    };
    assert_eq!(
        f.service
            .sftp_lifecycle(&query, features::SFTP_CONTROL)
            .await
            .status,
        SftpStatus::Unsupported as i32
    );
    let reply = command(&f, SftpOperation::ConnectProfile, id).await;
    assert!(
        reply.detail.starts_with("HostKeyUnknown:"),
        "{}",
        reply.detail
    );
    assert!(f.service.ssh_sessions.get_sftp(id).is_err());
    assert_eq!(f.terminal_requests.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_sftp_request_omits_new_receipt_fields_for_strict_old_clients() {
    let f = fixture(true).await;
    let session = SessionId::new();
    let reply = f
        .service
        .sftp_lifecycle(
            &SftpRequest {
                operation: SftpOperation::ConnectProfile as i32,
                session_id: session.as_uuid().as_bytes().to_vec(),
                document_json: serde_json::to_vec(&f.profile).unwrap(),
                ..Default::default()
            },
            u64::MAX,
        )
        .await;
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    let wire: serde_json::Value = serde_json::from_slice(&reply.document_json).unwrap();
    assert!(
        wire.get("initial_directory").is_none(),
        "old receipts deny unknown JSON fields"
    );
    let info: cshell_application::SftpConnectionInfo = serde_json::from_value(wire).unwrap();
    assert!(info.initial_directory.is_none());
    command(&f, SftpOperation::Disconnect, session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_connection_does_not_block_another_sessions_disconnect() {
    let f = fixture(true).await;
    let active = SessionId::new();
    assert_eq!(
        command(&f, SftpOperation::ConnectProfile, active)
            .await
            .status,
        SftpStatus::Ok as i32
    );
    let target = f.service.ssh_sessions.get_sftp(active).unwrap();

    // Accept TCP but never send an SSH banner. The second connection stays pending.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profiles = &f.service.profiles.as_ref().unwrap().service;
    let snapshot = profiles.load().await.unwrap().snapshot();
    let mut connection = snapshot.ssh_connections[0].clone();
    connection.port = listener.local_addr().unwrap().port();
    profiles
        .apply_batch(
            snapshot.revision,
            &[CatalogChange::UpsertSshConnection(connection)],
        )
        .await
        .unwrap();
    let pending = SessionId::new();
    {
        let opening = command(&f, SftpOperation::ConnectProfile, pending);
        tokio::pin!(opening);
        let _socket = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                reply = &mut opening => panic!("connection finished before handshake: {}", reply.detail),
            }
        }).await.unwrap();
        let invalid_disconnect = SftpRequest {
            operation: SftpOperation::Disconnect as i32,
            session_id: pending.as_uuid().as_bytes().to_vec(),
            document_json: b"unexpected".to_vec(),
            ..Default::default()
        };
        let invalid = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            f.service.sftp_lifecycle(&invalid_disconnect, u64::MAX),
        )
        .await
        .unwrap();
        assert_eq!(invalid.status, SftpStatus::InvalidRequest as i32);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut opening,)
                .await
                .is_err(),
            "an invalid disconnect must not cancel the connection"
        );
        let closed = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            command(&f, SftpOperation::Disconnect, active),
        )
        .await
        .expect("an unrelated connection must not delay disconnect");
        assert_eq!(closed.status, SftpStatus::Ok as i32, "{}", closed.detail);
        assert!(!target.running());
        let (cancelled, disconnected) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(
                    &mut opening,
                    command(&f, SftpOperation::Disconnect, pending),
                )
            })
            .await
            .expect("disconnect must cancel a stalled handshake immediately");
        assert_eq!(cancelled.status, SftpStatus::Cancelled as i32);
        assert_eq!(disconnected.status, SftpStatus::Ok as i32);
        assert!(f.service.ssh_sessions.get_sftp(pending).is_err());
    }
    // Explicit cancellation releases its reserved capacity.
    assert!(f.service.ssh_sessions.reserve_sftp(pending).is_ok());

    // Dropping an IPC connection future releases capacity without a disconnect request.
    let abandoned = SessionId::new();
    {
        let opening = command(&f, SftpOperation::ConnectProfile, abandoned);
        tokio::pin!(opening);
        let _socket = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                reply = &mut opening => panic!("connection finished before handshake: {}", reply.detail),
            }
        }).await.unwrap();
    }
    assert!(f.service.ssh_sessions.reserve_sftp(abandoned).is_ok());
}

#[test]
fn pending_connections_reserve_capacity_and_release_it_on_drop() {
    let registry = SshSessionRegistry::default();
    let id = SessionId::new();
    let first = registry.reserve_sftp(id).unwrap();
    assert_eq!(
        registry.reserve_sftp(id).err().unwrap().0,
        SftpStatus::Conflict
    );
    let mut pending = vec![first];
    for _ in 1..64 {
        pending.push(registry.reserve_sftp(SessionId::new()).unwrap());
    }
    assert_eq!(
        registry.reserve_sftp(SessionId::new()).err().unwrap().0,
        SftpStatus::CapacityReached
    );
    pending.pop();
    assert!(registry.reserve_sftp(SessionId::new()).is_ok());
    drop(pending);
    assert!(registry.reserve_sftp(id).is_ok());
}
