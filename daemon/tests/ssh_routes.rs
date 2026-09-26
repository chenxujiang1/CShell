use cshell_application::{CatalogChange, ProfileService};
use cshell_domain::{
    InputAction, ProfileId, ProfileKind, ProfileRecord, SshAuthMethod, SshConnectionRecord,
    SshRoute, TerminalOverrides,
};
use cshell_ipc::{
    Envelope, ProfileOperation, ProfileRequest, ProfileStatus, SessionCloseRequest,
    SessionCreateRequest, SessionCreateResponse, SessionFailureCode, SnapshotRequest,
    TerminalInputRequest, envelope, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use rand::rng;
use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const TARGET: &str = "target.cshell.invalid";
const PORT: u16 = 2222;

struct RoutedServer {
    accepted_key: PublicKey,
    authentications: Arc<AtomicUsize>,
    forward: Option<SocketAddr>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}
impl russh::server::Handler for RoutedServer {
    type Error = russh::Error;
    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        if user == "cshell" && key == &self.accepted_key {
            self.authentications.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _origin: &str,
        _origin_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if host != TARGET || port != u32::from(PORT) || self.forward.is_none() {
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        let Some(address) = self.forward else {
            return Ok(());
        };
        let mut socket = tokio::net::TcpStream::connect(address).await?;
        reply.accept().await;
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _cols: u32,
        _rows: u32,
        _width: u32,
        _height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        if let Some(channel) = self.channels.remove(&channel) {
            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(channel.into_stream());
                let mut reader = BufReader::new(reader);
                let mut line = Vec::new();
                while reader
                    .read_until(b'\n', &mut line)
                    .await
                    .is_ok_and(|count| count > 0)
                {
                    if writer.write_all(b"ACK:").await.is_err()
                        || writer.write_all(&line).await.is_err()
                    {
                        break;
                    }
                    line.clear();
                }
            });
        }
        Ok(())
    }
}

#[allow(clippy::unwrap_used)]
async fn host(
    key: PrivateKey,
    accepted_key: PublicKey,
    count: Arc<AtomicUsize>,
    forward: Option<SocketAddr>,
) -> (SocketAddr, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = russh::server::Config::default();
    config.keys.push(key);
    config.auth_rejection_time = Duration::from_millis(1);
    let config = Arc::new(config);
    let active = Arc::new(AtomicUsize::new(0));
    let server_active = Arc::clone(&active);
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = Arc::clone(&config);
            let accepted_key = accepted_key.clone();
            let count = Arc::clone(&count);
            let active = Arc::clone(&server_active);
            tokio::spawn(async move {
                if let Ok(running) = russh::server::run_stream(
                    config,
                    stream,
                    RoutedServer {
                        accepted_key,
                        authentications: count,
                        forward,
                        channels: HashMap::new(),
                    },
                )
                .await
                {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _ = running.await;
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            });
        }
    });
    (address, task, active)
}

#[allow(clippy::unwrap_used)]
async fn proxy(http: bool, target: SocketAddr) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                if http {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                        assert!(request.len() < 4096);
                    }
                    assert_eq!(
                        request,
                        format!(
                            "CONNECT {TARGET}:{PORT} HTTP/1.1\r\nHost: {TARGET}:{PORT}\r\n\r\n"
                        )
                        .as_bytes()
                    );
                    stream
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                } else {
                    let mut hello = [0; 3];
                    stream.read_exact(&mut hello).await.unwrap();
                    assert_eq!(hello, [5, 1, 0]);
                    stream.write_all(&[5, 0]).await.unwrap();
                    let mut header = [0; 5];
                    stream.read_exact(&mut header).await.unwrap();
                    assert_eq!(&header[..4], &[5, 1, 0, 3]);
                    let mut host = vec![0; usize::from(header[4])];
                    stream.read_exact(&mut host).await.unwrap();
                    assert_eq!(host, TARGET.as_bytes());
                    assert_eq!(stream.read_u16().await.unwrap(), PORT);
                    stream
                        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 22])
                        .await
                        .unwrap();
                }
                let mut socket = tokio::net::TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
            });
        }
    });
    (address, task)
}

