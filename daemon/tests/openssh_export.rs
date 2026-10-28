use cshell_application::{OpenSshImportOptions, import_openssh_config};
use cshell_domain::{ProfileId, SshRoute};
use cshell_ipc::{
    Envelope, ProfileOperation, ProfileRequest, ProfileResponse, ProfileStatus, envelope, features,
    read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::{error::Error, sync::Arc};

const EXPORT_BITS: u64 = features::PROFILE_CONTROL | features::OPENSSH_EXPORT;
const IMPORT_BITS: u64 = features::PROFILE_CONTROL
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
                request_id: 17,
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
    assert_eq!(response.request_id, 17);
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("missing Profile response".into());
    };
    Ok(response)
}

#[tokio::test]
async fn framed_openssh_export_preserves_jump_selection_checks_revision_and_requires_capability()
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
    let document = import_openssh_config(
        b"Host jump\nHostName jump.test\nUser ops\nPreferredAuthentications password\nHost app\nHostName app.test\nUser worker\nProxyJump jump\nPreferredAuthentications password\nHost excluded\nUser other\nPreferredAuthentications password\n",
        OpenSshImportOptions::default(),
    )?;
    let app = document
        .profiles
        .iter()
        .find(|p| p.name == "app")
        .ok_or("app")?
        .id;
    let jump = document
        .profiles
        .iter()
        .find(|p| p.name == "jump")
        .ok_or("jump")?
        .id;
    let excluded = document
        .profiles
        .iter()
        .find(|p| p.name == "excluded")
        .ok_or("excluded")?
        .id;
    let mut import = request(ProfileOperation::CommitImport, 0);
    import.import_json = serde_json::to_vec(&document)?;
    let created = exchange(&service, import, IMPORT_BITS).await?;
    assert_eq!(
        created.status,
        ProfileStatus::Ok as i32,
        "{}",
        created.detail
    );
    let before = created.catalog.ok_or("catalog")?;
    let mut outgoing = request(ProfileOperation::ExportOpenSsh, 1);
    outgoing.export_profile_ids = vec![app.as_uuid().as_bytes().to_vec()];
    for bits in [
        EXPORT_BITS & !features::OPENSSH_EXPORT,
        EXPORT_BITS & !features::PROFILE_CONTROL,
    ] {
        let refused = exchange(&service, outgoing.clone(), bits).await?;
        assert_eq!(refused.status, ProfileStatus::Unsupported as i32);
        assert!(refused.export_json.is_empty());
    }
    let mut stale = outgoing.clone();
    stale.expected_revision = 0;
    let stale = exchange(&service, stale, EXPORT_BITS).await?;
    assert_eq!(stale.status, ProfileStatus::Conflict as i32);
    assert!(stale.export_json.is_empty());
    let exported = exchange(&service, outgoing, EXPORT_BITS).await?;
    assert_eq!(
        exported.status,
        ProfileStatus::Ok as i32,
        "{}",
        exported.detail
    );
    assert_eq!(exported.revision, 1);
    assert!(exported.catalog.is_none());
    let text = std::str::from_utf8(&exported.export_json)?;
    assert!(text.contains(&cshell_application::openssh_export_alias(app)));
    assert!(text.contains(&cshell_application::openssh_export_alias(jump)));
    assert!(!text.contains(&cshell_application::openssh_export_alias(excluded)));
    let roundtrip = import_openssh_config(&exported.export_json, OpenSshImportOptions::default())?;
    assert_eq!(roundtrip.ssh_connections.len(), 2);
    assert_eq!(
        roundtrip
            .ssh_connections
            .iter()
            .filter(|target| matches!(target.route, SshRoute::Jump { .. }))
            .count(),
        1
    );
    let all = exchange(
        &service,
        request(ProfileOperation::ExportOpenSsh, 1),
        EXPORT_BITS,
    )
    .await?;
    assert_eq!(all.status, ProfileStatus::Ok as i32);
    assert_eq!(
        import_openssh_config(&all.export_json, OpenSshImportOptions::default())?
            .profiles
            .len(),
        3
    );
    drop(service);
    drop(profiles);
    let reopened = ProfileIpcService::new(SqliteProfileRepository::open(path).await?);
    let unchanged = reopened.handle(ProfileRequest::default()).await;
    assert_eq!(unchanged.revision, 1);
    let after = unchanged.catalog.ok_or("reopened")?;
    assert_eq!(before.profiles, after.profiles);
    assert_eq!(before.ssh_connections, after.ssh_connections);
    Ok(())
}

#[tokio::test]
async fn framed_openssh_export_refuses_malformed_and_mixed_requests_without_catalog_mutation()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("profiles.db")).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let document = import_openssh_config(b"Host app\nUser ops\n", OpenSshImportOptions::default())?;
    let id = document.profiles[0].id.as_uuid().as_bytes().to_vec();
    let mut import = request(ProfileOperation::CommitImport, 0);
    import.import_json = serde_json::to_vec(&document)?;
    assert_eq!(
        exchange(&service, import, IMPORT_BITS).await?.status,
        ProfileStatus::Ok as i32
    );
    let mut requests = Vec::new();
    for selection in [
        vec![vec![0; 15]],
        vec![ProfileId::new().as_uuid().as_bytes().to_vec()],
        vec![id.clone(), id.clone()],
        vec![id; cshell_application::MAX_OPENSSH_EXPORT_HOSTS + 1],
    ] {
        let mut invalid = request(ProfileOperation::ExportOpenSsh, 1);
        invalid.export_profile_ids = selection;
        requests.push(invalid);
    }
    let mut context = request(ProfileOperation::ExportOpenSsh, 1);
    context.include_local_context = true;
    requests.push(context);
    let mut secret = request(ProfileOperation::ExportOpenSsh, 1);
    secret.credential_secret = b"SECRET_EXPORT_SENTINEL".to_vec();
    requests.push(secret);
    let mut content = request(ProfileOperation::ExportOpenSsh, 1);
    content.import_json = b"SECRET_EXPORT_SENTINEL".to_vec();
    requests.push(content);
    let mut policy = request(ProfileOperation::ExportOpenSsh, 1);
    policy.import_policy = 1;
    requests.push(policy);
    let mut list = request(ProfileOperation::List, 1);
    list.export_profile_ids = vec![document.profiles[0].id.as_uuid().as_bytes().to_vec()];
    requests.push(list);
    for invalid in requests {
        let rejected = exchange(&service, invalid, EXPORT_BITS).await?;
        assert_eq!(
            rejected.status,
            ProfileStatus::Invalid as i32,
            "{}",
            rejected.detail
        );
        assert!(rejected.export_json.is_empty());
        assert!(!rejected.detail.contains("SENTINEL"));
    }
    let unchanged = profiles.handle(ProfileRequest::default()).await;
    assert_eq!(unchanged.revision, 1);
    assert_eq!(unchanged.catalog.ok_or("catalog")?.profiles.len(), 1);
    Ok(())
}
