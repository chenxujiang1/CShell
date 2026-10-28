#![allow(clippy::unwrap_used)]
use super::*;
use cshell_application::{CatalogChange, ImportConflictPolicy, ProfileExportOptions};
use cshell_domain::{ProfileKind, ProfileRecord, SshAuthMethod};
use cshell_storage::SqliteProfileRepository;

fn password() -> Secret {
    Secret::new(b"independent-package-test-password".to_vec())
}
fn target(id: ProfileId) -> SshConnectionRecord {
    SshConnectionRecord {
        options: Default::default(),
        profile_id: id,
        host: "example.test".into(),
        port: 22,
        username: "alice".into(),
        auth_method: SshAuthMethod::Password,
        private_key_path: None,
        certificate_path: None,
        agent_backend: Default::default(),
        agent_identity: None,
        route: SshRoute::Direct,
    }
}
fn profile(id: ProfileId, name: &str) -> ProfileRecord {
    ProfileRecord {
        id,
        name: name.into(),
        kind: ProfileKind::Ssh,
        folder_id: None,
        tags: Default::default(),
        favorite: false,
        terminal: Default::default(),
    }
}

#[test]
fn package_mapping_rejects_changed_jump_targets_bindings_and_ambiguous_names() {
    let server = ProfileId::new();
    let jump = ProfileId::new();
    let mut connection = target(server);
    connection.route = SshRoute::Jump { profile_id: jump };
    let gateway = target(jump);
    let mut snapshot = CatalogSnapshot {
        profiles: vec![profile(server, "server"), profile(jump, "gateway")],
        ssh_connections: vec![connection.clone(), gateway],
        ..Default::default()
    };
    let context =
        serde_json::to_vec(&export_context(&snapshot, &BTreeSet::from([server])).unwrap()).unwrap();
    let record = CredentialPackageRecord::new(
        reference(server, Kind::Password),
        binding(&connection, Kind::Password).unwrap(),
        Secret::new(vec![1]),
    );
    let records = [record];
    assert!(prepare_records(&context, &records, &snapshot).is_ok());
    snapshot.ssh_connections[1].host = "changed.example.test".into();
    assert!(matches!(
        prepare_records(&context, &records, &snapshot),
        Err(VaultError::AlreadyExists)
    ));
    snapshot.ssh_connections[1].host = "example.test".into();
    let forged = CredentialPackageRecord::new(
        reference(server, Kind::Password),
        "forged-binding".into(),
        Secret::new(vec![1]),
    );
    assert!(matches!(
        prepare_records(&context, &[forged], &snapshot),
        Err(VaultError::InvalidInput)
    ));
    let replacement = ProfileId::new();
    snapshot.profiles[0].id = replacement;
    snapshot.ssh_connections[0].profile_id = replacement;
    assert!(prepare_records(&context, &records, &snapshot).is_ok());
    let duplicate = ProfileId::new();
    snapshot.profiles.push(profile(duplicate, "server"));
    snapshot.ssh_connections.push(target(duplicate));
    assert!(matches!(
        prepare_records(&context, &records, &snapshot),
        Err(VaultError::AlreadyExists)
    ));
}
fn request(service: &ProfileIpcService, op: Op, revision: u64) -> VaultRequest {
    let status = service.vault.response(None);
    let mut request = VaultRequest::new(op);
    request.expected_profile_revision = revision;
    request.expected_vault_revision = status.revision;
    request.expected_lock_generation = status.lock_generation;
    request.secret = password().expose().to_vec();
    request
}

