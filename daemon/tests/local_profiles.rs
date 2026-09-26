use cshell_domain::{
    InputAction, LocalConnectionRecord, LocalWorkingDirectory, ProfileId, ProfileKind,
    ProfileRecord, TerminalOverrides,
};
use cshell_ipc::{
    Envelope, LocalConnectionData, ProfileChange, ProfileOperation, ProfileRecordData,
    ProfileRequest, ProfileStatus, SessionCloseRequest, SessionCreateRequest, SessionFailureCode,
    SnapshotRequest, TerminalInputRequest, envelope, profile_change, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

async fn exchange(
    service: &SessionIpcService,
    payload: envelope::Payload,
) -> Result<envelope::Payload, Box<dyn Error>> {
    let (mut client, mut server) = tokio::io::duplex(2 * 1024 * 1024);
    let work = async {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 17,
                payload: Some(payload),
                ..Envelope::default()
            },
        )
        .await?;
        read_envelope(&mut client).await
    };
    let (served, response) = tokio::join!(service.serve_one(&mut server), work);
    served?;
    let response = response?;
    assert_eq!(response.request_id, 17);
    Ok(response.payload.ok_or("missing response")?)
}

async fn save(
    service: &SessionIpcService,
    revision: u64,
    record: &ProfileRecord,
    target: &LocalConnectionRecord,
) -> Result<cshell_ipc::ProfileResponse, Box<dyn Error>> {
    let payload = exchange(
        service,
        envelope::Payload::ProfileRequest(ProfileRequest {
            operation: ProfileOperation::ApplyChanges as i32,
            expected_revision: revision,
            changes: vec![
                ProfileChange {
                    change: Some(profile_change::Change::UpsertProfile(
                        ProfileRecordData::from(record),
                    )),
                },
                ProfileChange {
                    change: Some(profile_change::Change::UpsertLocalConnection(
                        LocalConnectionData::from(target),
                    )),
                },
            ],
            ..ProfileRequest::default()
        }),
    )
    .await?;
    let envelope::Payload::ProfileResponse(response) = payload else {
        return Err("missing Profile response".into());
    };
    Ok(response)
}

