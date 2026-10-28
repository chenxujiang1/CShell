use cshell_application::{OpenSshImportOptions, ProfileImportDocument, import_openssh_config};
use cshell_domain::SshRoute;
use cshell_ipc::{
    Envelope, ProfileImportPolicy, ProfileOperation, ProfileRequest, ProfileResponse,
    ProfileStatus, envelope, features, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::error::Error;
use std::sync::Arc;

const IMPORT_FEATURES: u64 = features::PROFILE_CONTROL
    | features::PROFILE_CONNECTION_IMPORT
    | features::SSH_PROFILE_TARGET
    | features::SSH_PROFILE_AUTH
    | features::SSH_PROFILE_ROUTE
    | features::LOCAL_PROFILE;

fn import_request(
    operation: ProfileOperation,
    document: &ProfileImportDocument,
    revision: u64,
    policy: ProfileImportPolicy,
) -> Result<ProfileRequest, serde_json::Error> {
    let mut request = ProfileRequest::default();
    request.operation = operation as i32;
    request.expected_revision = revision;
    request.import_json = serde_json::to_vec(document)?;
    request.import_policy = policy as i32;
    Ok(request)
}

async fn exchange(
    service: &SessionIpcService,
    request: ProfileRequest,
    bits: u64,
) -> Result<ProfileResponse, Box<dyn Error>> {
    let (mut client, server) = tokio::io::duplex(2 * 1024 * 1024);
    let client_work = async move {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 7,
                payload: Some(envelope::Payload::ProfileRequest(request)),
                ..Default::default()
            },
        )
        .await?;
        read_envelope(&mut client).await
    };
    let (served, response) = tokio::join!(
        service.serve_connection_with_features(server, bits),
        client_work
    );
    served?;
    let response = response?;
    assert_eq!(response.request_id, 7);
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("missing Profile response".into());
    };
    Ok(response)
}

#[tokio::test]
async fn openssh_ipc_import_is_atomic_revision_checked_and_remaps_skipped_jump_aliases()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("import.db");
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(&path).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let initial = import_openssh_config(
        b"Host jump\nHostName original.test\nUser operator\n",
        OpenSshImportOptions::default(),
    )?;
    let original_id = initial.profiles[0].id;
    let created = exchange(
        &service,
        import_request(
            ProfileOperation::CommitImport,
            &initial,
            0,
            ProfileImportPolicy::Fail,
        )?,
        IMPORT_FEATURES,
    )
    .await?;
    assert_eq!(
        created.status,
        ProfileStatus::Ok as i32,
        "{}",
        created.detail
    );
    let replacement = import_openssh_config(b"Host jump\nHostName replacement.test\nHost app\nHostName app.test\nProxyJump jump\nHost *\nUser imported\n", OpenSshImportOptions::default())?;
    let fail = exchange(
        &service,
        import_request(
            ProfileOperation::PreviewImport,
            &replacement,
            0,
            ProfileImportPolicy::Fail,
        )?,
        IMPORT_FEATURES,
    )
    .await?;
    assert!(!fail.preview.ok_or("missing fail preview")?.can_commit);
    let skip = exchange(
        &service,
        import_request(
            ProfileOperation::PreviewImport,
            &replacement,
            0,
            ProfileImportPolicy::Skip,
        )?,
        IMPORT_FEATURES,
    )
    .await?;
    let preview = skip.preview.ok_or("missing skip preview")?;
    assert!(preview.can_commit);
    assert_eq!(preview.change_count, 2);
    assert!(preview.items[1].details.contains("imported@app.test:22"));
    assert!(preview.items[1].details.contains("jumps jump (operator@original.test:22)"));
    assert!(!preview.items[1].details.contains("replacement.test"));
    assert_eq!(profiles.handle(ProfileRequest::default()).await.revision, 1);

    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    sqlx::query("CREATE TRIGGER reject_target BEFORE INSERT ON profile_ssh_connections WHEN NEW.host = 'app.test' BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&pool).await?;
    let commit = import_request(
        ProfileOperation::CommitImport,
        &replacement,
        1,
        ProfileImportPolicy::Skip,
    )?;
    let failed = exchange(&service, commit.clone(), IMPORT_FEATURES).await?;
    assert_eq!(failed.status, ProfileStatus::Unavailable as i32);
    let intact = profiles.handle(ProfileRequest::default()).await;
    assert_eq!(intact.revision, 1);
    assert_eq!(
        intact
            .catalog
            .ok_or("missing intact catalog")?
            .profiles
            .len(),
        1
    );
    sqlx::query("DROP TRIGGER reject_target")
        .execute(&pool)
        .await?;
    pool.close().await;

    let committed = exchange(&service, commit.clone(), IMPORT_FEATURES).await?;
    assert_eq!(
        committed.status,
        ProfileStatus::Ok as i32,
        "{}",
        committed.detail
    );
    assert_eq!(committed.revision, 2);
    let catalog = committed.catalog.ok_or("missing committed catalog")?;
    let jump = catalog
        .ssh_connections
        .iter()
        .find(|target| target.host == "original.test")
        .ok_or("original jump missing")?;
    assert_eq!(jump.username, "operator");
    let app = catalog
        .ssh_connections
        .iter()
        .find(|target| target.host == "app.test")
        .ok_or("app missing")?;
    let app = cshell_domain::SshConnectionRecord::try_from(app.clone())?;
    assert_eq!(
        app.route,
        SshRoute::Jump {
            profile_id: original_id
        }
    );
    assert_eq!(
        exchange(&service, commit, IMPORT_FEATURES).await?.status,
        ProfileStatus::Conflict as i32
    );

    let replace = exchange(
        &service,
        import_request(
            ProfileOperation::CommitImport,
            &replacement,
            2,
            ProfileImportPolicy::Replace,
        )?,
        IMPORT_FEATURES,
    )
    .await?;
    assert_eq!(
        replace.status,
        ProfileStatus::Ok as i32,
        "{}",
        replace.detail
    );
    let catalog = replace.catalog.ok_or("missing replaced catalog")?;
    let jump = catalog
        .ssh_connections
        .iter()
        .find(|target| target.host == "replacement.test")
        .ok_or("replaced jump missing")?;
    assert_eq!(jump.profile_id, original_id.as_uuid().as_bytes());
    assert_eq!(catalog.profiles.len(), 2);
    assert_eq!(catalog.ssh_connections.len(), 2);
    drop(service);
    drop(profiles);
    let reopened = ProfileIpcService::new(SqliteProfileRepository::open(&path).await?);
    assert_eq!(reopened.handle(ProfileRequest::default()).await.revision, 3);
    Ok(())
}

#[tokio::test]
async fn openssh_connection_import_requires_each_negotiated_capability()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("import.db")).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let document = import_openssh_config(b"Host srv\nUser ops\n", OpenSshImportOptions::default())?;
    for missing in [
        features::PROFILE_CONNECTION_IMPORT,
        features::SSH_PROFILE_TARGET,
        features::SSH_PROFILE_AUTH,
        features::SSH_PROFILE_ROUTE,
        features::LOCAL_PROFILE,
    ] {
        for operation in [
            ProfileOperation::PreviewImport,
            ProfileOperation::CommitImport,
        ] {
            let response = exchange(
                &service,
                import_request(operation, &document, 0, ProfileImportPolicy::Fail)?,
                IMPORT_FEATURES & !missing,
            )
            .await?;
            assert_eq!(response.status, ProfileStatus::Unsupported as i32);
        }
    }
    assert_eq!(profiles.handle(ProfileRequest::default()).await.revision, 0);
    Ok(())
}
