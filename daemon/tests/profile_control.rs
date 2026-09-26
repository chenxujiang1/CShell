use cshell_application::{PROFILE_IMPORT_FORMAT, PROFILE_IMPORT_VERSION, ProfileImportDocument};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, SshAuthMethod,
    SshConnectionRecord, TerminalOverrides,
};
use cshell_ipc::{
    Envelope, ProfileChange, ProfileFolderData, ProfileImportPolicy, ProfileOperation,
    ProfileRecordData, ProfileRequest, ProfileResponse, ProfileStatus, SshConnectionData, envelope,
    profile_change, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::collections::BTreeSet;
use std::error::Error;
use std::sync::Arc;

fn folder(id: FolderId, name: &str) -> ProfileFolder {
    ProfileFolder {
        id,
        name: name.into(),
        parent_id: None,
        terminal: TerminalOverrides::default(),
    }
}

fn profile(id: ProfileId, name: &str, folder_id: FolderId) -> ProfileRecord {
    ProfileRecord {
        id,
        name: name.into(),
        kind: ProfileKind::Ssh,
        folder_id: Some(folder_id),
        tags: BTreeSet::new(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    }
}

fn request(operation: ProfileOperation) -> ProfileRequest {
    ProfileRequest {
        operation: operation as i32,
        expected_revision: 0,
        changes: vec![],
        import_json: vec![],
        import_policy: ProfileImportPolicy::Fail as i32,
        credential_profile_id: Vec::new(),
        credential_secret: Vec::new(),
        host_key_token: Vec::new(),
        host_key_fingerprint: String::new(),
    }
}

async fn exchange(
    service: &SessionIpcService,
    request: ProfileRequest,
) -> Result<ProfileResponse, Box<dyn Error>> {
    let (mut client, mut server) = tokio::io::duplex(2 * 1024 * 1024);
    let client_work = async {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 42,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::ProfileRequest(request)),
            },
        )
        .await?;
        read_envelope(&mut client).await
    };
    let (served, response) = tokio::join!(service.serve_one(&mut server), client_work);
    served?;
    let response = response?;
    assert_eq!(response.request_id, 42);
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("daemon did not return a Profile response".into());
    };
    Ok(response)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_host_key_confirmation_is_audited_and_unknown_sessions_stay_blocked()