#[tokio::test]
async fn encrypted_export_json_remapping_preview_and_atomic_restore_use_real_storage() {
    let directory = tempfile::tempdir().unwrap();
    let source = ProfileIpcService::new(
        SqliteProfileRepository::open(directory.path().join("source.db"))
            .await
            .unwrap(),
    );
    let first = ProfileId::new();
    let jump = ProfileId::new();
    let mut connection = target(first);
    connection.route = SshRoute::Jump { profile_id: jump };
    let gateway = target(jump);
    source
        .service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(profile(first, "server")),
                CatalogChange::UpsertProfile(profile(jump, "gateway")),
                CatalogChange::UpsertSshConnection(connection.clone()),
                CatalogChange::UpsertSshConnection(gateway.clone()),
            ],
        )
        .await
        .unwrap();
    source.vault.create(&password(), 0).unwrap();
    for connection in [&connection, &gateway] {
        source
            .vault
            .set_mode(
                connection.profile_id,
                source.vault.response(None).revision,
                true,
            )
            .unwrap();
        source
            .vault
            .edit(
                connection,
                Kind::Password,
                Some(&Secret::new(b"TRANSFER_SENTINEL".to_vec())),
            )
            .unwrap();
    }
    let snapshot = source.service.load().await.unwrap().snapshot();
    let mut outgoing = request(&source, Op::ExportPackage, snapshot.revision);
    outgoing.selected_profiles = vec![
        first.as_uuid().as_bytes().to_vec(),
        jump.as_uuid().as_bytes().to_vec(),
    ];
    let exported = source.handle_package(outgoing).await;
    assert_eq!(exported.status, Status::Ok as i32);
    assert!(
        !exported
            .package
            .windows(17)
            .any(|part| part == b"TRANSFER_SENTINEL")
    );
    let destination_path = directory.path().join("destination");
    std::fs::create_dir(&destination_path).unwrap();
    let destination = ProfileIpcService::new(
        SqliteProfileRepository::open(destination_path.join("profiles.db"))
            .await
            .unwrap(),
    );
    let json =
        cshell_application::export_profile_catalog(&snapshot, ProfileExportOptions::default())
            .unwrap();
    destination
        .service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(profile(ProfileId::new(), "server")),
                CatalogChange::UpsertProfile(profile(ProfileId::new(), "gateway")),
            ],
        )
        .await
        .unwrap();
    destination
        .service
        .commit_import(1, &json, ImportConflictPolicy::Replace)
        .await
        .unwrap();
    let saved = destination.service.load().await.unwrap().snapshot();
    assert!(
        !saved
            .ssh_connections
            .iter()
            .any(|target| target.profile_id == first)
    );
    destination.vault.create(&password(), 0).unwrap();
    let before = destination.vault.response(None);
    let mut wrong = request(&destination, Op::PreviewPackage, saved.revision);
    wrong.secret = b"wrong-password".to_vec();
    wrong.package = exported.package.clone();
    assert_eq!(
        destination.handle_package(wrong).await.status,
        Status::AuthenticationFailed as i32
    );
    assert_eq!(destination.vault.response(None).revision, before.revision);
    let mut preview_request = request(&destination, Op::PreviewPackage, saved.revision);
    preview_request.package = exported.package;
    let mut response = destination.handle_package(preview_request).await;
    assert_eq!(response.status, Status::Ok as i32);
    let preview = response.package_preview.take().unwrap();
    assert!(preview.can_commit);
    assert_eq!(preview.items.len(), 2);
    assert!(
        preview
            .items
            .iter()
            .all(|item| item.action == Action::Create as i32)
    );
    let mut commit = request(&destination, Op::CommitPackage, saved.revision);
    commit.preview_token = preview.token.clone();
    let replay = commit.clone();
    assert_eq!(
        destination.handle_package(commit).await.status,
        Status::Ok as i32
    );
    assert_eq!(
        destination.vault.response(None).revision,
        before.revision + 1
    );
    for target in &saved.ssh_connections {
        assert!(destination.vault.mode(target.profile_id).unwrap());
        assert_eq!(
            destination
                .vault
                .read(target, Kind::Password)
                .unwrap()
                .expose(),
            b"TRANSFER_SENTINEL"
        );
    }
    assert_eq!(
        destination.handle_package(replay).await.status,
        Status::Conflict as i32
    );
    assert!(destination.packages.lock().unwrap().0.is_empty());
    assert!(!format!("{preview:?} {response:?}").contains("alice"));
}