async fn create(
    service: &SessionIpcService,
    id: ProfileId,
) -> Result<cshell_ipc::SessionCreateResponse, Box<dyn Error>> {
    let response = exchange(
        service,
        envelope::Payload::SessionCreateRequest(SessionCreateRequest {
            profile_id: Some(id.as_uuid().as_bytes().to_vec()),
            rows: 24,
            cols: 100,
        }),
    )
    .await?;
    let envelope::Payload::SessionCreateResponse(response) = response else {
        return Err("missing create response".into());
    };
    Ok(response)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_local_profiles_launch_literal_args_cwd_env_and_keep_identity()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let cwd = temp.path().join("working directory \u{4e2d}\u{6587}");
    std::fs::create_dir(&cwd).map_err(|error| format!("create cwd: {error}"))?;
    let program = temp.path().join(if cfg!(windows) {
        "local helper.exe"
    } else {
        "local helper"
    });
    std::fs::copy(std::env::current_exe()?, &program)
        .map_err(|error| format!("copy helper: {error}"))?;
    let output = temp.path().join("probe.json");
    let registry = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 32)?);
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("profiles.db")).await?,
    ));
    let service =
        SessionIpcService::new(Arc::clone(&registry)).with_profiles(Arc::clone(&profiles));
    let record = ProfileRecord {
        id: ProfileId::new(),
        name: "Saved local".into(),
        kind: ProfileKind::Local,
        folder_id: None,
        tags: BTreeSet::new(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    };
    let args = vec![
        "--ignored",
        "--exact",
        "local_profile_probe",
        "--nocapture",
        "--skip",
        "literal spaces \"quotes\" $(); & \u{4e2d}\u{6587}",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let mut target = LocalConnectionRecord {
        profile_id: record.id,
        program: program.to_str().ok_or("non UTF8 path")?.into(),
        args: args.clone(),
        cwd: LocalWorkingDirectory::Explicit {
            path: cwd.to_str().ok_or("non UTF8 path")?.into(),
        },
        env_overrides: BTreeMap::from([
            (
                "CSHELL_LOCAL_PROBE_FILE".into(),
                output.to_str().ok_or("non UTF8 path")?.into(),
            ),
            (
                "CSHELL_LOCAL_VALUE".into(),
                "value with spaces \"quotes\" $()=\u{4e2d}\u{6587}".into(),
            ),
        ]),
    };
    let mut previous = None;
    for (index, (policy, expected_dir)) in [
        (target.cwd.clone(), cwd.clone()),
        (
            LocalWorkingDirectory::Home,
            cshell_local::home_directory().ok_or("home unavailable")?,
        ),
        (LocalWorkingDirectory::Inherit, std::env::current_dir()?),
    ]
    .into_iter()
    .enumerate()
    {
        target.cwd = policy;
        if output.exists() {
            std::fs::remove_file(&output)?;
        }
        let saved = save(&service, index as u64, &record, &target).await?;
        assert_eq!(saved.status, ProfileStatus::Ok as i32, "{}", saved.detail);
        let catalog = saved.catalog.ok_or("missing catalog")?;
        assert_eq!(
            catalog.local_connections,
            vec![LocalConnectionData::from(&target)]
        );
        let created = create(&service, record.id).await?;
        assert_eq!(
            created.failure_code,
            SessionFailureCode::None as i32,
            "{}",
            created.detail
        );
        let session = created.session.ok_or("missing local session")?;
        assert_eq!(
            session.profile_id,
            Some(record.id.as_uuid().as_bytes().to_vec())
        );
        assert_eq!(session.title, record.name);
        assert_ne!(previous.as_ref(), Some(&session.session_id));
        previous = Some(session.session_id.clone());
        let observed = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Ok(bytes) = tokio::fs::read(&output).await
                    && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        assert_eq!(
            std::fs::canonicalize(observed["cwd"].as_str().ok_or("missing cwd")?)
                .map_err(|error| format!("observed cwd: {error}; {}", observed["cwd"]))?,
            std::fs::canonicalize(expected_dir)?
        );
        assert_eq!(
            observed["value"],
            "value with spaces \"quotes\" $()=\u{4e2d}\u{6587}"
        );
        let actual_args = observed["args"]
            .as_array()
            .ok_or("missing args")?
            .iter()
            .map(|arg| arg.as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual_args, args);
        exchange(
            &service,
            envelope::Payload::TerminalInputRequest(TerminalInputRequest::from_action(
                cshell_domain::SessionId::from_bytes(session.session_id.as_slice().try_into()?),
                &InputAction::Text("local-input\r".into()),
            )?),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let envelope::Payload::FullFrame(frame) = exchange(
                    &service,
                    envelope::Payload::SnapshotRequest(SnapshotRequest {
                        session_id: session.session_id.clone(),
                        ..SnapshotRequest::default()
                    }),
                )
                .await?
                    && frame
                        .decode_terminal_snapshot()?
                        .cells
                        .iter()
                        .map(|cell| cell.character)
                        .collect::<String>()
                        .contains("ACK:local-input")
                {
                    break Ok::<_, Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        let id = cshell_domain::SessionId::from_bytes(session.session_id.as_slice().try_into()?);
        let info = registry.session_info(id)?;
        assert!(info.running);
        assert_eq!(info.profile_id, Some(record.id));
        exchange(
            &service,
            envelope::Payload::TerminalInputRequest(TerminalInputRequest::from_action(
                id,
                &InputAction::Text("exit\r".into()),
            )?),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !registry.session_info(id)?.running {
                    break Ok::<_, Box<dyn Error>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        assert_eq!(registry.session_info(id)?.profile_id, Some(record.id));
        assert!(
            registry.attach(id)?.full_frame().is_ok(),
            "exit should retain terminal history"
        );
        exchange(
            &service,
            envelope::Payload::SessionCloseRequest(SessionCloseRequest {
                session_id: session.session_id.clone(),
            }),
        )
        .await?;
        assert!(matches!(
            registry.session_info(id),
            Err(cshelld::SessionRegistryError::UnknownSession(_))
        ));
        assert!(registry.list()?.is_empty());
    }
    target.program = temp
        .path()
        .join("missing-program")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        save(&service, 3, &record, &target).await?.status,
        ProfileStatus::Ok as i32
    );
    let failed = create(&service, record.id).await?;
    assert!(failed.session.is_none());
    assert_eq!(
        failed.failure_code,
        SessionFailureCode::InvalidConfiguration as i32
    );
    assert_eq!(
        registry.list()?.len(),
        0,
        "failed launch must not create a replacement default session"
    );
    target.program = program.to_string_lossy().into_owned();
    target.cwd = LocalWorkingDirectory::Explicit {
        path: temp
            .path()
            .join("missing-cwd")
            .to_string_lossy()
            .into_owned(),
    };
    assert_eq!(
        save(&service, 4, &record, &target).await?.status,
        ProfileStatus::Ok as i32
    );
    std::fs::remove_file(&output)?;
    let fallback = create(&service, record.id).await?;
    assert_eq!(fallback.failure_code, SessionFailureCode::None as i32);
    let fallback = fallback.session.ok_or("missing fallback session")?;
    assert!(
        fallback
            .terminal_detail
            .contains("working directory is unavailable")
    );
    let observed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(&output).await
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(
        std::fs::canonicalize(observed["cwd"].as_str().ok_or("missing fallback cwd")?)?,
        std::fs::canonicalize(cshell_local::home_directory().ok_or("home unavailable")?)?
    );
    let envelope::Payload::SessionListResponse(list) = exchange(
        &service,
        envelope::Payload::SessionListRequest(cshell_ipc::SessionListRequest::default()),
    )
    .await?
    else {
        return Err("missing session list".into());
    };
    assert_eq!(list.sessions[0].terminal_detail, fallback.terminal_detail);
    exchange(
        &service,
        envelope::Payload::SessionCloseRequest(SessionCloseRequest {
            session_id: fallback.session_id,
        }),
    )
    .await?;
    assert_eq!(registry.list()?.len(), 0);
    Ok(())
}

#[tokio::test]
async fn local_configuration_and_discovery_require_capability() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("profiles.db")).await?,
    ));
    let service = Arc::new(
        SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
            temp.path().join("journals"),
            16,
        )?))
        .with_profiles(Arc::clone(&profiles)),
    );
    let record = ProfileRecord {
        id: ProfileId::new(),
        name: "Local".into(),
        kind: ProfileKind::Local,
        folder_id: None,
        tags: BTreeSet::new(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    };
    let target = LocalConnectionRecord {
        profile_id: record.id,
        program: std::env::current_exe()?.to_string_lossy().into_owned(),
        args: Vec::new(),
        cwd: LocalWorkingDirectory::Home,
        env_overrides: BTreeMap::new(),
    };
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let serving_service = Arc::clone(&service);
    let serving = tokio::spawn(async move {
        serving_service
            .serve_connection_with_features(server, cshell_ipc::features::PROFILE_CONTROL)
            .await
    });
    for (request_id, request) in [
        ProfileRequest {
            operation: ProfileOperation::ApplyChanges as i32,
            changes: vec![ProfileChange {
                change: Some(profile_change::Change::UpsertLocalConnection(
                    LocalConnectionData::from(&target),
                )),
            }],
            ..ProfileRequest::default()
        },
        ProfileRequest {
            operation: ProfileOperation::DiscoverLocalShells as i32,
            ..ProfileRequest::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: request_id as u64 + 1,
                payload: Some(envelope::Payload::ProfileRequest(request)),
                ..Envelope::default()
            },
        )
        .await?;
        let Some(envelope::Payload::ProfileResponse(response)) =
            read_envelope(&mut client).await?.payload
        else {
            return Err("missing capability response".into());
        };
        assert_eq!(response.status, ProfileStatus::Unsupported as i32);
    }
    assert_eq!(profiles.handle(ProfileRequest::default()).await.revision, 0);
    assert_eq!(
        save(&service, 0, &record, &target).await?.status,
        ProfileStatus::Ok as i32
    );
    write_envelope(
        &mut client,
        &Envelope {
            request_id: 3,
            payload: Some(envelope::Payload::SessionCreateRequest(
                SessionCreateRequest {
                    profile_id: Some(record.id.as_uuid().as_bytes().to_vec()),
                    rows: 24,
                    cols: 80,
                },
            )),
            ..Envelope::default()
        },
    )
    .await?;
    let Some(envelope::Payload::SessionCreateResponse(response)) =
        read_envelope(&mut client).await?.payload
    else {
        return Err("missing session response".into());
    };
    assert_eq!(
        response.failure_code,
        SessionFailureCode::Unsupported as i32
    );
    drop(client);
    serving.await??;
    let envelope::Payload::ProfileResponse(discovery) = exchange(
        &service,
        envelope::Payload::ProfileRequest(ProfileRequest {
            operation: ProfileOperation::DiscoverLocalShells as i32,
            ..ProfileRequest::default()
        }),
    )
    .await?
    else {
        return Err("missing discovery".into());
    };
    assert_eq!(discovery.status, ProfileStatus::Ok as i32);
    assert!(!discovery.local_shells.is_empty());
    for shell in discovery.local_shells {
        assert!(std::path::Path::new(&shell.program).is_file());
    }
    Ok(())
}

#[test]
#[ignore = "real PTY subprocess helper"]
fn local_profile_probe() -> Result<(), Box<dyn Error>> {
    let Some(output) = std::env::var_os("CSHELL_LOCAL_PROBE_FILE") else {
        return Ok(());
    };
    let value = serde_json::json!({
        "cwd": std::env::current_dir()?,
        "args": std::env::args().skip(1).collect::<Vec<_>>(),
        "value": std::env::var("CSHELL_LOCAL_VALUE")?,
    });
    std::fs::write(output, serde_json::to_vec(&value)?)?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line == "exit" {
            break;
        }
        println!("ACK:{line}");
        std::io::stdout().flush()?;
    }
    Ok(())
}