-> Result<(), Box<dyn Error>> {
    use rand::rng;
    use russh::keys::ssh_key::{Algorithm, PrivateKey};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct HostOnlyServer;
    impl russh::server::Handler for HostOnlyServer {
        type Error = russh::Error;
    }
    let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519)?;
    let public = key.public_key().to_openssh()?;
    let rotated_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519)?;
    let mut config = russh::server::Config::default();
    config.keys.push(key);
    let config = Arc::new(config);
    let mut rotated_config = russh::server::Config::default();
    rotated_config.keys.push(rotated_key);
    let rotated_config = Arc::new(rotated_config);
    let rotate = Arc::new(AtomicBool::new(false));
    let server_rotate = Arc::clone(&rotate);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = if server_rotate.load(Ordering::SeqCst) {
                Arc::clone(&rotated_config)
            } else {
                Arc::clone(&config)
            };
            tokio::spawn(async move {
                if let Ok(running) = russh::server::run_stream(config, stream, HostOnlyServer).await
                {
                    let _ = running.await;
                }
            });
        }
    });
    let temp = tempfile::tempdir()?;
    let known_hosts = temp.path().join(".ssh").join("known_hosts");
    let repository = SqliteProfileRepository::open(temp.path().join("profiles.db")).await?;
    let profiles =
        Arc::new(ProfileIpcService::new(repository).with_known_hosts_path(known_hosts.clone()));
    let sessions = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 16)?);
    let service = SessionIpcService::new(sessions)
        .with_known_hosts_path(known_hosts.clone())
        .with_profiles(profiles);
    let profile_id = ProfileId::new();
    let mut create = request(ProfileOperation::ApplyChanges);
    create.changes = vec![
        ProfileChange {
            change: Some(profile_change::Change::UpsertProfile(
                ProfileRecordData::from(&ProfileRecord {
                    id: profile_id,
                    name: "first host".into(),
                    kind: ProfileKind::Ssh,
                    folder_id: None,
                    tags: BTreeSet::new(),
                    favorite: false,
                    terminal: TerminalOverrides::default(),
                }),
            )),
        },
        ProfileChange {
            change: Some(profile_change::Change::UpsertSshConnection(
                SshConnectionData::from(&SshConnectionRecord {
                    profile_id,
                    host: "127.0.0.1".into(),
                    port,
                    username: "alice".into(),
                    auth_method: SshAuthMethod::Password,
                    private_key_path: None,
                    certificate_path: None,
                    agent_backend: Default::default(),
                    agent_identity: None,
                    route: Default::default(),
                }),
            )),
        },
    ];
    let created = exchange(&service, create).await?;
    assert_eq!(
        created.status,
        ProfileStatus::Ok as i32,
        "{}",
        created.detail
    );
    let (mut client, mut daemon) = tokio::io::duplex(64 * 1024);
    let check_unknown = async {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 7,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionCreateRequest(
                    cshell_ipc::SessionCreateRequest {
                        local_launch: None,
                        rows: 24,
                        cols: 80,
                        profile_id: Some(profile_id.as_uuid().as_bytes().to_vec()),
                    },
                )),
            },
        )
        .await?;
        read_envelope(&mut client).await
    };
    let (served, response) = tokio::join!(service.serve_one(&mut daemon), check_unknown);
    served?;
    let response = response?;
    let Some(envelope::Payload::SessionCreateResponse(blocked)) = response.payload else {
        return Err("missing SSH session response".into());
    };
    assert!(blocked.session.is_none());
    assert_eq!(
        blocked.failure_code,
        cshell_ipc::SessionFailureCode::HostKeyUnknown as i32
    );
    assert!(blocked.detail.contains("known_hosts"), "{}", blocked.detail);

    let mut preview_request = request(ProfileOperation::PreviewHostKey);
    preview_request.expected_revision = created.revision;
    preview_request.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
    let preview_response = exchange(&service, preview_request.clone()).await?;
    assert_eq!(
        preview_response.status,
        ProfileStatus::Ok as i32,
        "{}",
        preview_response.detail
    );
    let preview = preview_response
        .host_key_preview
        .ok_or("missing host-key preview")?;
    assert_eq!(preview.public_key_line, public);
    assert!(!known_hosts.exists());

    let mut confirm = request(ProfileOperation::ConfirmHostKey);
    confirm.expected_revision = created.revision;
    confirm.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
    confirm.host_key_token = preview.token.clone();
    confirm.host_key_fingerprint = "SHA256:incorrect".into();
    let wrong = exchange(&service, confirm.clone()).await?;
    assert_eq!(wrong.status, ProfileStatus::Invalid as i32);
    assert!(!known_hosts.exists());
    confirm.host_key_fingerprint = preview.fingerprint.clone();
    rotate.store(true, Ordering::SeqCst);
    let changed = exchange(&service, confirm.clone()).await?;
    assert_eq!(changed.status, ProfileStatus::Conflict as i32);
    assert!(!known_hosts.exists());
    rotate.store(false, Ordering::SeqCst);
    let new_preview = exchange(&service, preview_request.clone())
        .await?
        .host_key_preview
        .ok_or("missing refreshed host-key preview")?;
    confirm.host_key_token = new_preview.token;
    let imported = exchange(&service, confirm.clone()).await?;
    assert_eq!(
        imported.status,
        ProfileStatus::Ok as i32,
        "{}",
        imported.detail
    );
    let content = std::fs::read_to_string(&known_hosts)?;
    assert!(content.contains("CShell host-key-import v1"));
    assert!(content.contains(&format!("profile={profile_id}")));
    assert!(content.contains(&format!("[127.0.0.1]:{port} {public}")));
    let replay = exchange(&service, confirm).await?;
    assert_eq!(replay.status, ProfileStatus::Invalid as i32);
    let repeat = exchange(&service, preview_request).await?;
    assert_eq!(repeat.status, ProfileStatus::Conflict as i32);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn profile_control_previews_and_commits_import_once() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("cshell.db");
    let repository = SqliteProfileRepository::open(&db).await?;
    let sessions = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 16)?);
    let service = SessionIpcService::new(sessions)
        .with_profiles(Arc::new(ProfileIpcService::new(repository)));

    let first_folder = FolderId::new();
    let first_profile = ProfileId::new();
    let mut create = request(ProfileOperation::ApplyChanges);
    create.changes = vec![
        ProfileChange {
            change: Some(profile_change::Change::UpsertFolder(
                ProfileFolderData::from(&folder(first_folder, "Ops")),
            )),
        },
        ProfileChange {
            change: Some(profile_change::Change::UpsertProfile(
                ProfileRecordData::from(&profile(first_profile, "srv", first_folder)),
            )),
        },
        ProfileChange {
            change: Some(profile_change::Change::UpsertSshConnection(
                SshConnectionData::from(&SshConnectionRecord {
                    profile_id: first_profile,
                    host: "srv.example.com".into(),
                    port: 22,
                    username: "alice".into(),
                    auth_method: Default::default(),
                    private_key_path: None,
                    certificate_path: None,
                    agent_backend: Default::default(),
                    agent_identity: None,
                    route: Default::default(),
                }),
            )),
        },
    ];
    let response = exchange(&service, create).await?;
    assert_eq!(response.status, ProfileStatus::Ok as i32);
    assert_eq!(response.revision, 1);

    let source_folder = FolderId::new();
    let mut replacement = profile(ProfileId::new(), "srv", source_folder);
    replacement.tags.insert("imported".into());
    let document = ProfileImportDocument {
        format: PROFILE_IMPORT_FORMAT.into(),
        version: PROFILE_IMPORT_VERSION,
        folders: vec![folder(source_folder, "Ops")],
        profiles: vec![replacement, profile(ProfileId::new(), "new", source_folder)],
    };
    let bytes = serde_json::to_vec(&document)?;
    let mut preview = request(ProfileOperation::PreviewImport);
    preview.import_json = bytes.clone();
    let failed_policy = exchange(&service, preview.clone()).await?;
    assert_eq!(failed_policy.status, ProfileStatus::Ok as i32);
    assert!(!failed_policy.preview.ok_or("missing preview")?.can_commit);
    let before = exchange(&service, request(ProfileOperation::List)).await?;
    assert_eq!(before.revision, 1);
    let before_catalog = before.catalog.ok_or("missing catalog")?;
    assert_eq!(before_catalog.ssh_connections.len(), 1);
    assert_eq!(before_catalog.ssh_connections[0].host, "srv.example.com");

    preview.import_policy = ProfileImportPolicy::Skip as i32;
    let skipped = exchange(&service, preview.clone()).await?;
    let skipped_preview = skipped.preview.ok_or("missing skip preview")?;
    assert!(skipped_preview.can_commit);
    assert_eq!(skipped_preview.change_count, 1);

    preview.import_policy = ProfileImportPolicy::Replace as i32;
    let accepted = exchange(&service, preview).await?;
    let accepted_preview = accepted.preview.ok_or("missing replacement preview")?;
    assert!(accepted_preview.can_commit);
    assert_eq!(accepted_preview.base_revision, 1);
    assert_eq!(accepted_preview.change_count, 3);

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
        .await?;
    sqlx::query("CREATE TRIGGER reject_import BEFORE INSERT ON profile_records WHEN NEW.name = 'new' BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&pool).await?;
    let mut commit = request(ProfileOperation::CommitImport);
    commit.expected_revision = 1;
    commit.import_json = bytes.clone();
    commit.import_policy = ProfileImportPolicy::Replace as i32;
    let blocked = exchange(&service, commit.clone()).await?;
    assert_eq!(blocked.status, ProfileStatus::Unavailable as i32);
    let intact = exchange(&service, request(ProfileOperation::List)).await?;
    assert_eq!(intact.revision, 1);
    assert_eq!(intact.catalog.ok_or("missing catalog")?.profiles.len(), 1);
    sqlx::query("DROP TRIGGER reject_import")
        .execute(&pool)
        .await?;
    pool.close().await;

    let committed = exchange(&service, commit.clone()).await?;
    assert_eq!(committed.status, ProfileStatus::Ok as i32);
    assert_eq!(committed.revision, 2);
    let catalog = committed.catalog.ok_or("missing committed catalog")?;
    assert_eq!(catalog.profiles.len(), 2);
    assert_eq!(catalog.ssh_connections.len(), 1);
    assert!(catalog.profiles.iter().any(
        |record| record.id == first_profile.as_uuid().as_bytes() && record.tags == ["imported"]
    ));
    let stale = exchange(&service, commit).await?;
    assert_eq!(stale.status, ProfileStatus::Conflict as i32);
    assert_eq!(
        exchange(&service, request(ProfileOperation::List))
            .await?
            .revision,
        2
    );
    Ok(())
}

