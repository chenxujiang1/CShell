use cshell_application::{
    CatalogSnapshot, LocalProfileTransfer, ProfileExportOptions, ProfileImportDocument,
    export_profile_catalog,
};
use cshell_domain::{
    LocalConnectionRecord, LocalWorkingDirectory, ProfileId, ProfileKind, ProfileRecord,
    TerminalOverrides,
};
use cshell_ipc::{
    Envelope, ProfileImportPolicy, ProfileOperation, ProfileRequest, ProfileResponse,
    ProfileStatus, envelope, features, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::error::Error;
use std::sync::Arc;

const BITS: u64 = features::PROFILE_CONTROL
    | features::PROFILE_TRANSFER
    | features::PROFILE_CONNECTION_IMPORT
    | features::SSH_PROFILE_TARGET
    | features::SSH_PROFILE_AUTH
    | features::SSH_PROFILE_ROUTE
    | features::LOCAL_PROFILE;

fn request(operation: ProfileOperation, revision: u64) -> ProfileRequest {
    let mut request = ProfileRequest::default();
    request.operation = operation as i32;
    request.expected_revision = revision;
    request
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

fn fixture() -> Result<Vec<u8>, Box<dyn Error>> {
    let mut ssh = cshell_application::import_openssh_config(
        b"Host remote\nUser ops\n",
        cshell_application::OpenSshImportOptions::default(),
    )?;
    let id = ProfileId::new();
    ssh.profiles.push(ProfileRecord {
        id,
        name: "local".into(),
        kind: ProfileKind::Local,
        folder_id: None,
        tags: Default::default(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    });
    export_profile_catalog(
        &CatalogSnapshot {
            profiles: ssh.profiles,
            ssh_connections: ssh.ssh_connections,
            local_connections: vec![LocalConnectionRecord {
                profile_id: id,
                program: "shell".into(),
                args: vec!["ARG_SENTINEL".into()],
                cwd: LocalWorkingDirectory::Home,
                env_overrides: std::collections::BTreeMap::from([(
                    "TOKEN".into(),
                    "ENV_SENTINEL".into(),
                )]),
                close_policy: Default::default(),
            }],
            ..Default::default()
        },
        ProfileExportOptions {
            include_local_context: true,
        },
    )
    .map_err(Into::into)
}

#[tokio::test]
async fn framed_export_roundtrip_is_bounded_revision_checked_and_capability_gated()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("source.db")).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let mut import = request(ProfileOperation::CommitImport, 0);
    import.import_json = fixture()?;
    let created = exchange(&service, import.clone(), BITS).await?;
    assert_eq!(
        created.status,
        ProfileStatus::Ok as i32,
        "{}",
        created.detail
    );
    assert_eq!(created.revision, 1);
    let stale = exchange(&service, request(ProfileOperation::ExportCatalog, 0), BITS).await?;
    assert_eq!(stale.status, ProfileStatus::Conflict as i32);
    assert!(stale.export_json.is_empty());
    let refused = exchange(
        &service,
        request(ProfileOperation::ExportCatalog, 1),
        BITS & !features::PROFILE_TRANSFER,
    )
    .await?;
    assert_eq!(refused.status, ProfileStatus::Unsupported as i32);
    let refused = exchange(&service, import, BITS & !features::PROFILE_TRANSFER).await?;
    assert_eq!(refused.status, ProfileStatus::Unsupported as i32);
    let safe = exchange(&service, request(ProfileOperation::ExportCatalog, 1), BITS).await?;
    assert_eq!(safe.status, ProfileStatus::Ok as i32);
    let text = std::str::from_utf8(&safe.export_json)?;
    assert!(!text.contains("SENTINEL"));
    assert!(
        ProfileImportDocument::parse(&safe.export_json)?.local_connections[0]
            .args
            .is_none()
    );
    let mut full_request = request(ProfileOperation::ExportCatalog, 1);
    full_request.include_local_context = true;
    let full = exchange(&service, full_request, BITS).await?;
    assert!(std::str::from_utf8(&full.export_json)?.contains("ENV_SENTINEL"));
    assert!(!format!("{full:?}").contains("SENTINEL"));
    let destination = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("destination.db")).await?,
    ));
    let destination_service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("other-journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&destination));
    let mut restore = request(ProfileOperation::CommitImport, 0);
    restore.import_json = full.export_json;
    let restored = exchange(&destination_service, restore, BITS).await?;
    assert_eq!(
        restored.status,
        ProfileStatus::Ok as i32,
        "{}",
        restored.detail
    );
    let source = profiles
        .handle(ProfileRequest::default())
        .await
        .catalog
        .ok_or("source")?;
    let target = restored.catalog.ok_or("target")?;
    assert_eq!(source.profiles, target.profiles);
    assert_eq!(source.ssh_connections, target.ssh_connections);
    assert_eq!(source.local_connections, target.local_connections);
    assert_eq!(profiles.handle(ProfileRequest::default()).await.revision, 1);
    let mut invalid = request(ProfileOperation::List, 0);
    invalid.include_local_context = true;
    assert_eq!(
        exchange(&service, invalid, BITS).await?.status,
        ProfileStatus::Invalid as i32
    );
    let mut malformed = serde_json::from_slice::<serde_json::Value>(&fixture()?)?;
    malformed["local_connections"][0]["args"] = serde_json::json!("ARG_SENTINEL");
    let mut invalid = request(ProfileOperation::PreviewImport, 1);
    invalid.import_json = serde_json::to_vec(&malformed)?;
    let rejected = exchange(&service, invalid, BITS).await?;
    assert_eq!(rejected.status, ProfileStatus::Invalid as i32);
    assert!(!rejected.detail.contains("SENTINEL"));
    Ok(())
}