#[allow(clippy::unwrap_used)]
async fn exchange(service: &SessionIpcService, payload: envelope::Payload) -> Envelope {
    let (mut client, mut server) = tokio::io::duplex(128 * 1024);
    let request = async {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 7,
                payload: Some(payload),
                ..Envelope::default()
            },
        )
        .await
        .unwrap();
        read_envelope(&mut client).await.unwrap()
    };
    let (served, response) = tokio::join!(service.serve_one(&mut server), request);
    served.unwrap();
    response
}
#[allow(clippy::unwrap_used)]
async fn create(service: &SessionIpcService, id: ProfileId) -> SessionCreateResponse {
    let result = exchange(
        service,
        envelope::Payload::SessionCreateRequest(SessionCreateRequest {
            local_launch: None,
            rows: 24,
            cols: 80,
            profile_id: Some(id.as_uuid().as_bytes().to_vec()),
        }),
    )
    .await;
    let Some(envelope::Payload::SessionCreateResponse(created)) = result.payload else {
        panic!("missing creation response")
    };
    created
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::unwrap_used)]
async fn saved_routes_preview_confirm_and_connect_without_direct_fallback_or_target_credential_leak()
 {
    for mode in 0..3 {
        let temp = tempfile::tempdir().unwrap();
        let target_host_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let target_public = target_host_key.public_key().to_openssh().unwrap();
        let target_identity = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let target_auth = Arc::new(AtomicUsize::new(0));
        let (target_address, target_server, target_active) = host(
            target_host_key,
            target_identity.public_key().clone(),
            Arc::clone(&target_auth),
            None,
        )
        .await;
        let jump_host_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let jump_public = jump_host_key.public_key().to_openssh().unwrap();
        let jump_identity = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let jump_auth = Arc::new(AtomicUsize::new(0));
        let (jump_address, jump_server, jump_active) = host(
            jump_host_key,
            jump_identity.public_key().clone(),
            Arc::clone(&jump_auth),
            Some(target_address),
        )
        .await;
        let (proxy_address, proxy_server) = proxy(mode == 1, target_address).await;
        let target_key_path = temp.path().join("target-key");
        let jump_key_path = temp.path().join("jump-key");
        std::fs::write(
            &target_key_path,
            target_identity
                .to_openssh(LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        std::fs::write(
            &jump_key_path,
            jump_identity.to_openssh(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        let target_id = ProfileId::new();
        let jump_id = ProfileId::new();
        let record = |id, name: &str| ProfileRecord {
            id,
            name: name.into(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: Default::default(),
            favorite: false,
            terminal: TerminalOverrides::default(),
        };
        let route = match mode {
            0 => SshRoute::Socks5 {
                host: "127.0.0.1".into(),
                port: proxy_address.port(),
            },
            1 => SshRoute::HttpConnect {
                host: "127.0.0.1".into(),
                port: proxy_address.port(),
            },
            _ => SshRoute::Jump {
                profile_id: jump_id,
            },
        };
        let target = SshConnectionRecord {
            profile_id: target_id,
            host: TARGET.into(),
            port: PORT,
            username: "cshell".into(),
            auth_method: SshAuthMethod::PrivateKey,
            private_key_path: Some(target_key_path.to_str().unwrap().into()),
            certificate_path: None,
            agent_backend: Default::default(),
            agent_identity: None,
            route,
        };
        let jump = SshConnectionRecord {
            profile_id: jump_id,
            host: "127.0.0.1".into(),
            port: jump_address.port(),
            username: "cshell".into(),
            auth_method: SshAuthMethod::PrivateKey,
            private_key_path: Some(jump_key_path.to_str().unwrap().into()),
            certificate_path: None,
            agent_backend: Default::default(),
            agent_identity: None,
            route: SshRoute::Direct,
        };
        let repository = SqliteProfileRepository::open(temp.path().join("profiles.db"))
            .await
            .unwrap();
        let profile_service = ProfileService::new(repository);
        profile_service
            .apply_batch(
                0,
                &[
                    CatalogChange::UpsertProfile(record(target_id, "target")),
                    CatalogChange::UpsertProfile(record(jump_id, "jump")),
                    CatalogChange::UpsertSshConnection(target.clone()),
                    CatalogChange::UpsertSshConnection(jump),
                ],
            )
            .await
            .unwrap();
        let known_hosts = temp.path().join("known_hosts");
        let jump_line = format!("[127.0.0.1]:{} {jump_public}\n", jump_address.port());
        std::fs::write(&known_hosts, &jump_line).unwrap();
        let profiles = Arc::new(
            ProfileIpcService::new(profile_service.into_repository())
                .with_known_hosts_path(known_hosts.clone()),
        );
        let service = SessionIpcService::new(Arc::new(
            LocalSessionRegistry::new(temp.path().join("journals"), 16).unwrap(),
        ))
        .with_known_hosts_path(known_hosts.clone())
        .with_profiles(profiles);
        let unknown = create(&service, target_id).await;
        assert!(unknown.session.is_none(), "{}", unknown.detail);
        assert_eq!(
            unknown.failure_code,
            SessionFailureCode::HostKeyUnknown as i32
        );
        assert_eq!(target_auth.load(Ordering::SeqCst), 0);
        let preview = exchange(
            &service,
            envelope::Payload::ProfileRequest(ProfileRequest {
                operation: ProfileOperation::PreviewHostKey as i32,
                expected_revision: 1,
                credential_profile_id: target_id.as_uuid().as_bytes().to_vec(),
                ..ProfileRequest::default()
            }),
        )
        .await;
        let Some(envelope::Payload::ProfileResponse(preview)) = preview.payload else {
            panic!("missing preview")
        };
        assert_eq!(
            preview.status,
            ProfileStatus::Ok as i32,
            "{}",
            preview.detail
        );
        let preview = preview.host_key_preview.unwrap();
        assert_eq!(preview.host, TARGET);
        let confirmed = exchange(
            &service,
            envelope::Payload::ProfileRequest(ProfileRequest {
                operation: ProfileOperation::ConfirmHostKey as i32,
                expected_revision: 1,
                credential_profile_id: target_id.as_uuid().as_bytes().to_vec(),
                host_key_token: preview.token,
                host_key_fingerprint: preview.fingerprint,
                ..ProfileRequest::default()
            }),
        )
        .await;
        let Some(envelope::Payload::ProfileResponse(confirmed)) = confirmed.payload else {
            panic!("missing confirmation")
        };
        assert_eq!(
            confirmed.status,
            ProfileStatus::Ok as i32,
            "{}",
            confirmed.detail
        );
        assert_eq!(
            target_auth.load(Ordering::SeqCst),
            0,
            "preview/confirmation must not authenticate to target"
        );
        assert!(
            std::fs::read_to_string(&known_hosts)
                .unwrap()
                .contains(&format!("[{TARGET}]:{PORT} {target_public}"))
        );
        let created = create(&service, target_id).await;
        let session = created
            .session
            .unwrap_or_else(|| panic!("{}", created.detail));
        assert_eq!(
            session.profile_id,
            Some(target_id.as_uuid().as_bytes().to_vec())
        );
        assert_eq!(target_auth.load(Ordering::SeqCst), 1);
        exchange(
            &service,
            envelope::Payload::TerminalInputRequest(
                TerminalInputRequest::from_action(
                    cshell_domain::SessionId::from_bytes(
                        session.session_id.as_slice().try_into().unwrap(),
                    ),
                    &InputAction::Text("routed\n".into()),
                )
                .unwrap(),
            ),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = exchange(
                    &service,
                    envelope::Payload::SnapshotRequest(SnapshotRequest {
                        session_id: session.session_id.clone(),
                        ..SnapshotRequest::default()
                    }),
                )
                .await;
                if let Some(envelope::Payload::FullFrame(frame)) = snapshot.payload {
                    let snapshot = frame.decode_terminal_snapshot().unwrap();
                    if snapshot
                        .cells
                        .iter()
                        .map(|cell| cell.character)
                        .collect::<String>()
                        .contains("ACK:routed")
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        exchange(
            &service,
            envelope::Payload::SessionCloseRequest(SessionCloseRequest {
                apply_view_policy: false,
                session_id: session.session_id,
            }),
        )
        .await;
        let wrong = PrivateKey::random(&mut rng(), Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .to_openssh()
            .unwrap();
        std::fs::write(
            &known_hosts,
            format!("{jump_line}[{TARGET}]:{PORT} {wrong}\n"),
        )
        .unwrap();
        let changed = create(&service, target_id).await;
        assert!(changed.session.is_none());
        assert_eq!(
            changed.failure_code,
            SessionFailureCode::HostKeyChanged as i32,
            "{}",
            changed.detail
        );
        assert_eq!(target_auth.load(Ordering::SeqCst), 1);
        if mode == 2 {
            let before = jump_auth.load(Ordering::SeqCst);
            std::fs::write(
                &known_hosts,
                format!(
                    "[127.0.0.1]:{} {wrong}\n[{TARGET}]:{PORT} {target_public}\n",
                    jump_address.port()
                ),
            )
            .unwrap();
            let changed = create(&service, target_id).await;
            assert!(changed.session.is_none());
            assert_eq!(
                changed.failure_code,
                SessionFailureCode::HostKeyChanged as i32,
                "{}",
                changed.detail
            );
            assert!(changed.detail.starts_with("Jump Profile:"));
            assert_eq!(jump_auth.load(Ordering::SeqCst), before);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while jump_active.load(Ordering::SeqCst) != 0
                || target_active.load(Ordering::SeqCst) != 0
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if mode < 2 {
            // A reachable direct destination proves that failure must not bypass the proxy.
            let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let closed_port = unavailable.local_addr().unwrap().port();
            drop(unavailable);
            let mut blocked = target.clone();
            blocked.host = "127.0.0.1".into();
            blocked.port = target_address.port();
            blocked.route = if mode == 0 {
                SshRoute::Socks5 {
                    host: "127.0.0.1".into(),
                    port: closed_port,
                }
            } else {
                SshRoute::HttpConnect {
                    host: "127.0.0.1".into(),
                    port: closed_port,
                }
            };
            std::fs::write(
                &known_hosts,
                format!("[127.0.0.1]:{} {target_public}\n", target_address.port()),
            )
            .unwrap();
            let applied = exchange(
                &service,
                envelope::Payload::ProfileRequest(ProfileRequest {
                    operation: ProfileOperation::ApplyChanges as i32,
                    expected_revision: 1,
                    changes: vec![cshell_ipc::ProfileChange {
                        change: Some(cshell_ipc::profile_change::Change::UpsertSshConnection(
                            cshell_ipc::SshConnectionData::from(&blocked),
                        )),
                    }],
                    ..ProfileRequest::default()
                }),
            )
            .await;
            let Some(envelope::Payload::ProfileResponse(applied)) = applied.payload else {
                panic!("missing route update")
            };
            assert_eq!(
                applied.status,
                ProfileStatus::Ok as i32,
                "{}",
                applied.detail
            );
            let failed = create(&service, target_id).await;
            assert!(failed.session.is_none());
            assert_eq!(
                failed.failure_code,
                SessionFailureCode::NetworkUnavailable as i32,
                "{}",
                failed.detail
            );
            assert_eq!(
                target_auth.load(Ordering::SeqCst),
                1,
                "failed proxy must not connect to reachable direct destination"
            );
        }
        proxy_server.abort();
        jump_server.abort();
        target_server.abort();
    }
}
