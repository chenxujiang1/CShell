use super::*;
use cshell_domain::*;
use cshell_ipc::{
    Envelope, Handshake, HandshakePolicy, client_handshake, envelope, features, read_envelope,
    server_handshake, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey, PublicKey};
use russh::{
    Channel, ChannelId,
    server::{Auth, Msg, Session},
};
use std::{
    collections::{BTreeSet, HashMap},
    error::Error,
    net::SocketAddr,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
#[derive(Clone, Copy)]
enum Behavior {
    Complete,
    Rejected,
    MissingExit,
    Signal,
    EarlyFailure,
    DuplicateExit,
    Hanging,
}
struct Peer {
    key: PublicKey,
    behavior: Behavior,
    execs: Arc<AtomicUsize>,
    ptys: Arc<AtomicUsize>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}
impl russh::server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if user == "operator" && *key == self.key {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "operator" && password == "bound-password" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.ptys.fetch_add(1, Ordering::SeqCst);
        session.channel_failure(channel)?;
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        assert!(!command.is_empty());
        self.execs.fetch_add(1, Ordering::SeqCst);
        match self.behavior {
            Behavior::Rejected => {
                session.channel_failure(channel)?;
            }
            Behavior::Hanging => {
                session.channel_success(channel)?;
                return Ok(());
            }
            Behavior::EarlyFailure => {
                session.data(channel, b"early".to_vec())?;
                session.channel_failure(channel)?;
            }
            Behavior::Complete => {
                // Output precedes ACK; all subsequent chunks and stderr must survive.
                session.data(channel, b"early".to_vec())?;
                session.channel_success(channel)?;
                session.data(channel, vec![b'x'; 140_000])?;
                session.extended_data(channel, 1, b"error\0\xff".to_vec())?;
                session.exit_status_request(channel, 7)?;
            }
            Behavior::MissingExit => {
                session.channel_success(channel)?;
                session.data(channel, b"partial".to_vec())?;
            }
            Behavior::Signal => {
                session.channel_success(channel)?;
                session.exit_signal_request(
                    channel,
                    russh::Sig::TERM,
                    false,
                    "PRIVATE_REASON",
                    "en",
                )?;
            }
            Behavior::DuplicateExit => {
                session.channel_success(channel)?;
                session.exit_status_request(channel, 0)?;
                session.exit_status_request(channel, 1)?;
            }
        }
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}
struct Server {
    address: SocketAddr,
    key: PrivateKey,
    task: tokio::task::JoinHandle<()>,
    execs: Arc<AtomicUsize>,
    ptys: Arc<AtomicUsize>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(user: PublicKey, behavior: Behavior) -> TestResult<Self> {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let mut config = russh::server::Config::default();
        config.keys.push(key.clone());
        config.auth_rejection_time = Duration::from_millis(1);
        let config = Arc::new(config);
        let execs = Arc::new(AtomicUsize::new(0));
        let ptys = Arc::new(AtomicUsize::new(0));
        let (seen, pty_seen) = (execs.clone(), ptys.clone());
        let task = tokio::spawn(async move {
            let mut jobs = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break; };
                        let peer = Peer { key: user.clone(), behavior, execs: seen.clone(), ptys: pty_seen.clone(), channels: HashMap::new() };
                        let config = config.clone();
                        jobs.spawn(async move { if let Ok(server) = russh::server::run_stream(config, stream, peer).await { let _ = server.await; } });
                    }
                    _ = jobs.join_next(), if !jobs.is_empty() => {},
                }
            }
        });
        Ok(Self {
            address,
            key,
            task,
            execs,
            ptys,
        })
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    profiles: Arc<ProfileIpcService>,
    store: TaskOutputStore,
    record: TaskRunRecord,
    server: Server,
}
impl Fixture {
    async fn new(behavior: Behavior, count: usize) -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let user = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let server = Server::new(user.public_key().clone(), behavior).await?;
        let key_path = directory.path().join("user.key");
        std::fs::write(&key_path, user.to_openssh(LineEnding::LF)?.as_bytes())?;
        let trust_path = directory.path().join("known_hosts");
        std::fs::write(
            &trust_path,
            format!(
                "[127.0.0.1]:{} {}\n[localhost]:{} {}\n",
                server.address.port(),
                server.key.public_key().to_openssh()?,
                server.address.port(),
                server.key.public_key().to_openssh()?
            ),
        )?;
        let service = ProfileService::new(
            SqliteProfileRepository::open(directory.path().join("profiles.sqlite")).await?,
        );
        let id = ProfileId::new();
        let profile = ProfileRecord {
            id,
            name: "template".into(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::new(),
            favorite: false,
            terminal: TerminalOverrides::default(),
        };
        let connection = SshConnectionRecord {
            options: Default::default(),
            profile_id: id,
            host: "original-template.invalid".into(),
            port: 22,
            username: "operator".into(),
            auth_method: SshAuthMethod::PrivateKey,
            private_key_path: Some(key_path.to_string_lossy().into_owned()),
            certificate_path: None,
            agent_backend: SshAgentBackend::OpenSsh,
            agent_identity: None,
            route: SshRoute::Direct,
        };
        service
            .apply_batch(
                0,
                &[
                    CatalogChange::UpsertProfile(profile),
                    CatalogChange::UpsertSshConnection(connection),
                ],
            )
            .await?;
        let hosts = HostCatalog {
            hosts: (0..count)
                .map(|index| HostRecord {
                    id: HostId::new(),
                    name: format!("target-{index}"),
                    address: if index == 0 {
                        "127.0.0.1".into()
                    } else {
                        "localhost".into()
                    },
                    port: server.address.port(),
                    profile_id: id,
                    tags: BTreeSet::new(),
                    labels: BTreeMap::new(),
                    platform_hint: HostPlatformHint::Unknown,
                })
                .collect(),
            ..Default::default()
        };
        let hosts = service.repository().save_hosts(0, &hosts).await?;
        let frozen = freeze_hosts(
            &hosts,
            &service.load().await?.snapshot(),
            HostSelection::Selector {
                expression: "all".into(),
            },
            1000,
        )?;
        let definition = TaskDefinition {
            id: TaskId::new(),
            name: "probe".into(),
            shell: RemoteShellProfile::Posix,
            steps: vec![TaskStep {
                name: "step".into(),
                command: TaskCommand::Script {
                    text: "printf PRIVATE_COMMAND".into(),
                },
                success_codes: BTreeSet::from([7]),
                idempotent: true,
            }],
            variables: BTreeMap::new(),
            environment: BTreeMap::new(),
            policy: TaskPolicy::default(),
        };
        let record = TaskRunRecord::new(TaskSnapshot::new(
            definition,
            frozen,
            "operator".into(),
            SystemTaskClock::new()?.now_ms()?,
        )?)?;
        service.repository().create_task_run(&record).await?;
        let profiles = Arc::new(
            ProfileIpcService::new(service.into_repository()).with_known_hosts_path(trust_path),
        );
        let store = TaskOutputStore::new(directory.path().join("output"))?;
        Ok(Self {
            _directory: directory,
            profiles,
            store,
            record,
            server,
        })
    }
    fn service(&self) -> TestResult<TaskIpcService> {
        Ok(TaskIpcService::new(
            self.profiles.clone(),
            self.store.clone(),
        )?)
    }
    fn scheduler(&self) -> TestResult<TaskScheduler> {
        Ok(TaskScheduler::new(
            Arc::new(DaemonTaskRepository(self.profiles.clone())),
            Arc::new(SshTaskExecutor::new(
                self.profiles.clone(),
                self.store.clone(),
            )),
            TaskResourceBudget::new(TaskResourceLimits::default())?,
            Arc::new(SystemTaskClock::new()?),
        ))
    }
}
macro_rules! request { ($operation:ident $(, $field:ident: $value:expr)* $(,)?) => {{ let mut result = TaskRequest::default(); result.operation = TaskOperation::$operation as i32; $(result.$field = $value;)* result }}; }
async fn exchange(
    service: &crate::SessionIpcService,
    request: TaskRequest,
    bits: u64,
) -> TestResult<TaskResponse> {
    let (mut client, mut server) = tokio::io::duplex(64 * 1024);
    let policy = HandshakePolicy::with_instance_id([9; 32], [4; 16], features::TASK_CONTROL);
    let serving = async {
        let negotiated = server_handshake(&mut server, &policy).await?;
        service
            .serve_connection_with_features(server, negotiated.feature_bits)
            .await?;
        Ok::<_, Box<dyn Error>>(())
    };
    let querying = async {
        let mut handshake = Handshake::new(vec![4; 16], vec![9; 32]);
        handshake.feature_bits = bits;
        client_handshake(&mut client, 1, handshake).await?;
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TaskRequest(request)),
                ..Default::default()
            },
        )
        .await?;
        let reply = read_envelope(&mut client).await?;
        assert_eq!(reply.request_id, 2);
        drop(client);
        if let Some(envelope::Payload::TaskResponse(response)) = reply.payload {
            Ok::<_, Box<dyn Error>>(response)
        } else {
            Err("missing task response".into())
        }
    };
    let (served, response) = tokio::join!(serving, querying);
    served?;
    response
}
#[tokio::test]
async fn live_non_pty_task_preserves_early_output_stderr_and_known_exit() -> TestResult {
    let fixture = Fixture::new(Behavior::Complete, 1).await?;
    let (_, signals) = task_run_controls();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        fixture.scheduler()?.run(fixture.record.id(), signals),
    )
    .await??;
    assert_eq!(result.meta().state, TaskState::Completed);
    assert_eq!(result.targets()[0].steps[0].exit_code, Some(7));
    assert_eq!(fixture.server.ptys.load(Ordering::SeqCst), 0);
    let host = fixture.record.targets()[0].host_id;
    let mut out = Vec::new();
    let mut offset = 0;
    loop {
        let (page, length) = fixture
            .store
            .read(result.id(), host, 0, false, offset, 65536)
            .await?;
        offset += page.len() as u64;
        out.extend(page);
        if offset == length {
            break;
        }
    }
    assert_eq!(out, [b"early".as_slice(), &vec![b'x'; 140_000]].concat());
    assert_eq!(
        fixture
            .store
            .read(result.id(), host, 0, true, 0, 65536)
            .await?
            .0,
        b"error\0\xff"
    );
    let reopened = TaskOutputStore::new(fixture._directory.path().join("output"))?;
    assert_eq!(
        reopened.read(result.id(), host, 0, false, 0, 5).await?.0,
        b"early"
    );
    Ok(())
}
#[tokio::test]
async fn live_rejection_is_failed_but_missing_signal_or_conflicting_exit_is_unknown() -> TestResult
{
    for behavior in [
        Behavior::Rejected,
        Behavior::MissingExit,
        Behavior::Signal,
        Behavior::EarlyFailure,
        Behavior::DuplicateExit,
    ] {
        let fixture = Fixture::new(behavior, 1).await?;
        let (_, signals) = task_run_controls();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            fixture.scheduler()?.run(fixture.record.id(), signals),
        )
        .await??;
        assert_eq!(
            result.targets()[0].state,
            if matches!(behavior, Behavior::Rejected) {
                TargetState::Failed
            } else {
                TargetState::Unknown
            }
        );
        assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 1);
    }
    Ok(())
}
#[tokio::test]
async fn task_ipc_survives_client_disconnect_and_rejects_unnegotiated_or_stale_start() -> TestResult
{
    let fixture = Fixture::new(Behavior::Hanging, 1).await?;
    let tasks = fixture.service()?;
    let registry = Arc::new(crate::LocalSessionRegistry::new(
        fixture._directory.path().join("journals"),
        2,
    )?);
    let ipc = crate::SessionIpcService::new(registry).with_tasks(tasks.clone());
    let id = fixture.record.id().as_uuid().as_bytes().to_vec();
    let denied = exchange(
        &ipc,
        request!(Start, run_id: id.clone(), expected_revision: 1, confirmed: true),
        0,
    )
    .await?;
    assert_eq!(denied.status, TaskStatus::Unsupported as i32);
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 0);
    let stale = exchange(
        &ipc,
        request!(Start, run_id: id.clone(), expected_revision: 2, confirmed: true),
        features::TASK_CONTROL,
    )
    .await?;
    assert_eq!(stale.status, TaskStatus::Conflict as i32);
    let started = exchange(
        &ipc,
        request!(Start, run_id: id.clone(), expected_revision: 1, confirmed: true),
        features::TASK_CONTROL,
    )
    .await?;
    assert_eq!(started.status, TaskStatus::Ok as i32);
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.server.execs.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert!(
        tasks
            .control(fixture.record.id())
            .map_err(|s| format!("{s:?}"))?
            .is_some()
    );
    // Holding the ordinary operation lock must not block a direct cancel.
    let guard = tasks.0.operations.lock().await;
    let cancelled = tokio::time::timeout(
        Duration::from_millis(200),
        tasks.handle(&request!(Cancel, run_id: id.clone())),
    )
    .await?;
    assert_eq!(cancelled.status, TaskStatus::Ok as i32);
    assert_eq!(cancelled.revision, 0);
    drop(guard);
    tokio::time::timeout(Duration::from_secs(10), async {
        while tasks
            .control(fixture.record.id())
            .is_ok_and(|c| c.is_some())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let saved = tasks
        .load(fixture.record.id())
        .await
        .map_err(|s| format!("{s:?}"))?;
    assert_eq!(saved.targets()[0].state, TargetState::Unknown);
    let again = tasks
        .handle(
            &request!(Start, run_id: id, expected_revision: saved.meta().revision, confirmed: true),
        )
        .await;
    assert_eq!(again.status, TaskStatus::Conflict as i32);
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 1);
    Ok(())
}
#[tokio::test]
async fn orphaned_exec_intent_is_recovered_without_connecting_or_replaying() -> TestResult {
    let mut fixture = Fixture::new(Behavior::Complete, 1).await?;
    let tasks = fixture.service()?;
    let host = fixture.record.targets()[0].host_id;
    for action in [
        TaskRunAction::Start,
        TaskRunAction::Connect { host },
        TaskRunAction::StepIntent { host, step: 0 },
    ] {
        tasks
            .commit(&mut fixture.record, action)
            .await
            .map_err(|s| format!("{s:?}"))?;
    }
    let loaded = tasks
        .handle(&request!(Get, run_id: fixture.record.id().as_uuid().as_bytes().to_vec()))
        .await;
    assert_eq!(loaded.status, TaskStatus::Ok as i32);
    let saved = tasks
        .load(fixture.record.id())
        .await
        .map_err(|s| format!("{s:?}"))?;
    assert_eq!(saved.targets()[0].state, TargetState::Unknown);
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 0);
    assert!(!String::from_utf8_lossy(&loaded.document_json).contains("PRIVATE_COMMAND"));
    Ok(())
}
#[tokio::test]
async fn task_creation_freezes_catalogs_does_not_connect_and_rejects_mixed_fields() -> TestResult {
    let fixture = Fixture::new(Behavior::Complete, 1).await?;
    let tasks = fixture.service()?;
    let request = request!(Create, host_revision: 1, profile_revision: 1,
        definition_json: serde_json::to_vec(fixture.record.snapshot().definition())?,
        selection_json: serde_json::to_vec(&HostSelection::Selector { expression: "all".into() })?);
    let created = tasks.handle(&request).await;
    assert_eq!(created.status, TaskStatus::Ok as i32);
    let snapshot = decode_task_snapshot(&created.document_json)?;
    assert_eq!(
        snapshot
            .targets()
            .resolved_connection(fixture.record.targets()[0].host_id)
            .ok_or("host")?
            .port,
        fixture.server.address.port()
    );
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 0);
    let mut stale = request.clone();
    stale.host_revision = 2;
    assert_eq!(
        tasks.handle(&stale).await.status,
        TaskStatus::Conflict as i32
    );
    let mut mixed = request;
    mixed.confirmed = true;
    assert_eq!(
        tasks.handle(&mixed).await.status,
        TaskStatus::Invalid as i32
    );
    Ok(())
}
#[tokio::test]
async fn frozen_password_uses_original_binding_and_changed_host_key_blocks_exec() -> TestResult {
    let fixture = Fixture::new(Behavior::Complete, 1).await?;
    let profile_id = fixture.record.snapshot().targets().connections()[0].profile_id;
    let mut target = fixture.record.snapshot().targets().connections()[0].clone();
    target.auth_method = SshAuthMethod::Password;
    target.private_key_path = None;
    fixture
        .profiles
        .service
        .apply_batch(1, &[CatalogChange::UpsertSshConnection(target.clone())])
        .await?;
    let controller = fixture.profiles.vault.clone();
    let credential_target = target.clone();
    tokio::task::spawn_blocking(move || {
        controller.create(
            &cshell_vault::Secret::new(b"test-master-password".to_vec()),
            controller.response(None).lock_generation,
        )?;
        controller.set_mode(profile_id, 0, true)?;
        controller.edit(
            &credential_target,
            cshell_vault::VaultCredentialKind::Password,
            Some(&cshell_vault::Secret::new(b"bound-password".to_vec())),
        )
    })
    .await??;
    let hosts = fixture
        .profiles
        .service
        .repository()
        .load_hosts()
        .await?
        .ok_or("hosts")?;
    let profiles = fixture.profiles.service.load().await?.snapshot();
    let targets = freeze_hosts(
        &hosts,
        &profiles,
        HostSelection::Selector {
            expression: "all".into(),
        },
        1000,
    )?;
    let record = TaskRunRecord::new(TaskSnapshot::new(
        fixture.record.snapshot().definition().clone(),
        targets,
        "operator".into(),
        SystemTaskClock::new()?.now_ms()?,
    )?)?;
    fixture
        .profiles
        .service
        .repository()
        .create_task_run(&record)
        .await?;
    let (_, signals) = task_run_controls();
    let result = fixture.scheduler()?.run(record.id(), signals).await?;
    assert_eq!(result.meta().state, TaskState::Completed);
    let wrong = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    std::fs::write(
        fixture.profiles.known_hosts_path().map_err(|_| "trust")?,
        format!(
            "[127.0.0.1]:{} {}\n",
            fixture.server.address.port(),
            wrong.public_key().to_openssh()?
        ),
    )?;
    // Preserve the working password binding so rejection must reach host trust,
    // rather than an unavailable credential from the old private-key plan.
    let blocked_record = TaskRunRecord::new(TaskSnapshot::new(
        record.snapshot().definition().clone(),
        record.snapshot().targets().clone(),
        "operator".into(),
        SystemTaskClock::new()?.now_ms()?,
    )?)?;
    fixture
        .profiles
        .service
        .repository()
        .create_task_run(&blocked_record)
        .await?;
    let (_, signals) = task_run_controls();
    let blocked = fixture
        .scheduler()?
        .run(blocked_record.id(), signals)
        .await?;
    assert_eq!(blocked.targets()[0].state, TargetState::Failed);
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn live_output_limit_stops_task_as_unknown_with_partial_output() -> TestResult {
    let mut fixture = Fixture::new(Behavior::Complete, 1).await?;
    fixture.store = fixture.store.with_test_limit(8);
    let (_, signals) = task_run_controls();
    let result = fixture
        .scheduler()?
        .run(fixture.record.id(), signals)
        .await?;
    assert_eq!(result.targets()[0].state, TargetState::Unknown);
    assert_eq!(
        result.targets()[0].steps[0].reason,
        Some(TaskStopReason::OutputFailure)
    );
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn live_canary_requires_exact_gate_approval_before_second_host() -> TestResult {
    let fixture = Fixture::new(Behavior::Complete, 2).await?;
    let mut definition = fixture.record.snapshot().definition().clone();
    definition.policy.canary_count = 1;
    definition.policy.approve_after_canary = true;
    let record = TaskRunRecord::new(TaskSnapshot::new(
        definition,
        fixture.record.snapshot().targets().clone(),
        "operator".into(),
        SystemTaskClock::new()?.now_ms()?,
    )?)?;
    fixture
        .profiles
        .service
        .repository()
        .create_task_run(&record)
        .await?;
    let tasks = fixture.service()?;
    let id = record.id().as_uuid().as_bytes().to_vec();
    let start = tasks
        .handle(&request!(Start, run_id: id.clone(), expected_revision: 1, confirmed: true))
        .await;
    assert_eq!(start.status, TaskStatus::Ok as i32);
    let paused = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let record = tasks
                .load(record.id())
                .await
                .map_err(|s| format!("{s:?}"))?;
            if record.meta().state == TaskState::Paused {
                return Ok::<_, Box<dyn Error>>(record);
            }
            if task_terminal(record.meta().state) {
                return Err("canary failed before gate".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 1);
    let stale = tasks
        .handle(
            &request!(Approve, run_id: id.clone(), expected_revision: paused.meta().revision,
        gate_id: TaskGateId::new().as_uuid().as_bytes().to_vec(), confirmed: true),
        )
        .await;
    assert_eq!(stale.status, TaskStatus::Conflict as i32);
    let gate = paused.meta().gate.as_ref().ok_or("gate")?.id;
    let approved = tasks
        .handle(
            &request!(Approve, run_id: id, expected_revision: paused.meta().revision,
        gate_id: gate.as_uuid().as_bytes().to_vec(), confirmed: true),
        )
        .await;
    assert_eq!(approved.status, TaskStatus::Ok as i32);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let saved = tasks
                .load(record.id())
                .await
                .map_err(|s| format!("{s:?}"))?;
            if task_terminal(saved.meta().state) {
                assert_eq!(saved.meta().state, TaskState::Completed);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    assert_eq!(fixture.server.execs.load(Ordering::SeqCst), 2);
    Ok(())
}