#[tokio::test]
async fn local_import_failure_rolls_back_metadata_targets_and_revision()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("profiles.db");
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(&path).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    sqlx::query("CREATE TRIGGER reject_local BEFORE INSERT ON profile_local_connections BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&pool).await?;
    let mut import = request(ProfileOperation::CommitImport, 0);
    import.import_json = fixture()?;
    let failed = exchange(&service, import.clone(), BITS).await?;
    assert_eq!(failed.status, ProfileStatus::Unavailable as i32);
    let intact = profiles.handle(ProfileRequest::default()).await;
    assert_eq!(intact.revision, 0);
    let intact = intact.catalog.ok_or("catalog")?;
    assert!(intact.profiles.is_empty());
    assert!(intact.ssh_connections.is_empty());
    assert!(intact.local_connections.is_empty());
    sqlx::query("DROP TRIGGER reject_local")
        .execute(&pool)
        .await?;
    let success = exchange(&service, import, BITS).await?;
    assert_eq!(success.status, ProfileStatus::Ok as i32);
    assert_eq!(success.revision, 1);
    let before = success.catalog.ok_or("catalog")?;
    let mut omitted = ProfileImportDocument::parse(
        &exchange(&service, request(ProfileOperation::ExportCatalog, 1), BITS)
            .await?
            .export_json,
    )?;
    omitted.local_connections[0].profile_id = ProfileId::new();
    let new_id = omitted.local_connections[0].profile_id;
    omitted
        .profiles
        .iter_mut()
        .find(|profile| profile.kind == ProfileKind::Local)
        .ok_or("local")?
        .id = new_id;
    omitted.local_connections[0].program = "replacement".into();
    let mut replace = request(ProfileOperation::CommitImport, 1);
    replace.import_policy = ProfileImportPolicy::Replace as i32;
    replace.import_json = serde_json::to_vec(&omitted)?;
    let replaced = exchange(&service, replace, BITS).await?;
    assert_eq!(
        replaced.status,
        ProfileStatus::Ok as i32,
        "{}",
        replaced.detail
    );
    let after = replaced.catalog.ok_or("catalog")?;
    assert_eq!(
        after.local_connections[0].profile_id,
        before.local_connections[0].profile_id
    );
    assert_eq!(
        after.local_connections[0].args,
        before.local_connections[0].args
    );
    assert_eq!(
        after.local_connections[0].env_overrides,
        before.local_connections[0].env_overrides
    );
    drop(service);
    drop(profiles);
    pool.close().await;
    let reopened = ProfileIpcService::new(SqliteProfileRepository::open(&path).await?);
    assert_eq!(
        reopened
            .handle(ProfileRequest::default())
            .await
            .catalog
            .ok_or("reopened")?
            .local_connections,
        after.local_connections
    );
    Ok(())
}

#[test]
fn local_transfer_debug_hides_explicit_context() {
    let record = LocalProfileTransfer {
        profile_id: ProfileId::new(),
        program: "shell".into(),
        args: Some(vec!["ARG_SENTINEL".into()]),
        cwd: Default::default(),
        env_overrides: Some(std::collections::BTreeMap::from([(
            "TOKEN".into(),
            "ENV_SENTINEL".into(),
        )])),
        close_policy: Default::default(),
    };
    assert!(!format!("{record:?}").contains("SENTINEL"));
}
