use cshell_application::{
    CatalogChange, HostRepository, ProfileService, decode_frozen_hosts, decode_host_management,
    decode_host_preview, encode_host_selection, encode_hosts,
};
use cshell_domain::*;
use cshell_ipc::{
    Envelope, HostGroupOperation, HostGroupRequest, HostGroupResponse, HostGroupStatus, envelope,
    features, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    sync::Arc,
};

async fn exchange(
    service: &SessionIpcService,
    request: HostGroupRequest,
    bits: u64,
) -> Result<HostGroupResponse, Box<dyn Error>> {
    let (mut client, server) = tokio::io::duplex(2 * 1024 * 1024);
    let request = async move {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 81,
                payload: Some(envelope::Payload::HostGroupRequest(request)),
                ..Default::default()
            },
        )
        .await?;
        let reply = read_envelope(&mut client).await?;
        assert_eq!(reply.request_id, 81);
        Ok::<_, cshell_ipc::IpcError>(reply)
    };
    let (served, reply) = tokio::join!(
        service.serve_connection_with_features(server, bits),
        request
    );
    served?;
    match reply?.payload {
        Some(envelope::Payload::HostGroupResponse(reply)) => Ok(reply),
        _ => Err("missing host response".into()),
    }
}

fn save(hosts: &HostCatalog, revisions: (u64, u64)) -> Result<HostGroupRequest, Box<dyn Error>> {
    Ok(HostGroupRequest {
        operation: HostGroupOperation::Save as i32,
        expected_revision: revisions.0,
        expected_profile_revision: revisions.1,
        catalog_json: encode_hosts(hosts)?,
        selection_json: Vec::new(),
        target_limit: 0,
    })
}

fn query(
    operation: HostGroupOperation,
    expression: &str,
    revisions: (u64, u64),
    limit: u32,
) -> Result<HostGroupRequest, Box<dyn Error>> {
    Ok(HostGroupRequest {
        operation: operation as i32,
        expected_revision: revisions.0,
        expected_profile_revision: revisions.1,
        catalog_json: Vec::new(),
        // Deliberately serialize without application validation for server rejection tests.
        selection_json: serde_json::to_vec(&HostSelection::Selector {
            expression: expression.into(),
        })?,
        target_limit: limit,
    })
}