#[test]
fn restore_groups_skip_replace_capacity_and_stale_state_are_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let controller =
        crate::vault_control::VaultController::new(directory.path().join("master.vault"));
    controller.create(&password(), 0).unwrap();
    let first = target(ProfileId::new());
    let second = target(ProfileId::new());
    let records: Vec<_> = [&first, &second]
        .into_iter()
        .map(|t| {
            CredentialPackageRecord::new(
                reference(t.profile_id, Kind::Password),
                binding(t, Kind::Password).unwrap(),
                Secret::new(b"first-set".to_vec()),
            )
        })
        .collect();
    let status = controller.response(None);
    controller
        .restore_package(
            &records,
            Policy::Fail,
            status.revision,
            status.lock_generation,
        )
        .unwrap();
    let status = controller.response(None);
    assert_eq!(
        controller
            .package_actions(
                &[first.profile_id],
                Policy::Fail,
                status.revision,
                status.lock_generation
            )
            .unwrap(),
        vec![Action::Conflict]
    );
    controller
        .restore_package(
            &records,
            Policy::Skip,
            status.revision,
            status.lock_generation,
        )
        .unwrap();
    assert_eq!(controller.response(None).revision, status.revision);
    assert!(
        controller
            .restore_package(
                &records,
                Policy::Fail,
                status.revision,
                status.lock_generation
            )
            .is_err()
    );
    assert_eq!(controller.response(None).revision, status.revision);
    let oversized: Vec<_> = (0..64)
        .map(|_| {
            let t = target(ProfileId::new());
            CredentialPackageRecord::new(
                reference(t.profile_id, Kind::Password),
                binding(&t, Kind::Password).unwrap(),
                Secret::new(vec![1]),
            )
        })
        .collect();
    assert!(
        controller
            .restore_package(
                &oversized,
                Policy::Replace,
                status.revision,
                status.lock_generation
            )
            .is_err()
    );
    assert_eq!(controller.response(None).revision, status.revision);
    assert_eq!(
        controller.read(&first, Kind::Password).unwrap().expose(),
        b"first-set"
    );
    let current = controller.response(None);
    let replacement = CredentialPackageRecord::new(
        reference(first.profile_id, Kind::Password),
        binding(&first, Kind::Password).unwrap(),
        Secret::new(b"replacement".to_vec()),
    );
    controller
        .restore_package(
            &[replacement],
            Policy::Replace,
            current.revision,
            current.lock_generation,
        )
        .unwrap();
    assert_eq!(
        controller.read(&first, Kind::Password).unwrap().expose(),
        b"replacement"
    );
    let current = controller.response(None);
    controller.lock();
    assert!(
        controller
            .restore_package(
                &records,
                Policy::Replace,
                current.revision,
                current.lock_generation
            )
            .is_err()
    );
    assert_eq!(controller.response(None).revision, current.revision);
}

#[tokio::test]
async fn stale_catalog_confirmation_consumes_ciphertext_lease_without_vault_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let service = ProfileIpcService::new(
        SqliteProfileRepository::open(directory.path().join("profiles.db"))
            .await
            .unwrap(),
    );
    service.vault.create(&password(), 0).unwrap();
    let id = ProfileId::new();
    let connection = target(id);
    let context = serde_json::to_vec(&Context {
        version: 1,
        profiles: vec![PackageProfile {
            name: "server".into(),
            folder: Vec::new(),
            target: connection.clone(),
        }],
    })
    .unwrap();
    let record = CredentialPackageRecord::new(
        reference(id, Kind::Password),
        binding(&connection, Kind::Password).unwrap(),
        Secret::new(vec![1]),
    );
    let bytes = CredentialPackage::seal(&context, &[record], &password()).unwrap();
    let status = service.vault.response(None);
    let token = [5; 32];
    service.packages.lock().unwrap().0.insert(
        token,
        Lease {
            bytes: bytes.clone(),
            policy: Policy::Fail,
            profile_revision: 0,
            vault_revision: status.revision,
            generation: status.lock_generation,
            expires: Instant::now() + LEASE_LIFETIME,
        },
    );
    service
        .service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(profile(id, "server")),
                CatalogChange::UpsertSshConnection(connection),
            ],
        )
        .await
        .unwrap();
    let mut commit = request(&service, Op::CommitPackage, 0);
    commit.preview_token = token.to_vec();
    assert_eq!(
        service.handle_package(commit).await.status,
        Status::Conflict as i32
    );
    assert!(service.packages.lock().unwrap().0.is_empty());
    assert_eq!(service.vault.response(None).revision, status.revision);
    let second_token = [6; 32];
    service.packages.lock().unwrap().0.insert(
        second_token,
        Lease {
            bytes,
            policy: Policy::Fail,
            profile_revision: 1,
            vault_revision: status.revision,
            generation: status.lock_generation,
            expires: Instant::now() + LEASE_LIFETIME,
        },
    );
    let mut wrong = request(&service, Op::CommitPackage, 1);
    wrong.preview_token = second_token.to_vec();
    wrong.secret = b"wrong-password".to_vec();
    assert_eq!(
        service.handle_package(wrong).await.status,
        Status::AuthenticationFailed as i32
    );
    assert!(service.packages.lock().unwrap().0.is_empty());
    assert_eq!(service.vault.response(None).revision, status.revision);
}
