//! Real authenticated SSH/SFTP requests with isolated durable queue storage.
use super::*;
use crate::sftp_ipc::SftpIpcService;
use cshell_application::*;
use cshell_domain::SftpVerification;
use cshell_ipc::{SftpOperation, SftpRequest, SftpStatus};
use std::time::Duration;
struct Fixture {
    root: tempfile::TempDir,
    registry: super::super::SshSessionRegistry,
    service: SftpIpcService,
    state: Arc<Mutex<SftpFixtureState>>,
    id: SessionId,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        assert!(
            std::env::var_os("CSHELL_RUNTIME_DIR").is_some(),
            "use an isolated test runtime"
        );
        let root = tempfile::tempdir().unwrap();
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::from_millis(1),
            ..Default::default()
        });
        let state = Arc::new(Mutex::new(SftpFixtureState::default()));
        let remote = state.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let handler = EchoServer {
                sftp_state: remote,
                ..Default::default()
            };
            let running = russh::server::run_stream(config, stream, handler)
                .await
                .unwrap();
            let _ = running.await;
        });
        let verifier = KnownHostsVerifier::parse(
            &format!("[127.0.0.1]:{} {public}\n", address.port()),
            "127.0.0.1",
            address.port(),
        )
        .unwrap();
        let id = SessionId::new();
        let mut session = SshSession::connect(
            id,
            "policy".into(),
            SshConnect {
                username: "cshell".into(),
                verifier,
                authentication: super::super::SshAuthentication::Password("phase1".into()),
            },
            TerminalSize::cells(24, 80),
            &root.path().join("terminal.csjr"),
            16,
        )
        .await
        .unwrap();
        session.clipboard_trust = Some(cshell_domain::ClipboardTrust {
            host: cshell_domain::ClipboardHost {
                host: "127.0.0.1".into(),
                port: address.port(),
            },
            username: "cshell".into(),
            host_key_sha256: session
                .active_transport()
                .client
                .verified_host_key_fingerprint()
                .into(),
            route: "direct".into(),
        });
        let registry = super::super::SshSessionRegistry::default();
        registry.insert(session);
        Self {
            root,
            registry,
            service: Default::default(),
            state,
            id,
            server,
        }
    }
    async fn request(
        &self,
        operation: SftpOperation,
        revision: u64,
        document: Vec<u8>,
        transfer: Vec<u8>,
    ) -> cshell_ipc::SftpResponse {
        let response = self
            .service
            .handle(
                SftpRequest {
                    operation: operation as i32,
                    session_id: self.id.as_uuid().as_bytes().to_vec(),
                    expected_revision: revision,
                    confirmed: revision > 0,
                    document_json: document,
                    transfer_id: transfer,
                    ..Default::default()
                },
                &self.registry,
            )
            .await;
        assert_eq!(
            response.status,
            SftpStatus::Ok as i32,
            "{}",
            response.detail
        );
        response
    }
    async fn queue(&self) -> SftpQueueSnapshot {
        decode_sftp_queue(
            &self
                .request(SftpOperation::QueueGet, 0, vec![], vec![])
                .await
                .document_json,
        )
        .unwrap()
    }
    async fn start(&self, draft: &SftpQueueDraft) {
        let q = self.queue().await;
        let response = self
            .request(
                SftpOperation::QueueAddManaged,
                q.revision,
                serde_json::to_vec(draft).unwrap(),
                vec![],
            )
            .await;
        let q = decode_sftp_queue(&response.document_json).unwrap();
        self.request(SftpOperation::QueueStartManaged, q.revision, vec![], vec![])
            .await;
    }
    async fn settled(&self) -> SftpQueueSnapshot {
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let q = self.queue().await;
                if q.paused
                    && q.items.iter().all(|i| {
                        !matches!(i.state, SftpQueueState::Running | SftpQueueState::Pending)
                    })
                {
                    return q;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("transfer must settle")
    }
    async fn upload(&self, mode: SftpVerification, size: usize) -> (SftpQueueDraft, Vec<u8>) {
        let bytes = (0..size).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let source = self.root.path().join("source.bin");
        tokio::fs::write(&source, &bytes).await.unwrap();
        (
            SftpQueueDraft {
                verification: Some(mode),
                verified: true,
                local_path: source.to_string_lossy().into_owned(),
                remote_path: "/result.bin".into(),
                ..Default::default()
            },
            bytes,
        )
    }
    async fn success(&self, draft: &SftpQueueDraft) -> SftpQueueSnapshot {
        self.start(draft).await;
        let q = self.settled().await;
        assert_eq!(
            q.items.last().unwrap().state,
            SftpQueueState::Succeeded,
            "{:?}",
            q.items.last()
        );
        q
    }
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn standard_streams_once_and_coalesces_durable_checkpoints() {
    let f = Fixture::new().await;
    let (draft, bytes) = f
        .upload(SftpVerification::Standard, 20 * 1024 * 1024 + 17)
        .await;
    let start = std::time::Instant::now();
    let q = f.success(&draft).await;
    let state = f.state.lock().unwrap();
    eprintln!(
        "standard_upload bytes={} remote_read_bytes={} fsync={} queue_revision={}",
        bytes.len(),
        state.read_bytes,
        state.flushes,
        q.revision
    );
    assert_eq!(state.files.get("/result.bin"), Some(&bytes));
    assert_eq!(state.read_bytes, 0);
    assert_eq!(state.checksum_commands, 0);
    let timed_checkpoints = start.elapsed().as_secs() / 2;
    assert!(
        state.flushes <= 4 + timed_checkpoints,
        "{} fsync requests",
        state.flushes
    );
    assert!(
        q.revision < 25 + timed_checkpoints * 2,
        "{} queue writes",
        q.revision
    );
    assert!(state.files.keys().all(|path| !path.ends_with(".part")));
    assert!(!q.items[0].verified.as_ref().unwrap().digest_pending);
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn full_checksum_uses_server_after_namespace_challenge() {
    let f = Fixture::new().await;
    f.state.lock().unwrap().checksum_exec = true;
    let (draft, bytes) = f.upload(SftpVerification::Full, 2 * 1024 * 1024 + 17).await;
    f.success(&draft).await;
    let state = f.state.lock().unwrap();
    assert_eq!(state.files.get("/result.bin"), Some(&bytes));
    assert_eq!(state.read_bytes, 0);
    assert_eq!(state.checksum_commands, 1);
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn full_checksum_prefers_advertised_sftp_extension() {
    let f = Fixture::new().await;
    f.state.lock().unwrap().checksum_extension = true;
    let (draft, bytes) = f.upload(SftpVerification::Full, 1024 * 1024 + 17).await;
    f.success(&draft).await;
    let state = f.state.lock().unwrap();
    assert_eq!(state.files.get("/result.bin"), Some(&bytes));
    assert_eq!(state.read_bytes, 0);
    assert_eq!(state.checksum_commands, 1);
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn mixed_policies_preserve_three_file_concurrency_and_waiting_fourth() {
    let f = Fixture::new().await;
    let (mut draft, bytes) = f.upload(SftpVerification::Standard, 1024 * 1024 + 17).await;
    draft.rate_limit = Some(512 * 1024);
    for index in 0..4 {
        draft.remote_path = format!("/parallel-{index}.bin");
        draft.verification = Some(if index == 1 {
            SftpVerification::Full
        } else {
            SftpVerification::Standard
        });
        let q = f.queue().await;
        f.request(
            SftpOperation::QueueAddManaged,
            q.revision,
            serde_json::to_vec(&draft).unwrap(),
            vec![],
        )
        .await;
    }
    let q = f.queue().await;
    let reply = f
        .request(
            SftpOperation::QueueConfigure,
            q.revision,
            serde_json::to_vec(&SftpQueueSettings { concurrency: 3 }).unwrap(),
            vec![],
        )
        .await;
    let q = decode_sftp_queue(&reply.document_json).unwrap();
    f.request(SftpOperation::QueueStartManaged, q.revision, vec![], vec![])
        .await;
    let mut three = false;
    let mut fourth_waited = false;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let q = f.queue().await;
            let running = q
                .items
                .iter()
                .filter(|i| i.state == SftpQueueState::Running)
                .count();
            assert!(running <= 3);
            three |= running == 3;
            fourth_waited |=
                running == 3 && q.items.iter().any(|i| i.state == SftpQueueState::Pending);
            assert!(
                q.items.iter().all(|i| matches!(
                    i.state,
                    SftpQueueState::Pending | SftpQueueState::Running | SftpQueueState::Succeeded
                )),
                "{:?}",
                q.items
            );
            if q.paused && q.items.iter().all(|i| i.state == SftpQueueState::Succeeded) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    assert!(three && fourth_waited);
    let state = f.state.lock().unwrap();
    assert_eq!(state.read_bytes, bytes.len() as u64);
    for index in 0..4 {
        assert_eq!(
            state.files.get(&format!("/parallel-{index}.bin")),
            Some(&bytes)
        );
    }
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn full_checksum_falls_back_to_exactly_one_read_and_rejects_other_namespace() {
    for wrong_namespace in [false, true] {
        let f = Fixture::new().await;
        {
            let mut s = f.state.lock().unwrap();
            s.checksum_exec = wrong_namespace;
            s.checksum_wrong_namespace = wrong_namespace;
        }
        let (draft, bytes) = f.upload(SftpVerification::Full, 1024 * 1024 + 17).await;
        f.success(&draft).await;
        let state = f.state.lock().unwrap();
        assert_eq!(state.files.get("/result.bin"), Some(&bytes));
        assert_eq!(state.read_bytes, bytes.len() as u64);
        assert_eq!(state.checksum_commands, 0);
    }
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn full_checksum_mismatch_never_downgrades_or_publishes() {
    for command in [false, true] {
        let f = Fixture::new().await;
        {
            let mut s = f.state.lock().unwrap();
            s.checksum_exec = command;
            s.checksum_corrupt = command;
            s.corrupt_writes = !command;
        }
        let (draft, _) = f.upload(SftpVerification::Full, 128 * 1024 + 17).await;
        f.start(&draft).await;
        let q = f.settled().await;
        assert_eq!(q.items[0].state, SftpQueueState::Unknown);
        assert!(!q.items[0].verified.as_ref().unwrap().committed);
        assert!(!f.state.lock().unwrap().files.contains_key("/result.bin"));
    }
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn empty_upload_and_both_download_policies_need_no_extra_remote_pass() {
    for mode in [SftpVerification::Standard, SftpVerification::Full] {
        let f = Fixture::new().await;
        let (draft, _) = f.upload(mode, 0).await;
        f.success(&draft).await;
        let bytes = vec![5; 1024 * 1024 + 17];
        f.state
            .lock()
            .unwrap()
            .files
            .insert("/download.bin".into(), bytes.clone());
        let destination = f.root.path().join("download.bin");
        let draft = SftpQueueDraft {
            verification: Some(mode),
            verified: true,
            direction: SftpQueueDirection::Download,
            remote_path: "/download.bin".into(),
            local_path: destination.to_string_lossy().into_owned(),
            ..Default::default()
        };
        f.success(&draft).await;
        assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);
        assert_eq!(f.state.lock().unwrap().read_bytes, bytes.len() as u64);
    }
}
#[tokio::test]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn cancelled_uncheckpointed_writes_resume_only_when_prefix_matches() {
    for mutate_source in [false, true] {
        let f = Fixture::new().await;
        let (mut draft, bytes) = f
            .upload(SftpVerification::Standard, 3 * 1024 * 1024 + 17)
            .await;
        draft.rate_limit = Some(1024 * 1024);
        f.start(&draft).await;
        let transfer = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let q = f.queue().await;
                if let Some(transfer) = q.items[0].transfer_id {
                    let reply = f
                        .request(
                            SftpOperation::TransferStatus,
                            0,
                            vec![],
                            transfer.as_uuid().as_bytes().to_vec(),
                        )
                        .await;
                    if reply
                        .transfer
                        .is_some_and(|t| t.bytes_transferred >= 1024 * 1024)
                    {
                        break transfer;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        f.request(
            SftpOperation::CancelTransfer,
            0,
            vec![],
            transfer.as_uuid().as_bytes().to_vec(),
        )
        .await;
        let q = f.settled().await;
        let item = &q.items[0];
        assert!(item.can_resume_verified());
        assert!(item.verified.as_ref().unwrap().write_end.is_some());
        if mutate_source {
            let mut changed = bytes.clone();
            changed[0] ^= 1;
            tokio::fs::write(&draft.local_path, changed).await.unwrap();
        }
        f.request(
            SftpOperation::QueueRetryMany,
            q.revision,
            serde_json::to_vec(&SftpQueueRetry {
                items: vec![item.id],
                start: true,
            })
            .unwrap(),
            vec![],
        )
        .await;
        let q = f.settled().await;
        if mutate_source {
            assert_ne!(q.items[0].state, SftpQueueState::Succeeded);
            assert!(!f.state.lock().unwrap().files.contains_key("/result.bin"));
        } else {
            assert_eq!(
                q.items[0].state,
                SftpQueueState::Succeeded,
                "{:?}",
                q.items[0]
            );
            assert_eq!(
                f.state.lock().unwrap().files.get("/result.bin"),
                Some(&bytes)
            );
        }
    }
}