fn profile() -> (ProfileRecord, SshConnectionRecord) {
    let record = ProfileRecord {
        id: ProfileId::new(),
        name: "template".into(),
        kind: ProfileKind::Ssh,
        folder_id: None,
        tags: BTreeSet::new(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    };
    let ssh = SshConnectionRecord {
        options: Default::default(),
        profile_id: record.id,
        host: "template.test".into(),
        port: 22,
        username: "operator".into(),
        auth_method: SshAuthMethod::Password,
        private_key_path: None,
        certificate_path: None,
        agent_backend: SshAgentBackend::OpenSsh,
        agent_identity: None,
        route: SshRoute::Direct,
    };
    (record, ssh)
}

#[tokio::test]
async fn host_requests_require_capability_strict_shapes_and_both_catalog_revisions()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let registry = Arc::new(LocalSessionRegistry::new(
        directory.path().join("journals"),
        4,
    )?);
    let repository = SqliteProfileRepository::open(directory.path().join("hosts.sqlite")).await?;
    let service = SessionIpcService::new(Arc::clone(&registry))
        .with_profiles(Arc::new(ProfileIpcService::new(repository)));
    assert_eq!(
        exchange(&service, HostGroupRequest::default(), 0)
            .await?
            .status,
        HostGroupStatus::Unsupported as i32
    );
    assert_eq!(
        exchange(
            &SessionIpcService::new(Arc::clone(&registry)),
            HostGroupRequest::default(),
            features::HOST_GROUP_CONTROL
        )
        .await?
        .status,
        HostGroupStatus::Unavailable as i32
    );
    let mut mixed = HostGroupRequest::default();
    mixed.catalog_json = b"PRIVATE_QUERY".to_vec();
    let reply = exchange(&service, mixed, features::HOST_GROUP_CONTROL).await?;
    assert_eq!(reply.status, HostGroupStatus::Invalid as i32);
    assert!(!reply.detail.contains("PRIVATE_QUERY"));
    let hosts = HostCatalog::default();
    let reply = exchange(
        &service,
        save(&hosts, (0, 0))?,
        features::HOST_GROUP_CONTROL,
    )
    .await?;
    assert_eq!(reply.status, HostGroupStatus::Ok as i32);
    assert_eq!(reply.revision, 1);
    assert_eq!(decode_host_management(&reply.document_json)?.catalog, hosts);
    for revisions in [(0, 0), (1, 1)] {
        assert_eq!(
            exchange(
                &service,
                save(&hosts, revisions)?,
                features::HOST_GROUP_CONTROL
            )
            .await?
            .status,
            HostGroupStatus::Conflict as i32
        );
    }
    for expression in ["", "tag:", "unknown:PRIVATE_QUERY", "all OR"] {
        let reply = exchange(
            &service,
            query(HostGroupOperation::Preview, expression, (1, 0), 1000)?,
            features::HOST_GROUP_CONTROL,
        )
        .await?;
        assert_eq!(reply.status, HostGroupStatus::Invalid as i32);
        assert!(reply.document_json.is_empty());
        assert!(!reply.detail.contains("PRIVATE_QUERY"));
    }
    let mut mixed = query(HostGroupOperation::Preview, "all", (1, 0), 1)?;
    mixed.catalog_json = encode_hosts(&hosts)?;
    assert_eq!(
        exchange(&service, mixed, features::HOST_GROUP_CONTROL)
            .await?
            .status,
        HostGroupStatus::Invalid as i32
    );
    for limit in [0, 1001] {
        assert_eq!(
            exchange(
                &service,
                query(HostGroupOperation::Freeze, "all", (1, 0), limit)?,
                features::HOST_GROUP_CONTROL
            )
            .await?
            .status,
            HostGroupStatus::Invalid as i32
        );
    }
    assert!(registry.list()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn frozen_group_targets_survive_inventory_and_profile_changes_without_opening_sessions()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("hosts.sqlite");
    let profiles = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    let (profile, ssh) = profile();
    profiles
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(profile.clone()),
                CatalogChange::UpsertSshConnection(ssh.clone()),
            ],
        )
        .await?;
    let first = HostRecord {
        id: HostId::new(),
        name: "web".into(),
        address: "web.test".into(),
        port: 2222,
        profile_id: profile.id,
        tags: BTreeSet::from(["web".into()]),
        labels: BTreeMap::from([("env".into(), "prod".into())]),
        platform_hint: HostPlatformHint::Linux,
    };
    let mut second = first.clone();
    second.id = HostId::new();
    second.name = "db".into();
    second.address = "db.test".into();
    second.tags = BTreeSet::from(["db".into()]);
    let group = HostGroupRecord {
        id: HostGroupId::new(),
        name: "production web".into(),
        parent_id: None,
        membership: HostGroupMembership::Dynamic {
            selector: "tag:web AND env:prod".into(),
        },
    };
    let mut hosts = HostCatalog {
        hosts: vec![first.clone(), second],
        groups: vec![group.clone()],
        ..Default::default()
    };
    let registry = Arc::new(LocalSessionRegistry::new(
        directory.path().join("journals"),
        4,
    )?);
    let service = SessionIpcService::new(Arc::clone(&registry))
        .with_profiles(Arc::new(ProfileIpcService::new(profiles.into_repository())));
    assert_eq!(
        exchange(
            &service,
            save(&hosts, (0, 1))?,
            features::HOST_GROUP_CONTROL
        )
        .await?
        .status,
        HostGroupStatus::Ok as i32
    );
    let selection = HostSelection::Group { id: group.id };
    let request = || -> Result<HostGroupRequest, Box<dyn Error>> {
        Ok(HostGroupRequest {
            operation: HostGroupOperation::Freeze as i32,
            expected_revision: 1,
            expected_profile_revision: 1,
            catalog_json: Vec::new(),
            selection_json: encode_host_selection(&selection)?,
            target_limit: 1000,
        })
    };
    let preview = exchange(
        &service,
        query(
            HostGroupOperation::Preview,
            "tag:web AND env:prod",
            (1, 1),
            1000,
        )?,
        features::HOST_GROUP_CONTROL,
    )
    .await?;
    assert_eq!(
        decode_host_preview(&preview.document_json)?.hosts,
        vec![first.clone()]
    );
    let limited = exchange(
        &service,
        query(HostGroupOperation::Preview, "all", (1, 1), 1)?,
        features::HOST_GROUP_CONTROL,
    )
    .await?;
    assert_eq!(limited.status, HostGroupStatus::Limit as i32);
    assert!(limited.document_json.is_empty());
    let reply = exchange(&service, request()?, features::HOST_GROUP_CONTROL).await?;
    assert_eq!(reply.status, HostGroupStatus::Ok as i32);
    let frozen = decode_frozen_hosts(&reply.document_json)?;
    assert_eq!(frozen.targets(), std::slice::from_ref(&first));
    assert_eq!(frozen.connections(), std::slice::from_ref(&ssh));
    assert_eq!(
        frozen
            .resolved_connection(first.id)
            .ok_or("missing host connection")?
            .port,
        2222
    );
    let observer = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    hosts.hosts[0].address = "changed.test".into();
    observer.repository().save_hosts(1, &hosts).await?;
    let mut changed = ssh;
    changed.username = "new-operator".into();
    observer
        .apply_batch(1, &[CatalogChange::UpsertSshConnection(changed)])
        .await?;
    assert_eq!(
        exchange(&service, request()?, features::HOST_GROUP_CONTROL)
            .await?
            .status,
        HostGroupStatus::Conflict as i32
    );
    assert_eq!(decode_frozen_hosts(&reply.document_json)?, frozen);
    assert_eq!(frozen.targets(), &[first]);
    assert_eq!(frozen.connections()[0].username, "operator");
    observer
        .apply_batch(2, &[CatalogChange::RemoveProfile(profile.id)])
        .await?;
    let reply = exchange(
        &service,
        HostGroupRequest::default(),
        features::HOST_GROUP_CONTROL,
    )
    .await?;
    assert_eq!(reply.status, HostGroupStatus::Ok as i32);
    assert!(
        decode_host_management(&reply.document_json)?
            .profiles
            .is_empty()
    );
    assert_eq!(
        exchange(
            &service,
            query(HostGroupOperation::Freeze, "all", (2, 3), 1000)?,
            features::HOST_GROUP_CONTROL
        )
        .await?
        .status,
        HostGroupStatus::MissingProfile as i32
    );
    assert!(registry.list()?.is_empty());
    observer.into_repository().close().await;
    Ok(())
}
