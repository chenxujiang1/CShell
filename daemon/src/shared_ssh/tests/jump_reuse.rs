//! Authenticated IPC + real SSH MFA; managed-B integration additionally uses local OpenSSH.
use super::*;
use cshell_domain::{JumpConnectionOverride, JumpReusePolicy, SshRoute};
use cshell_ipc::{JumpCandidatesRequest, JumpCandidatesResponse, SessionFailureCode};
use std::io::Write;

async fn save_b(f: &Fixture, mut target: SshConnectionRecord) -> ProfileId {
    let profiles = f.service.profiles.as_ref().unwrap();
    let loaded = profiles.service.load().await.unwrap();
    let revision = loaded.snapshot().revision;
    target.profile_id = ProfileId::new();
    let id = target.profile_id;
    profiles
        .service
        .apply_batch(
            revision,
            &[
                CatalogChange::UpsertProfile(ProfileRecord {
                    id,
                    name: format!("B-{id}"),
                    kind: ProfileKind::Ssh,
                    folder_id: None,
                    tags: Default::default(),
                    favorite: false,
                    terminal: Default::default(),
                }),
                CatalogChange::UpsertSshConnection(target.clone()),
            ],
        )
        .await
        .unwrap();
    if !target.options.authentication_on_jump {
        let vault = &profiles.vault;
        let revision = vault.response(None).revision;
        vault.set_mode(id, revision, true).unwrap();
        vault
            .edit(
                &target,
                VaultCredentialKind::Password,
                Some(&Secret::new(b"phase1".to_vec())),
            )
            .unwrap();
    }
    id
}
async fn a_record(f: &Fixture) -> SshConnectionRecord {
    f.service
        .profiles
        .as_ref()
        .unwrap()
        .service
        .load()
        .await
        .unwrap()
        .snapshot()
        .ssh_connections
        .iter()
        .find(|target| target.profile_id == f.profile)
        .unwrap()
        .clone()
}
async fn query(f: &Fixture, id: ProfileId) -> JumpCandidatesResponse {
    let envelope::Payload::JumpCandidatesResponse(reply) = f
        .call(envelope::Payload::JumpCandidatesRequest(
            JumpCandidatesRequest {
                target_profile_id: id.as_uuid().as_bytes().to_vec(),
            },
        ))
        .await
    else {
        panic!("candidate response")
    };
    assert_eq!(
        reply.status,
        cshell_ipc::JumpCandidatesStatus::Ok as i32,
        "{}",
        reply.detail
    );
    reply
}
async fn open(
    f: &Fixture,
    profile: ProfileId,
    selection: Option<JumpConnectionOverride>,
) -> cshell_ipc::SessionCreateResponse {
    let envelope::Payload::SessionCreateResponse(reply) = f
        .call(envelope::Payload::SessionCreateRequest(
            SessionCreateRequest {
                profile_id: Some(profile.as_uuid().as_bytes().to_vec()),
                rows: 24,
                cols: 100,
                jump_override: selection.as_ref().map(Into::into),
                ..Default::default()
            },
        ))
        .await
    else {
        panic!("session response")
    };
    reply
}
fn choice(reply: &JumpCandidatesResponse, index: usize) -> JumpConnectionOverride {
    cshell_ipc::JumpConnectionOverrideData {
        policy: Some(2),
        candidate: reply.candidates[index].selection.clone(),
    }
    .try_into()
    .unwrap()
}
async fn setup_native(
    policy: JumpReusePolicy,
) -> (Fixture, super::jump_leases::Endpoint, ProfileId) {
    let b = super::jump_leases::Endpoint::new().await;
    let port = b.verifier.target().1;
    let f = Fixture::with_routes(None, Some(([127, 0, 0, 1], port).into()), None).await;
    std::fs::OpenOptions::new()
        .append(true)
        .open(f.service.known_hosts_path.as_ref().unwrap())
        .unwrap()
        .write_all(b.known_line.as_bytes())
        .unwrap();
    let mut target = a_record(&f).await;
    target.port = port;
    target.route = SshRoute::Jump {
        profile_id: f.profile,
    };
    target.options = SshConnectionOptions {
        jump_reuse_policy: policy,
        ..Default::default()
    };
    let id = save_b(&f, target).await;
    (f, b, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn native_b_and_independent_sftp_reuse_mfa_a_deduplicate_and_keep_channels_alive() {
    let (f, _endpoint, b) = setup_native(JumpReusePolicy::OnlyExisting).await;
    let missing = open(&f, b, None).await;
    assert!(missing.session.is_none());
    f.counts(0);
    let source = f.create().await;
    let a = f.service.ssh_sessions.get(id(&source)).unwrap();
    f.counts(1);
    assert_eq!(query(&f, b).await.candidates.len(), 1);
    let duplicate = f.duplicate(a.id()).await.session.unwrap();
    assert_eq!(query(&f, b).await.candidates.len(), 1);
    f.close(id(&duplicate)).await;
    let reserved = (0..63)
        .map(|_| a.fork_transport().unwrap())
        .collect::<Vec<_>>();
    assert!(open(&f, b, None).await.session.is_none());
    f.counts(1);
    for lease in reserved {
        assert!(lease.release_confirmed().await);
    }
    let connected = open(&f, b, None).await;
    let connected = connected
        .session
        .unwrap_or_else(|| panic!("{}", connected.detail));
    let terminal = f.service.ssh_sessions.get(id(&connected)).unwrap();
    echo(&a, "a-after-b").await;
    echo(&terminal, "b-through-a").await;
    f.counts(1);
    let sftp = SessionId::new();
    let envelope::Payload::SftpResponse(reply) = f
        .call(envelope::Payload::SftpRequest(SftpRequest {
            operation: SftpOperation::ConnectProfile as i32,
            session_id: sftp.as_uuid().as_bytes().to_vec(),
            document_json: serde_json::to_vec(&cshell_application::SftpConnectRequest {
                profile: b,
                source_ssh_session: None,
                discover_directory: true,
                jump_override: None,
            })
            .unwrap(),
            ..Default::default()
        }))
        .await
    else {
        panic!()
    };
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    f.counts(1);
    f.close(a.id()).await;
    echo(&terminal, "b-after-a-tag-close").await;
    f.call(envelope::Payload::SftpRequest(SftpRequest {
        operation: SftpOperation::Disconnect as i32,
        session_id: sftp.as_uuid().as_bytes().to_vec(),
        ..Default::default()
    }))
    .await;
    echo(&terminal, "b-after-sftp-close").await;
    f.close(terminal.id()).await;
    assert!(query(&f, b).await.candidates.is_empty());
    assert!(open(&f, b, None).await.session.is_none());
    f.counts(1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn multiple_candidates_require_choice_tokens_are_single_use_and_changes_do_not_reauthenticate()
 {
    let (f, _endpoint, b) = setup_native(JumpReusePolicy::OnlyExisting).await;
    let first = f.create().await;
    let second = f.create().await;
    f.counts(2);
    let candidates = query(&f, b).await;
    assert_eq!(candidates.candidates.len(), 2);
    assert_eq!(
        open(&f, b, None).await.failure_code,
        SessionFailureCode::JumpSelectionRequired as i32
    );
    f.counts(2);
    let selected = choice(&candidates, 0);
    let connected = open(&f, b, Some(selected.clone())).await;
    let connected = connected
        .session
        .unwrap_or_else(|| panic!("{}", connected.detail));
    assert!(open(&f, b, Some(selected)).await.session.is_none());
    f.counts(2);
    let expired = choice(&query(&f, b).await, 0);
    f.service.ssh_sessions.jump_reuse.expire_choices();
    assert!(open(&f, b, Some(expired)).await.session.is_none());
    f.counts(2);
    let stale = choice(&query(&f, b).await, 0);
    let original = std::fs::read(f.service.known_hosts_path.as_ref().unwrap()).unwrap();
    let revoked = String::from_utf8(original.clone())
        .unwrap()
        .lines()
        .map(|line| format!("@revoked {line}\n"))
        .collect::<String>();
    std::fs::write(f.service.known_hosts_path.as_ref().unwrap(), revoked).unwrap();
    assert!(open(&f, b, Some(stale)).await.session.is_none());
    f.counts(2);
    std::fs::write(f.service.known_hosts_path.as_ref().unwrap(), original).unwrap();
    f.close(id(&connected)).await;
    let first_transport = f
        .service
        .ssh_sessions
        .get(id(&first))
        .unwrap()
        .shared_lease()
        .identity();
    let candidates = query(&f, b).await;
    let index = candidates
        .candidates
        .iter()
        .position(|candidate| {
            candidate.selection.as_ref().unwrap().transport_id
                == first_transport.as_uuid().as_bytes()
        })
        .unwrap();
    let closed = choice(&candidates, index);
    f.close(id(&first)).await;
    assert_eq!(query(&f, b).await.candidates.len(), 1);
    assert!(open(&f, b, Some(closed)).await.session.is_none());
    f.counts(2);
    let stale = choice(&query(&f, b).await, 0);
    let profiles = f.service.profiles.as_ref().unwrap();
    let loaded = profiles.service.load().await.unwrap();
    let mut changed = a_record(&f).await;
    changed.username = "other-account".into();
    profiles
        .service
        .apply_batch(
            loaded.snapshot().revision,
            &[CatalogChange::UpsertSshConnection(changed)],
        )
        .await
        .unwrap();
    assert!(open(&f, b, Some(stale)).await.session.is_none());
    f.counts(2);
    f.close(id(&second)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_lock_cancels_first_and_queued_auth_without_late_session_or_second_challenge() {
    let (f, _endpoint, b) = setup_native(JumpReusePolicy::PreferExisting).await;
    let launch = |service: SessionIpcService| {
        tokio::spawn(async move {
            ipc(
                &service,
                envelope::Payload::SessionCreateRequest(SessionCreateRequest {
                    profile_id: Some(b.as_uuid().as_bytes().to_vec()),
                    rows: 24,
                    cols: 80,
                    ..Default::default()
                }),
            )
            .await
        })
    };
    let first = launch(f.service.clone());
    let prompt = vault_connection_prompt(&f).await;
    assert!(!prompt.vault_unlock);
    let second = launch(f.service.clone());
    tokio::time::sleep(Duration::from_millis(30)).await;
    f.service.profiles.as_ref().unwrap().vault.lock();
    for request in [first, second] {
        let envelope::Payload::SessionCreateResponse(reply) =
            tokio::time::timeout(Duration::from_secs(10), request)
                .await
                .unwrap()
                .unwrap()
        else {
            panic!()
        };
        assert!(reply.session.is_none());
    }
    assert_eq!(f.tcp.load(Ordering::SeqCst), 1);
    assert_eq!(f.codes.load(Ordering::SeqCst), 1);
    assert_eq!(f.passwords.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_b_requests_share_one_a_mfa_and_one_unlock() {
    let (f, _endpoint, b) = setup_native(JumpReusePolicy::PreferExisting).await;
    let profiles = f.service.profiles.as_ref().unwrap();
    profiles.vault.lock();
    let requests = (0..3)
        .map(|_| {
            let service = f.service.clone();
            tokio::spawn(async move {
                ipc(
                    &service,
                    envelope::Payload::SessionCreateRequest(SessionCreateRequest {
                        profile_id: Some(b.as_uuid().as_bytes().to_vec()),
                        rows: 24,
                        cols: 80,
                        ..Default::default()
                    }),
                )
                .await
            })
        })
        .collect::<Vec<_>>();
    let master = vault_connection_prompt(&f).await;
    assert!(master.vault_unlock);
    vault_connection_answer(&f, master, "shared-ssh-test-master", false).await;
    let code = vault_connection_prompt(&f).await;
    assert!(!code.vault_unlock);
    vault_connection_answer(&f, code, "654321", false).await;
    let mut opened = Vec::new();
    for request in requests {
        let envelope::Payload::SessionCreateResponse(reply) =
            tokio::time::timeout(Duration::from_secs(30), request)
                .await
                .unwrap()
                .unwrap()
        else {
            panic!()
        };
        let session = reply.session.unwrap_or_else(|| panic!("{}", reply.detail));
        echo(
            &f.service.ssh_sessions.get(id(&session)).unwrap(),
            "concurrent-b",
        )
        .await;
        opened.push(id(&session));
    }
    f.counts(1);
    for session in opened {
        f.close(session).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated CSHELL_MANAGED_SSH_FIXTURE; runs MFA SSH through a controlled exec bridge to real A/B OpenSSH"]
async fn managed_peer_real_openssh_mfa_a_reuse_locked_vault_upload_and_metrics() {
    let _fixture = crate::managed_peer_tests::fixture_guard().await;
    #[derive(serde::Deserialize)]
    struct Remote {
        a_port: u16,
        a_public: String,
        a_private_windows: String,
        b_port: u16,
        b_private_remote: String,
        b_files: String,
    }
    let fixture: Remote = serde_json::from_slice(
        &std::fs::read(std::env::var_os("CSHELL_MANAGED_SSH_FIXTURE").unwrap()).unwrap(),
    )
    .unwrap();
    let key = cshell_ssh::SshPrivateKey::decode_private_key(
        &std::fs::read_to_string(&fixture.a_private_windows).unwrap(),
        None,
    )
    .unwrap();
    let verifier = cshell_ssh::KnownHostsVerifier::parse(
        &format!("[127.0.0.1]:{} {}\n", fixture.a_port, fixture.a_public),
        "127.0.0.1",
        fixture.a_port,
    )
    .unwrap();
    let bridge = Arc::new(
        cshell_ssh::SshConnection::connect_known_hosts(
            "root".into(),
            cshell_ssh::SshAuthentication::PrivateKey(key),
            verifier,
        )
        .await
        .unwrap(),
    );
    let f = Fixture::with_routes(None, None, Some(bridge.clone())).await;
    let mut target = a_record(&f).await;
    target.port = fixture.b_port;
    target.username = "root".into();
    target.private_key_path = Some(fixture.b_private_remote.clone());
    target.route = SshRoute::Jump {
        profile_id: f.profile,
    };
    target.options = SshConnectionOptions {
        authentication_on_jump: true,
        jump_reuse_policy: JumpReusePolicy::OnlyExisting,
        ..Default::default()
    };
    let b = save_b(&f, target).await;
    let a = f.create().await;
    f.counts(1);
    f.service.profiles.as_ref().unwrap().vault.lock();
    let connected = open(&f, b, None).await;
    let connected = connected
        .session
        .unwrap_or_else(|| panic!("{}", connected.detail));
    let session = f.service.ssh_sessions.get(id(&connected)).unwrap();
    f.counts(1);
    let stats = crate::server_resources::Service::default()
        .sample(&f.service.ssh_sessions, session.id())
        .await
        .unwrap();
    assert!(stats.memory_total_kib > 0);
    let sftp = session.open_sftp().await.unwrap();
    let destination = format!("{}/mfa-reuse-proof.bin", fixture.b_files);
    let bytes = b"MFA-A TO REAL-B\0\xff\n";
    let mut input = bytes.as_slice();
    sftp.upload_atomic(
        &mut input,
        destination.clone(),
        Some(bytes.len() as u64),
        &cshell_sftp::TransferControl::default(),
        |_| {},
    )
    .await
    .unwrap();
    let mut restored = Vec::new();
    sftp.download(
        destination.clone(),
        &mut restored,
        &cshell_sftp::TransferControl::default(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(restored, bytes);
    sftp.remove_entry(&destination, false).await.unwrap();
    sftp.abort_channel();
    f.close(id(&a)).await;
    let alive = session
        .checksum_connection()
        .unwrap()
        .exec_with_pty("printf '__REAL_%s__' \"$CSHELL_FIXTURE_SIDE\"", 8, 100)
        .await
        .unwrap();
    assert_eq!(alive.stdout, b"__REAL_B__");
    f.close(session.id()).await;
    f.counts(1);
    assert!(bridge.disconnect_confirmed().await);
}