#[tokio::test]
async fn ssh_target_write_requires_negotiated_feature() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repository = SqliteProfileRepository::open(temp.path().join("cshell.db")).await?;
    let sessions = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 16)?);
    let service = SessionIpcService::new(sessions)
        .with_profiles(Arc::new(ProfileIpcService::new(repository)));
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(async move {
        service
            .serve_connection_with_features(server, cshell_ipc::features::PROFILE_CONTROL)
            .await
    });
    let mut request = request(ProfileOperation::ApplyChanges);
    request.changes.push(ProfileChange {
        change: Some(profile_change::Change::UpsertSshConnection(
            SshConnectionData::from(&SshConnectionRecord {
                profile_id: ProfileId::new(),
                host: "example.com".into(),
                port: 22,
                username: "alice".into(),
                auth_method: Default::default(),
                private_key_path: None,
                certificate_path: None,
                agent_backend: Default::default(),
                agent_identity: None,
                route: Default::default(),
            }),
        )),
    });
    write_envelope(
        &mut client,
        &Envelope {
            request_id: 7,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::ProfileRequest(request)),
        },
    )
    .await?;
    let response = read_envelope(&mut client).await?;
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("missing Profile response".into());
    };
    assert_eq!(response.status, ProfileStatus::Unsupported as i32);
    drop(client);
    serving.await??;
    Ok(())
}

