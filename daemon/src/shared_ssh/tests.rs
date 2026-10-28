use super::*;
mod product_workspace;
mod connection_diagnostics;
use crate::{LocalSessionRegistry, ProfileIpcService, SessionIpcService, SshSession};
use cshell_application::{CatalogChange, ProfileService};
use cshell_domain::{
    InputAction, ProfileId, ProfileKind, ProfileRecord, SessionId, SshAuthMethod,
    SshConnectionOptions, SshConnectionRecord,
};
use cshell_ipc::{
    Envelope, Handshake, HandshakePolicy, KeyboardAuthAnswer, SessionCreateRequest, SessionSummary,
    SftpOperation, SftpRequest, SftpResponse, SftpStatus, VaultControlStatus, VaultOperation,
    VaultRequest, client_handshake, envelope, read_envelope, server_handshake, write_envelope,
};
use cshell_vault::{Secret, VaultCredentialKind};
use std::{sync::atomic::AtomicUsize, time::Duration};
struct Fixture {
    _root: tempfile::TempDir,
    service: SessionIpcService,
    profile: ProfileId,
    server: tokio::task::JoinHandle<()>,
    tcp: Arc<AtomicUsize>,
    codes: Arc<AtomicUsize>,
    passwords: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(terminal_limit: Option<usize>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let key = russh::keys::ssh_key::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::from_millis(1),
            ..Default::default()
        });
        let tcp = Arc::new(AtomicUsize::new(0));
        let codes = Arc::new(AtomicUsize::new(0));
        let passwords = Arc::new(AtomicUsize::new(0));
        let (connections, otp, pw) = (tcp.clone(), codes.clone(), passwords.clone());
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                connections.fetch_add(1, Ordering::SeqCst);
                let mut handler = crate::ssh_session::tests::EchoServer::default();
                handler.mfa_codes = Some(otp.clone());
                handler.mfa_passwords = Some(pw.clone());
                handler.terminal_limit = terminal_limit;
                handler.enable_upload_test_commands();
                let config = config.clone();
                tasks.spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        let profile = ProfileId::new();
        let target = SshConnectionRecord {
            profile_id: profile,
            host: "127.0.0.1".into(),
            port: address.port(),
            username: "cshell".into(),
            auth_method: SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: Default::default(),
            agent_identity: None,
            route: Default::default(),
            options: SshConnectionOptions {
                keyboard_interactive: true,
                ..Default::default()
            },
        };
        let repository =
            cshell_storage::SqliteProfileRepository::open(root.path().join("profiles.db"))
                .await
                .unwrap();
        let catalog = ProfileService::new(repository);
        catalog
            .apply_batch(
                0,
                &[
                    CatalogChange::UpsertProfile(ProfileRecord {
                        id: profile,
                        name: "MFA host".into(),
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
        let profiles = Arc::new(ProfileIpcService::new(catalog.into_repository()));
        let vault = &profiles.vault;
        let master = Secret::new(b"shared-ssh-test-master".to_vec());
        vault
            .create(&master, vault.response(None).lock_generation)
            .unwrap();
        vault
            .unlock(&master, vault.response(None).lock_generation)
            .unwrap();
        vault.set_mode(profile, 0, true).unwrap();
        vault
            .edit(
                &target,
                VaultCredentialKind::Password,
                Some(&Secret::new(b"phase1".to_vec())),
            )
            .unwrap();
        let known = root.path().join("known_hosts");
        std::fs::write(&known, format!("[127.0.0.1]:{} {public}\n", address.port())).unwrap();
        let service = SessionIpcService::new(Arc::new(
            LocalSessionRegistry::new(root.path().join("journals"), 16).unwrap(),
        ))
        .with_known_hosts_path(known)
        .with_profiles(profiles);
        Self {
            _root: root,
            service,
            profile,
            server,
            tcp,
            codes,
            passwords,
        }
    }
    async fn call(&self, payload: envelope::Payload) -> envelope::Payload {
        ipc(&self.service, payload).await
    }
    async fn create(&self) -> SessionSummary {
        let service = self.service.clone();
        let profile = self.profile;
        let job = tokio::spawn(async move {
            ipc(
                &service,
                envelope::Payload::SessionCreateRequest(SessionCreateRequest {
                    rows: 24,
                    cols: 80,
                    profile_id: Some(profile.as_uuid().as_bytes().to_vec()),
                    ..Default::default()
                }),
            )
            .await
        });
        let prompt = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let envelope::Payload::VaultResponse(reply) = self
                    .call(envelope::Payload::VaultRequest(VaultRequest::new(
                        VaultOperation::KeyboardAuthPoll,
                    )))
                    .await
                else {
                    panic!()
                };
                if let Some(prompt) = reply.auth_challenge {
                    break prompt;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!prompt.login);
        assert!(prompt.prompts[0].text.contains("Verification"));
        let mut reply = VaultRequest::new(VaultOperation::KeyboardAuthRespond);
        reply.auth_answer = Some(KeyboardAuthAnswer {
            id: prompt.id,
            answers: vec!["654321".into()],
            cancel: false,
            username: String::new(),
            save_credentials: false,
        });
        let envelope::Payload::VaultResponse(answer) =
            self.call(envelope::Payload::VaultRequest(reply)).await
        else {
            panic!()
        };
        assert_eq!(answer.status, VaultControlStatus::Ok as i32);
        let envelope::Payload::SessionCreateResponse(reply) =
            tokio::time::timeout(Duration::from_secs(8), job)
                .await
                .unwrap()
                .unwrap()
        else {
            panic!()
        };
        reply.session.unwrap_or_else(|| panic!("{}", reply.detail))
    }
    async fn duplicate(&self, source: SessionId) -> cshell_ipc::SessionCreateResponse {
        let envelope::Payload::SessionCreateResponse(reply) = self
            .call(envelope::Payload::SessionCreateRequest(
                SessionCreateRequest {
                    rows: 24,
                    cols: 80,
                    source_ssh_session_id: Some(source.as_uuid().as_bytes().to_vec()),
                    ..Default::default()
                },
            ))
            .await
        else {
            panic!()
        };
        reply
    }
    async fn sftp(
        &self,
        operation: SftpOperation,
        id: SessionId,
        source: Option<SessionId>,
    ) -> SftpResponse {
        let envelope::Payload::SftpResponse(reply) = self
            .call(envelope::Payload::SftpRequest(SftpRequest {
                operation: operation as i32,
                session_id: id.as_uuid().as_bytes().to_vec(),
                document_json: if operation == SftpOperation::ConnectProfile {
                    serde_json::to_vec(&cshell_application::SftpConnectRequest {
                        profile: self.profile,
                        source_ssh_session: source,
                        discover_directory:true,
                    })
                    .unwrap()
                } else {
                    vec![]
                },
                ..Default::default()
            }))
            .await
        else {
            panic!()
        };
        reply
    }
    async fn close(&self, id: SessionId) {
        let reply = self
            .call(envelope::Payload::SessionCloseRequest(
                cshell_ipc::SessionCloseRequest {
                    session_id: id.as_uuid().as_bytes().to_vec(),
                    apply_view_policy: false,
                },
            ))
            .await;
        assert!(matches!(reply, envelope::Payload::SessionCloseResponse(_)));
    }
    fn counts(&self, expected: usize) {
        assert_eq!(self.tcp.load(Ordering::SeqCst), expected);
        assert_eq!(self.codes.load(Ordering::SeqCst), expected);
        assert_eq!(self.passwords.load(Ordering::SeqCst), expected);
    }
}
async fn ipc(service: &SessionIpcService, payload: envelope::Payload) -> envelope::Payload {
    let (mut client, mut server) = tokio::io::duplex(65536);
    let service = service.clone();
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
    let reply = read_envelope(&mut client).await.unwrap();
    job.await.unwrap();
    assert_eq!(reply.request_id, 2);
    reply.payload.unwrap()
}
fn id(summary: &SessionSummary) -> SessionId {
    SessionId::from_bytes(summary.session_id.as_slice().try_into().unwrap())
}
#[tokio::test(flavor="multi_thread",worker_threads=3)]
async fn cancelling_mfa_reports_cancelled_for_ssh_and_sftp_and_creates_no_session() {
    let f=Fixture::new(None).await;
    for sftp in [false,true] {
        let service=f.service.clone();let profile=f.profile;let session=SessionId::new();
        let payload=if sftp {envelope::Payload::SftpRequest(SftpRequest{operation:SftpOperation::ConnectProfile as i32,session_id:session.as_uuid().as_bytes().to_vec(),document_json:serde_json::to_vec(&cshell_application::SftpConnectRequest{profile,source_ssh_session:None,discover_directory:true}).unwrap(),..Default::default()})} else {envelope::Payload::SessionCreateRequest(SessionCreateRequest{profile_id:Some(profile.as_uuid().as_bytes().to_vec()),rows:24,cols:80,..Default::default()})};
        let job=tokio::spawn(async move {ipc(&service,payload).await});
        let challenge=tokio::time::timeout(Duration::from_secs(8),async {loop {
            let envelope::Payload::VaultResponse(reply)=f.call(envelope::Payload::VaultRequest(VaultRequest::new(VaultOperation::KeyboardAuthPoll))).await else {panic!()};
            if let Some(challenge)=reply.auth_challenge {break challenge;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.unwrap();
        assert!(!challenge.login);
        let mut reply=VaultRequest::new(VaultOperation::KeyboardAuthRespond);reply.auth_answer=Some(KeyboardAuthAnswer{id:challenge.id,answers:vec![],cancel:true,username:String::new(),save_credentials:false});
        let envelope::Payload::VaultResponse(reply)=f.call(envelope::Payload::VaultRequest(reply)).await else {panic!()};assert_eq!(reply.status,VaultControlStatus::Ok as i32);
        match tokio::time::timeout(Duration::from_secs(8),job).await.unwrap().unwrap() {
            envelope::Payload::SessionCreateResponse(reply)=>{assert!(!sftp);assert!(reply.session.is_none());assert_eq!(reply.failure_code,cshell_ipc::SessionFailureCode::AuthenticationCancelled as i32);},
            envelope::Payload::SftpResponse(reply)=>{assert!(sftp);assert_eq!(reply.status,SftpStatus::Cancelled as i32);},
            _=>panic!("wrong response"),
        }
        assert!(f.service.ssh_sessions.list().is_empty());assert!(f.service.ssh_sessions.get_sftp(session).is_err());
        assert_eq!(f.passwords.load(Ordering::SeqCst),0,"cancelling OTP must never continue to password");
    }
    let live=f.create().await;assert!(live.running,"explicit retry must still authenticate normally");
    f.close(id(&live)).await;
}
fn text(session: &SshSession) -> String {
    session
        .history_snapshots()
        .latest()
        .map(|frame| frame.cells.iter().map(|c| c.character).collect())
        .unwrap_or_default()
}
async fn wait(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn echo(session: &SshSession, command: &str) {
    session
        .send_input(&InputAction::Text(format!("{command}\n")))
        .unwrap();
    wait(|| text(session).contains(&format!("ACK:{command}"))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shared_channel_and_sftp_reuse_mfa_and_close_independently() {
    let f = Fixture::new(None).await;
    let original = f.create().await;
    let source = id(&original);
    f.counts(1);
    let duplicate = f.duplicate(source).await;
    let copy = id(&duplicate
        .session
        .unwrap_or_else(|| panic!("{}", duplicate.detail)));
    assert_ne!(source, copy);
    let original = f.service.ssh_sessions.get(source).unwrap();
    let other = f.service.ssh_sessions.get(copy).unwrap();
    echo(&original, "original-input").await;
    echo(&other, "copy-input").await;
    assert!(!text(&original).contains("copy-input"));
    assert!(!text(&other).contains("original-input"));
    let observer = original.fork_transport().unwrap();
    let client = observer.client();
    observer.release().await;
    let sftp = SessionId::new();
    let reply = f
        .sftp(SftpOperation::ConnectProfile, sftp, Some(source))
        .await;
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    let info: cshell_application::SftpConnectionInfo =
        serde_json::from_slice(&reply.document_json).unwrap();
    assert_eq!(info.shared_from, Some(source));
    assert_eq!(info.peer, original.clipboard_trust().unwrap().clone());
    f.counts(1);
    echo(&original, "while-sftp-open").await;
    f.close(source).await;
    echo(&other, "after-original-close").await;
    assert!(!client.transport_closed());
    let listing = f
        .service
        .sftp
        .handle(
            SftpRequest {
                operation: SftpOperation::List as i32,
                session_id: sftp.as_uuid().as_bytes().to_vec(),
                remote_path: "/".into(),
                max_entries: 64,
                ..Default::default()
            },
            &f.service.ssh_sessions,
        )
        .await;
    assert_eq!(listing.status, SftpStatus::Ok as i32, "{}", listing.detail);
    assert_eq!(
        f.sftp(SftpOperation::Disconnect, sftp, None).await.status,
        SftpStatus::Ok as i32
    );
    echo(&other, "after-sftp-close").await;
    let last_sftp = SessionId::new();
    assert_eq!(
        f.sftp(SftpOperation::ConnectProfile, last_sftp, Some(copy))
            .await
            .status,
        SftpStatus::Ok as i32
    );
    f.close(copy).await;
    assert!(!client.transport_closed());
    assert_eq!(
        f.sftp(SftpOperation::ConnectionStatus, last_sftp, None)
            .await
            .status,
        SftpStatus::Ok as i32
    );
    assert_eq!(
        f.sftp(SftpOperation::Disconnect, last_sftp, None)
            .await
            .status,
        SftpStatus::Ok as i32
    );
    wait(|| client.transport_closed()).await;
    f.counts(1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn independent_session_requires_new_mfa_while_channel_copy_does_not() {
    let f = Fixture::new(None).await;
    let first = id(&f.create().await);
    let independent = id(&f.create().await);
    f.counts(2);
    let copy = id(&f.duplicate(first).await.session.unwrap());
    f.counts(2);
    f.close(independent).await;
    echo(
        &f.service.ssh_sessions.get(first).unwrap(),
        "independent-close",
    )
    .await;
    f.close(first).await;
    echo(
        &f.service.ssh_sessions.get(copy).unwrap(),
        "shared-copy-remains",
    )
    .await;
    f.close(copy).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn invalid_source_and_channel_denial_never_reauthenticate_or_damage_source() {
    let f = Fixture::new(Some(1)).await;
    let source = id(&f.create().await);
    assert!(f.duplicate(SessionId::new()).await.session.is_none());
    let denied = f.duplicate(source).await;
    assert!(denied.session.is_none());
    assert!(!denied.detail.is_empty());
    let original = f.service.ssh_sessions.get(source).unwrap();
    echo(&original, "after-channel-denied").await;
    let request = SessionCreateRequest {
        rows: 24,
        cols: 80,
        profile_id: Some(f.profile.as_uuid().as_bytes().to_vec()),
        source_ssh_session_id: Some(source.as_uuid().as_bytes().to_vec()),
        ..Default::default()
    };
    let envelope::Payload::SessionCreateResponse(invalid) = f
        .call(envelope::Payload::SessionCreateRequest(request))
        .await
    else {
        panic!()
    };
    assert!(invalid.session.is_none());
    let invalid = cshell_application::SftpConnectRequest {
        profile: ProfileId::new(),
        source_ssh_session: Some(source),
        discover_directory:true,
    };
    let envelope::Payload::SftpResponse(reply) = f
        .call(envelope::Payload::SftpRequest(SftpRequest {
            operation: SftpOperation::ConnectProfile as i32,
            session_id: SessionId::new().as_uuid().as_bytes().to_vec(),
            document_json: serde_json::to_vec(&invalid).unwrap(),
            ..Default::default()
        }))
        .await
    else {
        panic!()
    };
    assert_eq!(reply.status, SftpStatus::InvalidRequest as i32);
    let sftp = SessionId::new();
    assert_eq!(
        f.sftp(SftpOperation::ConnectProfile, sftp, Some(source))
            .await
            .status,
        SftpStatus::Ok as i32
    );
    assert_eq!(
        f.sftp(SftpOperation::Disconnect, sftp, None).await.status,
        SftpStatus::Ok as i32
    );
    f.close(source).await;
    assert!(f.duplicate(source).await.session.is_none());
    f.counts(1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shared_transport_is_bounded_and_remote_exit_keeps_other_shell_alive() {
    let f = Fixture::new(None).await;
    let source = id(&f.create().await);
    let copy = id(&f.duplicate(source).await.session.unwrap());
    let original = f.service.ssh_sessions.get(source).unwrap();
    let mut leases = Vec::new();
    loop {
        match original.fork_transport() {
            Ok(lease) => leases.push(lease),
            Err(error) => {
                assert!(error.to_string().contains("64"));
                break;
            }
        }
    }
    assert_eq!(leases.len(), 62);
    drop(leases);
    let lease = original.fork_transport().unwrap();
    lease.release().await;
    lease.release().await;
    original
        .send_input(&InputAction::Text("exit\n".into()))
        .unwrap();
    wait(|| !original.running()).await;
    let other = f.service.ssh_sessions.get(copy).unwrap();
    echo(&other, "after-remote-exit").await;
    let third_id=id(&f.duplicate(copy).await.session.unwrap());
    let third=f.service.ssh_sessions.get(third_id).unwrap();
    let observer=other.fork_transport().unwrap();let client=observer.client();observer.release().await;
    client.disconnect().await.unwrap();
    wait(|| !other.running() && !third.running()).await;
    f.counts(1);
    f.close(source).await;
    f.close(copy).await;
    f.close(third_id).await;
}

#[tokio::test(flavor="multi_thread",worker_threads=3)]
async fn sftp_initial_directory_uses_exact_source_shell_and_independent_home() {
    let f=Fixture::new(None).await;
    let original=f.create().await;let source=id(&original);
    let first=f.service.ssh_sessions.get(source).unwrap();
    echo(&first,"cd /srv/one").await;
    let copy=id(&f.duplicate(source).await.session.unwrap());
    let second=f.service.ssh_sessions.get(copy).unwrap();
    echo(&second,"cd /srv/two").await;
    assert_eq!(first.current_remote_directory().await.as_deref(),Some("/srv/one"));
    assert_eq!(second.current_remote_directory().await.as_deref(),Some("/srv/two"));
    for (session,directory) in [(source,"/srv/one"),(copy,"/srv/two")] {
        let file_id=SessionId::new();
        let reply=f.sftp(SftpOperation::ConnectProfile,file_id,Some(session)).await;
        assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
        let info:cshell_application::SftpConnectionInfo=serde_json::from_slice(&reply.document_json).unwrap();
        assert_eq!(info.initial_directory.as_deref(),Some(directory));
        f.sftp(SftpOperation::Disconnect,file_id,None).await;
    }
    f.counts(1);
    let client=first.open_sftp().await.unwrap();
    assert_eq!(client.resolve_path(".").await.unwrap(),"/home/cshell");
    client.close().await.unwrap();
    f.close(source).await;f.close(copy).await;
}

async fn upload_call(f:&Fixture,source:SessionId,id:SessionId,operation:SftpOperation,options:Option<&cshell_application::TerminalUploadStart>)->cshell_application::TerminalUploadSnapshot {
    let envelope::Payload::SftpResponse(reply)=f.call(envelope::Payload::SftpRequest(SftpRequest{operation:operation as i32,session_id:source.as_uuid().as_bytes().to_vec(),transfer_id:id.as_uuid().as_bytes().to_vec(),confirmed:options.is_some(),document_json:options.map(|o|serde_json::to_vec(o).unwrap()).unwrap_or_default(),..Default::default()})).await else{panic!()};
    assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    serde_json::from_slice(&reply.document_json).unwrap()
}
async fn upload_wait(f:&Fixture,source:SessionId,id:SessionId,ready:bool)->cshell_application::TerminalUploadSnapshot {
    tokio::time::timeout(Duration::from_secs(80),async {loop{
        let view=upload_call(f,source,id,SftpOperation::TerminalUploadGet,None).await;
        if view.state.finished() || ready && view.state==cshell_application::TerminalUploadState::Ready {return view;}
        tokio::time::sleep(Duration::from_millis(30)).await;
    }}).await.unwrap()
}
async fn tree_call(f:&Fixture,source:SessionId,id:SessionId,op:SftpOperation,options:Option<&cshell_application::SftpTreeStart>)->cshell_application::SftpTreeSnapshot {
    let envelope::Payload::SftpResponse(reply)=f.call(envelope::Payload::SftpRequest(SftpRequest{operation:op as i32,session_id:source.as_uuid().as_bytes().to_vec(),transfer_id:id.as_uuid().as_bytes().to_vec(),confirmed:op!=SftpOperation::TreeGet,document_json:options.map(|o|serde_json::to_vec(o).unwrap()).unwrap_or_default(),..Default::default()})).await else{panic!()};
    assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);serde_json::from_slice(&reply.document_json).unwrap()
}
async fn tree_wait(f:&Fixture,source:SessionId,id:SessionId)->cshell_application::SftpTreeSnapshot {
    tokio::time::timeout(Duration::from_secs(80),async{loop{let v=tree_call(f,source,id,SftpOperation::TreeGet,None).await;if v.state.finished(){return v;}tokio::time::sleep(Duration::from_millis(30)).await;}}).await.unwrap()
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires isolated CSHELL_RUNTIME_DIR"]
async fn sftp_tree_pause_cancel_and_retry_known_conflicts_keep_completed_files(){
    use cshell_application::{SftpTreeStart,SftpQueueDirection as D,TerminalUploadState as S};
    let f=Fixture::new(None).await;let source=id(&f.create().await);let shell=f.service.ssh_sessions.get(source).unwrap();
    let folder=f._root.path().join("tree-controls");std::fs::create_dir(&folder).unwrap();std::fs::write(folder.join("file.txt"),b"tree-controls-content").unwrap();
    let options=SftpTreeStart{direction:D::Upload,paths:vec![folder.to_str().unwrap().into()],destination:"/srv/one".into(),verification:Default::default()};
    let task=SessionId::new();tree_call(&f,source,task,SftpOperation::TreeStart,Some(&options)).await;tree_call(&f,source,task,SftpOperation::TreePause,None).await;
    tokio::time::timeout(Duration::from_secs(5),async{loop{if tree_call(&f,source,task,SftpOperation::TreeGet,None).await.state==S::Paused{break;}tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap();
    tree_call(&f,source,task,SftpOperation::TreeCancel,None).await;assert_eq!(tree_wait(&f,source,task).await.state,S::Cancelled);
    let original=SessionId::new();tree_call(&f,source,original,SftpOperation::TreeStart,Some(&options)).await;assert_eq!(tree_wait(&f,source,original).await.state,S::Succeeded);
    let conflict=SessionId::new();tree_call(&f,source,conflict,SftpOperation::TreeStart,Some(&options)).await;let failed=tree_wait(&f,source,conflict).await;assert_eq!(failed.state,S::Failed);assert!(failed.retryable,"{}",failed.detail);
    let client=shell.open_sftp().await.unwrap();assert_eq!(client.metadata("/srv/one/tree-controls/file.txt",cshell_sftp::SymlinkPolicy::NoFollow).await.unwrap().size,Some(21));client.remove_entry("/srv/one/tree-controls/file.txt",false).await.unwrap();client.close().await.unwrap();
    tree_call(&f,source,conflict,SftpOperation::TreeRetry,None).await;let done=tree_wait(&f,source,conflict).await;assert_eq!(done.state,S::Succeeded,"{}",done.detail);assert_eq!(done.completed,1);
    f.counts(1);echo(&shell,"after-tree-controls").await;f.close(source).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires isolated CSHELL_RUNTIME_DIR"]
async fn sftp_tree_roundtrip_preserves_empty_directories_rolls_batches_and_never_reauthenticates(){
    use cshell_application::{SftpTreeStart,SftpQueueDirection as D,TerminalUploadState as S};
    let f=Fixture::new(None).await;let original=id(&f.create().await);let source=SessionId::new();
    assert_eq!(f.sftp(SftpOperation::ConnectProfile,source,Some(original)).await.status,SftpStatus::Ok as i32);
    let folder=f._root.path().join("tree-upload");std::fs::create_dir_all(folder.join("nested/empty")).unwrap();for n in 0..5{std::fs::write(folder.join(format!("nested/file-{n}")),format!("tree-content-{n}")).unwrap();}
    let upload=SftpTreeStart{direction:D::Upload,paths:vec![folder.to_str().unwrap().into()],destination:"/srv/one".into(),verification:Default::default()};
    let task=SessionId::new();tree_call(&f,source,task,SftpOperation::TreeStart,Some(&upload)).await;let result=tree_wait(&f,source,task).await;assert_eq!(result.state,S::Succeeded,"{}",result.detail);assert_eq!(result.completed,5);
    let local=f._root.path().join("download-target");std::fs::create_dir(&local).unwrap();let download=SftpTreeStart{direction:D::Download,paths:vec!["/srv/one/tree-upload".into()],destination:local.to_str().unwrap().into(),verification:Default::default()};
    let task=SessionId::new();tree_call(&f,source,task,SftpOperation::TreeStart,Some(&download)).await;let result=tree_wait(&f,source,task).await;assert_eq!(result.state,S::Succeeded,"{}",result.detail);assert_eq!(std::fs::read(local.join("tree-upload/nested/file-4")).unwrap(),b"tree-content-4");assert!(local.join("tree-upload/nested/empty").is_dir());
    f.close(original).await;f.counts(1);assert!(f.service.ssh_sessions.get_sftp(source).unwrap().running());f.sftp(SftpOperation::Disconnect,source,None).await;
}

async fn sync_call(f:&Fixture,source:SessionId,id:SessionId,operation:SftpOperation,data:Vec<u8>)->SftpResponse {
    let envelope::Payload::SftpResponse(reply)=f.call(envelope::Payload::SftpRequest(SftpRequest{operation:operation as i32,session_id:source.as_uuid().as_bytes().to_vec(),transfer_id:id.as_uuid().as_bytes().to_vec(),confirmed:operation==SftpOperation::SyncApply,document_json:data,..Default::default()})).await else{panic!()};reply
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires isolated CSHELL_RUNTIME_DIR"]
async fn sftp_sync_preview_roundtrip_rejects_changed_and_replayed_plans_without_reauth(){
    use cshell_application::{SftpSyncOptions,SftpSyncApply,SftpSyncPreview,SftpQueueDirection as D,TerminalUploadState as S};
    let f=Fixture::new(None).await;let original=id(&f.create().await);let source=SessionId::new();
    assert_eq!(f.sftp(SftpOperation::ConnectProfile,source,Some(original)).await.status,SftpStatus::Ok as i32);
    let shell=f.service.ssh_sessions.get(original).unwrap();let client=shell.open_sftp().await.unwrap();
    client.create_directory("/srv/one/maturity-sync",0o755).await.unwrap();client.close().await.unwrap();
    let local=f._root.path().join("sync-source");std::fs::create_dir_all(local.join("nested/empty")).unwrap();std::fs::write(local.join("nested/a.txt"),b"first-content").unwrap();
    let options=SftpSyncOptions{local:local.to_str().unwrap().into(),remote:"/srv/one/maturity-sync".into(),direction:D::Upload,verification:Default::default(),compare_content:true,excludes:Vec::new(),preserve_modified:false};
    let task=SessionId::new();let reply=sync_call(&f,source,task,SftpOperation::SyncScan,serde_json::to_vec(&options).unwrap()).await;
    assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);let preview:SftpSyncPreview=serde_json::from_slice(&reply.document_json).unwrap();
    let apply=SftpSyncApply{selected:preview.entries.iter().filter(|e|e.eligible(D::Upload,false)).map(|e|e.relative.clone()).collect(),backup_replace:false};
    let data=serde_json::to_vec(&apply).unwrap();let reply=sync_call(&f,source,task,SftpOperation::SyncApply,data.clone()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    let result=tree_wait(&f,source,task).await;assert_eq!(result.state,S::Succeeded,"{}",result.detail);assert_eq!(result.completed,1);assert_eq!(result.total_bytes,13);
    assert_ne!(sync_call(&f,source,task,SftpOperation::SyncApply,data).await.status,SftpStatus::Ok as i32);
    std::fs::File::options().write(true).open(local.join("nested/a.txt")).unwrap().set_modified(std::time::UNIX_EPOCH+Duration::from_secs(10)).unwrap();
    for compare_content in [false,true] {
        let checked_options=SftpSyncOptions{compare_content,..options.clone()};let check=SessionId::new();
        let reply=sync_call(&f,source,check,SftpOperation::SyncScan,serde_json::to_vec(&checked_options).unwrap()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
        let preview:SftpSyncPreview=serde_json::from_slice(&reply.document_json).unwrap();let file=preview.entries.iter().find(|e|e.relative=="nested/a.txt").unwrap();
        assert_eq!(file.difference,if compare_content{cshell_application::SftpSyncDifference::ContentMatch}else{cshell_application::SftpSyncDifference::Changed});
        if compare_content{assert!(!file.eligible(D::Upload,true));}
    }
    std::fs::write(local.join("nested/a.txt"),b"other-content").unwrap();
    std::fs::File::options().write(true).open(local.join("nested/a.txt")).unwrap().set_modified(std::time::UNIX_EPOCH+Duration::from_secs(10)).unwrap();
    let check=SessionId::new();let reply=sync_call(&f,source,check,SftpOperation::SyncScan,serde_json::to_vec(&options).unwrap()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    let preview:SftpSyncPreview=serde_json::from_slice(&reply.document_json).unwrap();assert_eq!(preview.entries.iter().find(|e|e.relative=="nested/a.txt").unwrap().difference,cshell_application::SftpSyncDifference::ContentChanged);
    std::fs::write(local.join("nested/a.txt"),b"first-content").unwrap();
    std::fs::File::options().write(true).open(local.join("nested/a.txt")).unwrap().set_modified(std::time::UNIX_EPOCH+Duration::from_secs(10)).unwrap();
    let rejected=SftpSyncApply{selected:vec!["nested/a.txt".into()],backup_replace:true};
    let reply=sync_call(&f,source,check,SftpOperation::SyncApply,serde_json::to_vec(&rejected).unwrap()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    let guarded=tree_wait(&f,source,check).await;assert_eq!(guarded.state,S::Failed,"changed content must not be replayed with old digest: {}",guarded.detail);assert_eq!(guarded.completed,0);

    let download=f._root.path().join("sync-download");std::fs::create_dir(&download).unwrap();let options=SftpSyncOptions{local:download.to_str().unwrap().into(),direction:D::Download,..options.clone()};
    let task=SessionId::new();let reply=sync_call(&f,source,task,SftpOperation::SyncScan,serde_json::to_vec(&options).unwrap()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    let preview:SftpSyncPreview=serde_json::from_slice(&reply.document_json).unwrap();let apply=SftpSyncApply{selected:preview.entries.iter().filter(|e|e.eligible(D::Download,false)).map(|e|e.relative.clone()).collect(),backup_replace:false};
    assert_eq!(sync_call(&f,source,task,SftpOperation::SyncApply,serde_json::to_vec(&apply).unwrap()).await.status,SftpStatus::Ok as i32);
    let result=tree_wait(&f,source,task).await;assert_eq!(result.state,S::Succeeded,"{}",result.detail);assert_eq!(std::fs::read(download.join("nested/a.txt")).unwrap(),b"first-content");assert!(download.join("nested/empty").is_dir());
    let task=SessionId::new();let reply=sync_call(&f,source,task,SftpOperation::SyncScan,serde_json::to_vec(&options).unwrap()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    std::fs::write(download.join("nested/a.txt"),b"external modification after preview").unwrap();
    let apply=SftpSyncApply{selected:vec!["nested/a.txt".into()],backup_replace:true};assert_ne!(sync_call(&f,source,task,SftpOperation::SyncApply,serde_json::to_vec(&apply).unwrap()).await.status,SftpStatus::Ok as i32);assert_eq!(std::fs::read(download.join("nested/a.txt")).unwrap(),b"external modification after preview");
    let task=SessionId::new();let reply=sync_call(&f,source,task,SftpOperation::SyncScan,serde_json::to_vec(&options).unwrap()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    let apply=SftpSyncApply{selected:vec!["nested/a.txt".into()],backup_replace:true};
    let reply=sync_call(&f,source,task,SftpOperation::SyncApply,serde_json::to_vec(&apply).unwrap()).await;assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
    let result=tree_wait(&f,source,task).await;assert_eq!(result.state,S::Succeeded,"{}",result.detail);assert_eq!(std::fs::read(download.join("nested/a.txt")).unwrap(),b"first-content");
    assert!(std::fs::read_dir(download.join("nested")).unwrap().flatten().any(|e|e.file_name()!="a.txt"&&std::fs::read(e.path()).is_ok_and(|v|v==b"external modification after preview")),"original destination backup must survive");
    f.counts(1);f.sftp(SftpOperation::Disconnect,source,None).await;f.close(original).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires isolated CSHELL_RUNTIME_DIR"]
async fn terminal_drop_upload_preserves_folders_batches_and_current_directory_without_mfa() {
    use cshell_application::{TerminalUploadProtocol as Protocol,TerminalUploadStart as Start,TerminalUploadState as State};
    let f=Fixture::new(None).await;let source=id(&f.create().await);let shell=f.service.ssh_sessions.get(source).unwrap();
    echo(&shell,"cd /srv/one").await;
    let local=f._root.path().join("拖放文件夹");std::fs::create_dir_all(local.join("nested/empty")).unwrap();
    for index in 0..5 {std::fs::write(local.join(format!("nested/file-{index:03}")),format!("data-{index}")).unwrap();}
    let options=Start{paths:vec![local.to_str().unwrap().into()],protocol:Protocol::Zmodem,verification:Default::default(),confirm_before_start:false};
    let task=SessionId::new();upload_call(&f,source,task,SftpOperation::TerminalUploadStart,Some(&options)).await;
    let result=upload_wait(&f,source,task,false).await;
    assert_eq!(result.state,State::Succeeded,"{}",result.detail);assert_eq!(result.protocol,Protocol::Sftp);assert_eq!(result.directory,"/srv/one");assert_eq!(result.completed,5);
    let client=shell.open_sftp().await.unwrap();
    assert!(client.directory_path("/srv/one/拖放文件夹/nested/empty",false).await.is_ok());
    assert_eq!(client.metadata("/srv/one/拖放文件夹/nested/file-004",cshell_sftp::SymlinkPolicy::NoFollow).await.unwrap().size,Some(6));
    assert!(client.metadata("/home/cshell/拖放文件夹/nested/file-004",cshell_sftp::SymlinkPolicy::NoFollow).await.is_err());
    client.close().await.unwrap();echo(&shell,"after-drop-upload").await;f.counts(1);f.close(source).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires isolated CSHELL_RUNTIME_DIR"]
async fn terminal_drop_confirmation_cancel_conflicts_and_zmodem_keep_source_terminal_usable() {
    use cshell_application::{TerminalUploadProtocol as Protocol,TerminalUploadStart as Start,TerminalUploadState as State};
    let f=Fixture::new(None).await;let source=id(&f.create().await);let shell=f.service.ssh_sessions.get(source).unwrap();echo(&shell,"cd /srv/two").await;
    let file=f._root.path().join("terminal-drop.bin");std::fs::write(&file,b"original-content").unwrap();
    let mut options=Start{paths:vec![file.to_str().unwrap().into()],protocol:Protocol::Sftp,verification:Default::default(),confirm_before_start:true};
    let cancelled=SessionId::new();upload_call(&f,source,cancelled,SftpOperation::TerminalUploadStart,Some(&options)).await;
    let prepared=upload_wait(&f,source,cancelled,true).await;assert_eq!(prepared.state,State::Ready);assert_eq!(prepared.directory,"/srv/two");
    upload_call(&f,source,cancelled,SftpOperation::TerminalUploadCancel,None).await;
    assert_eq!(upload_wait(&f,source,cancelled,false).await.state,State::Cancelled);
    let client=shell.open_sftp().await.unwrap();assert!(client.metadata("/srv/two/terminal-drop.bin",cshell_sftp::SymlinkPolicy::NoFollow).await.is_err());client.close().await.unwrap();
    let task=SessionId::new();upload_call(&f,source,task,SftpOperation::TerminalUploadStart,Some(&options)).await;assert_eq!(upload_wait(&f,source,task,true).await.state,State::Ready);
    echo(&shell,"cd /srv/one").await;
    upload_call(&f,source,task,SftpOperation::TerminalUploadContinue,None).await;
    let result=upload_wait(&f,source,task,false).await;assert_eq!(result.state,State::Succeeded,"{}",result.detail);assert_eq!(result.directory,"/srv/two");
    echo(&shell,"cd /srv/two").await;std::fs::write(&file,b"changed-local-content").unwrap();options.confirm_before_start=false;
    let conflict=SessionId::new();upload_call(&f,source,conflict,SftpOperation::TerminalUploadStart,Some(&options)).await;
    assert_eq!(upload_wait(&f,source,conflict,false).await.state,State::Failed);
    let client=shell.open_sftp().await.unwrap();assert_eq!(client.metadata("/srv/two/terminal-drop.bin",cshell_sftp::SymlinkPolicy::NoFollow).await.unwrap().size,Some(16));client.close().await.unwrap();
    let zfile=f._root.path().join("zmodem-drop.bin");std::fs::write(&zfile,vec![0x8a;100_000]).unwrap();options.paths=vec![zfile.to_str().unwrap().into()];options.protocol=Protocol::Zmodem;
    let ztask=SessionId::new();upload_call(&f,source,ztask,SftpOperation::TerminalUploadStart,Some(&options)).await;
    let result=upload_wait(&f,source,ztask,false).await;assert_eq!(result.state,State::Succeeded,"{}",result.detail);assert_eq!(result.protocol,Protocol::Zmodem);
    let client=shell.open_sftp().await.unwrap();assert_eq!(client.metadata("/srv/two/zmodem-drop.bin",cshell_sftp::SymlinkPolicy::NoFollow).await.unwrap().size,Some(100_000));client.close().await.unwrap();
    echo(&shell,"after-zmodem-drop").await;f.counts(1);f.close(source).await;
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires isolated CSHELL_RUNTIME_DIR"]
async fn terminal_drop_pause_cancel_source_close_and_request_identity_are_enforced() {
    use cshell_application::{TerminalUploadProtocol as Protocol,TerminalUploadStart as Start,TerminalUploadState as State};
    let f=Fixture::new(None).await;let source=id(&f.create().await);let shell=f.service.ssh_sessions.get(source).unwrap();echo(&shell,"cd /srv/one").await;
    let folder=f._root.path().join("pause-case");std::fs::create_dir(&folder).unwrap();
    for n in 0..4 {std::fs::write(folder.join(format!("part-{n}")),vec![0x61;1_000_000]).unwrap();}
    let options=Start{paths:vec![folder.to_str().unwrap().into()],protocol:Protocol::Sftp,verification:Default::default(),confirm_before_start:true};
    let task=SessionId::new();upload_call(&f,source,task,SftpOperation::TerminalUploadStart,Some(&options)).await;assert_eq!(upload_wait(&f,source,task,true).await.state,State::Ready);
    for (request,status) in [
        (SftpRequest{operation:SftpOperation::TerminalUploadGet as i32,session_id:SessionId::new().as_uuid().as_bytes().to_vec(),transfer_id:task.as_uuid().as_bytes().to_vec(),..Default::default()},SftpStatus::Unavailable),
        (SftpRequest{operation:SftpOperation::TerminalUploadStart as i32,session_id:source.as_uuid().as_bytes().to_vec(),transfer_id:task.as_uuid().as_bytes().to_vec(),confirmed:true,document_json:serde_json::to_vec(&options).unwrap(),..Default::default()},SftpStatus::Conflict),
        (SftpRequest{operation:SftpOperation::TerminalUploadGet as i32,session_id:source.as_uuid().as_bytes().to_vec(),transfer_id:task.as_uuid().as_bytes().to_vec(),remote_path:"/unexpected".into(),..Default::default()},SftpStatus::InvalidRequest)
    ] {assert_eq!(f.service.sftp.handle(request,&f.service.ssh_sessions).await.status,status as i32);}
    upload_call(&f,source,task,SftpOperation::TerminalUploadContinue,None).await;
    upload_call(&f,source,task,SftpOperation::TerminalUploadPause,None).await;
    tokio::time::timeout(Duration::from_secs(12),async {loop{let s=upload_call(&f,source,task,SftpOperation::TerminalUploadGet,None).await;assert!(!s.state.finished(),"{}",s.detail);if s.state==State::Paused{break;}tokio::time::sleep(Duration::from_millis(50)).await;}}).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;let paused=upload_call(&f,source,task,SftpOperation::TerminalUploadGet,None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;let still=upload_call(&f,source,task,SftpOperation::TerminalUploadGet,None).await;assert_eq!(still.state,State::Paused);assert_eq!(still.bytes,paused.bytes);
    upload_call(&f,source,task,SftpOperation::TerminalUploadContinue,None).await;
    tokio::time::timeout(Duration::from_secs(12),async {loop{let s=upload_call(&f,source,task,SftpOperation::TerminalUploadGet,None).await;if s.state!=State::Paused {break;}tokio::time::sleep(Duration::from_millis(30)).await;}}).await.unwrap();
    upload_call(&f,source,task,SftpOperation::TerminalUploadCancel,None).await;assert_eq!(upload_wait(&f,source,task,false).await.state,State::Cancelled);echo(&shell,"after-cancelled-upload").await;
    let waiting=SessionId::new();upload_call(&f,source,waiting,SftpOperation::TerminalUploadStart,Some(&options)).await;assert_eq!(upload_wait(&f,source,waiting,true).await.state,State::Ready);
    f.close(source).await;assert_eq!(upload_wait(&f,source,waiting,false).await.state,State::Cancelled);f.counts(1);
}

async fn vault_connection_prompt(f:&Fixture)->cshell_ipc::KeyboardAuthChallenge {
    tokio::time::timeout(Duration::from_secs(20),async {loop {let envelope::Payload::VaultResponse(r)=f.call(envelope::Payload::VaultRequest(VaultRequest::new(VaultOperation::KeyboardAuthPoll))).await else{panic!()};if let Some(p)=r.auth_challenge{return p;}tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap()
}
async fn vault_connection_answer(f:&Fixture,p:cshell_ipc::KeyboardAuthChallenge,password:&str,cancel:bool){let mut r=VaultRequest::new(VaultOperation::KeyboardAuthRespond);r.auth_answer=Some(KeyboardAuthAnswer{id:p.id,answers:if cancel{vec![]}else{vec![password.into()]},cancel,username:String::new(),save_credentials:false});let envelope::Payload::VaultResponse(r)=f.call(envelope::Payload::VaultRequest(r)).await else{panic!()};assert_eq!(r.status,VaultControlStatus::Ok as i32);}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn vault_protection_unlock_precedes_ssh_and_sftp_mfa_and_never_reaches_the_server() {
    for sftp in [false,true] {
        let f=Fixture::new(None).await;let vault=&f.service.profiles.as_ref().unwrap().vault;vault.lock();let file_id=SessionId::new();
        let payload=if sftp {envelope::Payload::SftpRequest(SftpRequest{operation:SftpOperation::ConnectProfile as i32,session_id:file_id.as_uuid().as_bytes().to_vec(),document_json:serde_json::to_vec(&cshell_application::SftpConnectRequest{profile:f.profile,source_ssh_session:None,discover_directory:true}).unwrap(),..Default::default()})}else{envelope::Payload::SessionCreateRequest(SessionCreateRequest{profile_id:Some(f.profile.as_uuid().as_bytes().to_vec()),rows:24,cols:80,..Default::default()})};
        let service=f.service.clone();let work=tokio::spawn(async move{ipc(&service,payload).await});
        let p=vault_connection_prompt(&f).await;assert!(p.vault_unlock);assert_eq!(f.tcp.load(Ordering::SeqCst),0);vault_connection_answer(&f,p,"shared-ssh-test-master",false).await;
        let otp=vault_connection_prompt(&f).await;assert!(!otp.vault_unlock);assert!(otp.prompts[0].text.contains("Verification"));
        #[cfg(any(windows, target_os="linux", target_os="macos"))]
        vault.observe_desktop_session(cshell_ipc::VaultSessionState::Locked);
        assert!(vault.response(None).unlocked, "locking the desktop must not interrupt pending SSH/SFTP MFA");
        vault_connection_answer(&f,otp,"654321",false).await;
        let result=tokio::time::timeout(Duration::from_secs(20),work).await.unwrap().unwrap();
        if sftp {let envelope::Payload::SftpResponse(r)=result else{panic!()};assert_eq!(r.status,SftpStatus::Ok as i32,"{}",r.detail);f.sftp(SftpOperation::Disconnect,file_id,None).await;}
        else {let envelope::Payload::SessionCreateResponse(r)=result else{panic!()};let source=id(&r.session.unwrap_or_else(||panic!("{}",r.detail)));vault.lock();assert_eq!(f.sftp(SftpOperation::ConnectProfile,file_id,Some(source)).await.status,SftpStatus::Ok as i32);assert!(!vault.response(None).unlocked);f.sftp(SftpOperation::Disconnect,file_id,None).await;f.close(source).await;}
        f.counts(1);
    }
}
#[tokio::test(flavor="multi_thread",worker_threads=3)]
async fn vault_protection_cancelled_sftp_unlock_closes_attempt_without_contacting_server() {
    let f=Fixture::new(None).await;f.service.profiles.as_ref().unwrap().vault.lock();let file_id=SessionId::new();let service=f.service.clone();let profile=f.profile;
    let work=tokio::spawn(async move{ipc(&service,envelope::Payload::SftpRequest(SftpRequest{operation:SftpOperation::ConnectProfile as i32,session_id:file_id.as_uuid().as_bytes().to_vec(),document_json:serde_json::to_vec(&cshell_application::SftpConnectRequest{profile,source_ssh_session:None,discover_directory:true}).unwrap(),..Default::default()})).await});
    let p=vault_connection_prompt(&f).await;assert!(p.vault_unlock);vault_connection_answer(&f,p,"",true).await;let envelope::Payload::SftpResponse(r)=work.await.unwrap() else{panic!()};assert_eq!(r.status,SftpStatus::Cancelled as i32);f.counts(0);
}
