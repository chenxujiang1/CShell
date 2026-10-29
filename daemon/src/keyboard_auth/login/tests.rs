use super::*;
use cshell_application::{CatalogChange, ProfileService};
use cshell_domain::{ProfileId, ProfileKind, ProfileRecord, TerminalOverrides};
use cshell_ipc::{
    Envelope, Handshake, HandshakePolicy, KeyboardAuthAnswer, VaultControlStatus, VaultOperation,
    VaultRequest, client_handshake, envelope, read_envelope, server_handshake, write_envelope,
};
use russh::server::Auth;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
struct PasswordOnly {
    keyboard: Arc<AtomicUsize>,
    attempts: Arc<AtomicUsize>,
}
impl russh::server::Handler for PasswordOnly {
    type Error = russh::Error;
    async fn auth_none(&mut self, _: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: Some((&[russh::MethodKind::Password][..]).into()),
            partial_success: false,
        })
    }
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        Ok(if user == "operator" && password == "correct-password" {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: Some((&[russh::MethodKind::Password][..]).into()),
                partial_success: false,
            }
        })
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        _: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        self.keyboard.fetch_add(1, Ordering::Relaxed);
        Ok(Auth::reject())
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    profiles: Arc<crate::ProfileIpcService>,
    service: crate::SessionIpcService,
    target: SshConnectionRecord,
    known: PathBuf,
    server: tokio::task::JoinHandle<()>,
    keyboard: Arc<AtomicUsize>,
    attempts: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(username: &str, saved: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let hostkey = russh::keys::ssh_key::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let public = hostkey.public_key().to_openssh().unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![hostkey],
            auth_rejection_time: Duration::from_millis(1),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keyboard = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));
        let k = keyboard.clone();
        let a = attempts.clone();
        let server = tokio::spawn(async move {
            let mut children = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let config = config.clone();
                let handler = PasswordOnly {
                    keyboard: k.clone(),
                    attempts: a.clone(),
                };
                children.spawn(async move {
                    let connection = russh::server::run_stream(config, stream, handler)
                        .await
                        .unwrap();
                    let _ = connection.await;
                });
            }
        });
        let id = ProfileId::new();
        let target = SshConnectionRecord {
            profile_id: id,
            host: "127.0.0.1".into(),
            port: address.port(),
            username: username.into(),
            auth_method: SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: Default::default(),
            agent_identity: None,
            route: Default::default(),
            options: cshell_domain::SshConnectionOptions {
                authentication_order: vec![SshAuthStep::KeyboardInteractive, SshAuthStep::Password],
                ..Default::default()
            },
        };
        let repository =
            cshell_storage::SqliteProfileRepository::open(root.path().join("profiles.db"))
                .await
                .unwrap();
        let service = ProfileService::new(repository);
        service
            .apply_batch(
                0,
                &[
                    CatalogChange::UpsertProfile(ProfileRecord {
                        id,
                        name: "interactive login".into(),
                        kind: ProfileKind::Ssh,
                        folder_id: None,
                        tags: Default::default(),
                        favorite: false,
                        terminal: TerminalOverrides::default(),
                    }),
                    CatalogChange::UpsertSshConnection(target.clone()),
                ],
            )
            .await
            .unwrap();
        let profiles = Arc::new(crate::ProfileIpcService::new(service.into_repository()));
        let vault = &profiles.vault;
        let master = Secret::new(b"test-master-password".to_vec());
        vault
            .create(&master, vault.response(None).lock_generation)
            .unwrap();
        vault
            .unlock(&master, vault.response(None).lock_generation)
            .unwrap();
        vault.set_mode(id, 0, true).unwrap();
        if let Some(password) = saved {
            vault
                .edit(
                    &target,
                    Kind::Password,
                    Some(&Secret::new(password.as_bytes().to_vec())),
                )
                .unwrap();
        }
        let known = root.path().join("known_hosts");
        std::fs::write(&known, format!("[127.0.0.1]:{} {public}\n", address.port())).unwrap();
        let service = crate::SessionIpcService::new(Arc::new(
            crate::LocalSessionRegistry::new(root.path().join("journals"), 16).unwrap(),
        ))
        .with_profiles(profiles.clone());
        Self {
            _root: root,
            profiles,
            service,
            target,
            known,
            server,
            keyboard,
            attempts,
        }
    }
    async fn connect_with_frontend(
        &self,
        target: SshConnectionRecord,
    ) -> tokio::task::JoinHandle<
        Result<crate::ssh_route::RoutedSshConnection, crate::session_ipc::SshLaunchFailure>,
    > {
        // Register the interactive frontend before starting authentication.
        // A fast empty-account path must not race the first prompt poll.
        self.call(VaultRequest::new(VaultOperation::KeyboardAuthPoll))
            .await;
        self.connect(target)
    }
    fn connect(
        &self,
        target: SshConnectionRecord,
    ) -> tokio::task::JoinHandle<
        Result<crate::ssh_route::RoutedSshConnection, crate::session_ipc::SshLaunchFailure>,
    > {
        let vault = self.profiles.vault.clone();
        let known = self.known.clone();
        tokio::spawn(async move {
            crate::ssh_route::connect_profile(
                &crate::ssh_route::SshConnectionPlan {
                    title: "interactive login".into(),
                    target,
                    jumps: vec![],
                },
                &known,
                &vault,
            )
            .await
        })
    }
    async fn call(&self, request: VaultRequest) -> cshell_ipc::VaultResponse {
        let (mut client, mut server) = tokio::io::duplex(65536);
        let service = self.service.clone();
        let job = tokio::spawn(async move {
            server_handshake(
                &mut server,
                &HandshakePolicy::with_instance_id([19; 32], [21; 16], u64::MAX),
            )
            .await
            .unwrap();
            service.serve_one(&mut server).await.unwrap();
        });
        let mut hello = Handshake::new(vec![21; 16], vec![19; 32]);
        hello.feature_bits = cshell_ipc::features::VAULT_CONTROL;
        client_handshake(&mut client, 1, hello).await.unwrap();
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::VaultRequest(request)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let response = read_envelope(&mut client).await.unwrap();
        job.await.unwrap();
        let Some(envelope::Payload::VaultResponse(response)) = response.payload else {
            panic!()
        };
        response
    }
    async fn prompt(&self) -> KeyboardAuthChallenge {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = self
                    .call(VaultRequest::new(VaultOperation::KeyboardAuthPoll))
                    .await;
                if let Some(c) = response.auth_challenge {
                    return c;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn answer(
        &self,
        challenge: &KeyboardAuthChallenge,
        password: &str,
        save: bool,
        cancel: bool,
    ) {
        let mut request = VaultRequest::new(VaultOperation::KeyboardAuthRespond);
        request.auth_answer = Some(KeyboardAuthAnswer {
            id: challenge.id.clone(),
            username: "operator".into(),
            answers: if cancel {
                vec![]
            } else {
                vec![password.into()]
            },
            save_credentials: save,
            cancel,
        });
        assert_eq!(
            self.call(request).await.status,
            VaultControlStatus::Ok as i32
        );
    }
    async fn saved_target(&self) -> SshConnectionRecord {
        self.profiles
            .service
            .load()
            .await
            .unwrap()
            .snapshot()
            .ssh_connections
            .into_iter()
            .find(|c| c.profile_id == self.target.profile_id)
            .unwrap()
    }
}
async fn connected(
    job: tokio::task::JoinHandle<
        Result<crate::ssh_route::RoutedSshConnection, crate::session_ipc::SshLaunchFailure>,
    >,
) -> crate::ssh_route::RoutedSshConnection {
    tokio::time::timeout(Duration::from_secs(5), job)
        .await
        .unwrap()
        .unwrap()
        .unwrap_or_else(|e| panic!("{}", e.detail))
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn login_empty_account_wrong_password_retry_saves_only_verified_credentials_and_reuses_them()
{
    let fixture = Fixture::new("", None).await;
    let epoch = *fixture.profiles.vault.cancellation().borrow();
    let job = fixture.connect_with_frontend(fixture.target.clone()).await;
    let first = fixture.prompt().await;
    assert!(first.login && first.username_editable);
    fixture.answer(&first, "wrong-password", true, false).await;
    let retry = fixture.prompt().await;
    assert!(retry.login && retry.rejected && !retry.username_editable);
    assert_eq!(retry.username, "operator");
    assert_eq!(fixture.saved_target().await.username, "");
    assert!(matches!(
        fixture.profiles.vault.read(&fixture.target, Kind::Password),
        Err(cshell_vault::VaultError::Missing)
    ));
    fixture
        .answer(&retry, "correct-password", true, false)
        .await;
    let connection = connected(job).await;
    assert_eq!(connection.client.authenticated_username(), Some("operator"));
    let original = crate::ssh_route::SshConnectionPlan {
        title: "login".into(),
        target: fixture.target.clone(),
        jumps: vec![],
    };
    let runtime = original.authenticated_plan(&connection.client, connection.jump.as_ref());
    let peer = runtime
        .verified_peer(&connection.client, connection.jump.as_ref())
        .unwrap();
    assert!(!original.matches_captured_peer(&peer));
    assert!(runtime.matches_captured_peer(&peer));
    let mut wrong_account = peer.clone();
    wrong_account.username = "another-account".into();
    assert!(!runtime.matches_captured_peer(&wrong_account));
    connection.client.disconnect().await.unwrap();
    let saved = fixture.saved_target().await;
    assert_eq!(saved.username, "operator");
    assert_eq!(
        fixture
            .profiles
            .vault
            .read(&saved, Kind::Password)
            .unwrap()
            .expose(),
        b"correct-password"
    );
    assert_eq!(
        *fixture.profiles.vault.cancellation().borrow(),
        epoch,
        "saving successful credentials must not cancel its own connection"
    );
    let connection = connected(fixture.connect_with_frontend(saved).await).await;
    connection.client.disconnect().await.unwrap();
    assert_eq!(
        fixture.keyboard.load(Ordering::Relaxed),
        0,
        "server did not advertise keyboard-interactive"
    );
    assert_eq!(fixture.attempts.load(Ordering::Relaxed), 3);
    assert!(
        fixture
            .profiles
            .vault
            .keyboard_auth
            .pending
            .lock()
            .unwrap()
            .is_empty()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn login_without_save_and_cancelled_retry_do_not_persist_any_credentials() {
    let fixture = Fixture::new("", None).await;
    let job = fixture.connect_with_frontend(fixture.target.clone()).await;
    let first = fixture.prompt().await;
    fixture.answer(&first, "", false, false).await;
    let password = fixture.prompt().await;
    assert!(password.password_required && !password.username_editable && !password.rejected);
    fixture
        .answer(&password, "correct-password", false, false)
        .await;
    let connection = connected(job).await;
    connection.client.disconnect().await.unwrap();
    assert_eq!(fixture.saved_target().await.username, "");
    assert!(matches!(
        fixture.profiles.vault.read(&fixture.target, Kind::Password),
        Err(cshell_vault::VaultError::Missing)
    ));
    let job = fixture.connect_with_frontend(fixture.target.clone()).await;
    let first = fixture.prompt().await;
    fixture.answer(&first, "wrong-password", true, false).await;
    let retry = fixture.prompt().await;
    fixture.answer(&retry, "", true, true).await;
    assert!(job.await.unwrap().is_err());
    assert_eq!(fixture.saved_target().await.username, "");
    assert!(matches!(
        fixture.profiles.vault.read(&fixture.target, Kind::Password),
        Err(cshell_vault::VaultError::Missing)
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn login_rejected_saved_password_can_be_corrected_and_replaced_after_success() {
    let fixture = Fixture::new("operator", Some("old-password")).await;
    let job = fixture.connect_with_frontend(fixture.target.clone()).await;
    let retry = fixture.prompt().await;
    assert!(retry.rejected);
    assert_eq!(
        fixture
            .profiles
            .vault
            .read(&fixture.target, Kind::Password)
            .unwrap()
            .expose(),
        b"old-password"
    );
    fixture
        .answer(&retry, "correct-password", true, false)
        .await;
    let connection = connected(job).await;
    connection.client.disconnect().await.unwrap();
    assert_eq!(
        fixture
            .profiles
            .vault
            .read(&fixture.target, Kind::Password)
            .unwrap()
            .expose(),
        b"correct-password"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn login_does_not_overwrite_configuration_edited_while_prompt_is_open() {
    let fixture = Fixture::new("", None).await;
    let job = fixture.connect_with_frontend(fixture.target.clone()).await;
    let first = fixture.prompt().await;
    let snapshot = fixture.profiles.service.load().await.unwrap().snapshot();
    let mut changed = fixture.target.clone();
    changed.host = "another.test".into();
    fixture
        .profiles
        .service
        .apply_batch(
            snapshot.revision,
            &[CatalogChange::UpsertSshConnection(changed.clone())],
        )
        .await
        .unwrap();
    fixture
        .answer(&first, "correct-password", true, false)
        .await;
    let connection = connected(job).await;
    connection.client.disconnect().await.unwrap();
    assert_eq!(fixture.saved_target().await, changed);
    assert!(matches!(
        fixture.profiles.vault.read(&changed, Kind::Password),
        Err(cshell_vault::VaultError::Missing)
    ));
    assert!(
        !fixture
            .call(VaultRequest::new(VaultOperation::KeyboardAuthPoll))
            .await
            .auth_notice
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn login_without_a_frontend_returns_interactive_required_instead_of_hanging() {
    let fixture = Fixture::new("", None).await;
    let failure = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.connect(fixture.target.clone()),
    )
    .await
    .unwrap()
    .unwrap()
    .err()
    .expect("interactive frontend required");
    assert_eq!(
        failure.code,
        cshell_ipc::SessionFailureCode::CredentialUnavailable
    );
    assert!(failure.detail.contains("桌面"), "{}", failure.detail);
    assert!(fixture.saved_target().await.username.is_empty());
    assert!(
        fixture
            .profiles
            .vault
            .keyboard_auth
            .pending
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn login_saving_account_preserves_strictly_bound_portable_key_credentials() {
    let fixture = Fixture::new("", None).await;
    let mut original = fixture.target.clone();
    original
        .options
        .authentication_order
        .push(SshAuthStep::PrivateKey);
    original.private_key_path = Some(
        fixture
            ._root
            .path()
            .join("id_ed25519")
            .to_string_lossy()
            .into_owned(),
    );
    let snapshot = fixture.profiles.service.load().await.unwrap().snapshot();
    fixture
        .profiles
        .service
        .apply_batch(
            snapshot.revision,
            &[CatalogChange::UpsertSshConnection(original.clone())],
        )
        .await
        .unwrap();
    for (kind, bytes) in [
        (Kind::PrivateKey, b"test-portable-key".as_slice()),
        (Kind::KeyPassphrase, b"test-key-phrase".as_slice()),
    ] {
        fixture
            .profiles
            .vault
            .edit(&original, kind, Some(&Secret::new(bytes.to_vec())))
            .unwrap();
    }
    let epoch = *fixture.profiles.vault.cancellation().borrow();
    fixture
        .profiles
        .save_authenticated_login(&original, "operator".into(), None, epoch)
        .await
        .unwrap();
    let saved = fixture.saved_target().await;
    assert_eq!(saved.username, "operator");
    for (kind, bytes) in [
        (Kind::PrivateKey, b"test-portable-key".as_slice()),
        (Kind::KeyPassphrase, b"test-key-phrase".as_slice()),
    ] {
        assert_eq!(
            fixture.profiles.vault.read(&saved, kind).unwrap().expose(),
            bytes
        );
        assert!(matches!(
            fixture.profiles.vault.read(&original, kind),
            Err(cshell_vault::VaultError::BindingMismatch)
        ));
    }
    assert_eq!(*fixture.profiles.vault.cancellation().borrow(), epoch);
}