#[tokio::test]
async fn saved_ssh_profile_requires_keychain_password_before_connecting()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repository = SqliteProfileRepository::open(temp.path().join("cshell.db")).await?;
    let sessions = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 16)?);
    let known_hosts = temp.path().join("known_hosts");
    std::fs::write(&known_hosts, "")?;
    let service = SessionIpcService::new(sessions)
        .with_known_hosts_path(known_hosts)
        .with_profiles(Arc::new(ProfileIpcService::new(repository)));
    let folder_id = FolderId::new();
    let profile_id = ProfileId::new();
    let mut create = request(ProfileOperation::ApplyChanges);
    create.changes = vec![
        ProfileChange {
            change: Some(profile_change::Change::UpsertFolder(
                ProfileFolderData::from(&folder(folder_id, "SSH")),
            )),
        },
        ProfileChange {
            change: Some(profile_change::Change::UpsertProfile(
                ProfileRecordData::from(&profile(profile_id, "remote", folder_id)),
            )),
        },
        ProfileChange {
            change: Some(profile_change::Change::UpsertSshConnection(
                SshConnectionData::from(&SshConnectionRecord {
                    profile_id,
                    host: "127.0.0.1".into(),
                    port: 22,
                    username: "alice".into(),
                    auth_method: Default::default(),
                    private_key_path: None,
                    certificate_path: None,
                    agent_backend: Default::default(),
                    agent_identity: None,
                    route: Default::default(),
                }),
            )),
        },
    ];
    assert_eq!(
        exchange(&service, create).await?.status,
        ProfileStatus::Ok as i32
    );
    let (mut client, mut server) = tokio::io::duplex(64 * 1024);
    let client_work = async {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 91,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionCreateRequest(
                    cshell_ipc::SessionCreateRequest {
                        local_launch: None,
                        rows: 24,
                        cols: 80,
                        profile_id: Some(profile_id.as_uuid().as_bytes().to_vec()),
                    },
                )),
            },
        )
        .await?;
        read_envelope(&mut client).await
    };
    let (served, response) = tokio::join!(service.serve_one(&mut server), client_work);
    served?;
    let response = response?;
    assert_eq!(response.request_id, 91);
    let Some(envelope::Payload::SessionCreateResponse(created)) = response.payload else {
        return Err("missing SSH create response".into());
    };
    assert!(created.session.is_none());
    assert!(
        created.detail.contains("password unavailable"),
        "{}",
        created.detail
    );
    Ok(())
}

#[tokio::test]
async fn saved_key_passphrase_control_is_bound_to_profile_key_path() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("CSHELL_KEYCHAIN_NATIVE_TEST").is_none() {
        return Ok(());
    }
    use cshell_vault::{KeychainError, ProfileKeyPassphraseRef, SystemProfileKeyPassphraseVault};
    let temp = tempfile::tempdir()?;
    let repository = SqliteProfileRepository::open(temp.path().join("profiles.db")).await?;
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        16,
    )?))
    .with_profiles(Arc::new(ProfileIpcService::new(repository)));
    let folder_id = FolderId::new();
    let profile_id = ProfileId::new();
    let key_path = temp.path().join("id_ed25519");
    let key_path = key_path.to_str().ok_or("key path is not UTF-8")?.to_owned();
    let reference = ProfileKeyPassphraseRef::from_profile_bytes(*profile_id.as_uuid().as_bytes());
    let vault = SystemProfileKeyPassphraseVault::new();
    let result: Result<(), Box<dyn Error>> = async {
        let mut create = request(ProfileOperation::ApplyChanges);
        create.changes = vec![
            ProfileChange {
                change: Some(profile_change::Change::UpsertFolder(
                    ProfileFolderData::from(&folder(folder_id, "SSH")),
                )),
            },
            ProfileChange {
                change: Some(profile_change::Change::UpsertProfile(
                    ProfileRecordData::from(&profile(profile_id, "key", folder_id)),
                )),
            },
            ProfileChange {
                change: Some(profile_change::Change::UpsertSshConnection(
                    SshConnectionData::from(&SshConnectionRecord {
                        profile_id,
                        host: "example.com".into(),
                        port: 22,
                        username: "alice".into(),
                        auth_method: SshAuthMethod::PrivateKey,
                        private_key_path: Some(key_path.clone()),
                        certificate_path: None,
                        agent_backend: Default::default(),
                        agent_identity: None,
                        route: Default::default(),
                    }),
                )),
            },
        ];
        let created = exchange(&service, create).await?;
        assert_eq!(
            created.status,
            ProfileStatus::Ok as i32,
            "{}",
            created.detail
        );
        let mut set = request(ProfileOperation::SetKeyPassphrase);
        set.expected_revision = created.revision;
        set.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
        set.credential_secret = b"test-passphrase".to_vec();
        let saved = exchange(&service, set).await?;
        assert_eq!(saved.status, ProfileStatus::Ok as i32, "{}", saved.detail);
        assert_eq!(
            vault
                .read(&reference, &key_path)
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?
                .expose(),
            b"test-passphrase"
        );
        assert_eq!(
            vault.read(&reference, "other-key").err(),
            Some(KeychainError::BindingMismatch)
        );
        let mut remove = request(ProfileOperation::DeleteKeyPassphrase);
        remove.expected_revision = created.revision;
        remove.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
        assert_eq!(
            exchange(&service, remove).await?.status,
            ProfileStatus::Ok as i32
        );
        assert_eq!(
            vault.read(&reference, &key_path).err(),
            Some(KeychainError::Missing)
        );
        Ok(())
    }
    .await;
    let _ = vault.delete(&reference);
    result
}

#[tokio::test]
async fn ssh_route_write_requires_negotiated_feature() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("routes.db")).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        16,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(async move {
        service
            .serve_connection_with_features(
                server,
                cshell_ipc::features::PROFILE_CONTROL
                    | cshell_ipc::features::SSH_PROFILE_TARGET
                    | cshell_ipc::features::SSH_PROFILE_AUTH,
            )
            .await
    });
    let mut request = request(ProfileOperation::ApplyChanges);
    request.changes.push(ProfileChange {
        change: Some(profile_change::Change::UpsertSshConnection(
            SshConnectionData::from(&SshConnectionRecord {
                profile_id: ProfileId::new(),
                host: "target.example.com".into(),
                port: 22,
                username: "alice".into(),
                auth_method: Default::default(),
                private_key_path: None,
                certificate_path: None,
                agent_backend: Default::default(),
                agent_identity: None,
                route: cshell_domain::SshRoute::HttpConnect {
                    host: "proxy.example.com".into(),
                    port: 8080,
                },
            }),
        )),
    });
    write_envelope(
        &mut client,
        &Envelope {
            request_id: 7,
            payload: Some(envelope::Payload::ProfileRequest(request)),
            ..Envelope::default()
        },
    )
    .await?;
    let response = read_envelope(&mut client).await?;
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("missing Profile response".into());
    };
    assert_eq!(response.status, ProfileStatus::Unsupported as i32);
    assert!(response.detail.contains("routes"));
    drop(client);
    serving.await??;
    let catalog = profiles.handle(ProfileRequest::default()).await;
    assert_eq!(catalog.revision, 0);
    assert!(
        catalog
            .catalog
            .ok_or("missing catalog")?
            .ssh_connections
            .is_empty()
    );
    Ok(())
}
