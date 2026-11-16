#![allow(clippy::unwrap_used, clippy::field_reassign_with_default)]
use super::*;
use cshell_domain::{
    JumpConnectionOverride, JumpReusePolicy, ProfileKind, SshConnectionRecord, SshRoute,
};

#[tokio::test]
async fn unsupported_saved_policy_is_rejected_before_a_tcp_or_credentials() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a_id = ProfileId::new();
    let b_id = ProfileId::new();
    let profile = |id, name: &str| cshell_domain::ProfileRecord {
        id,
        name: name.into(),
        kind: ProfileKind::Ssh,
        folder_id: None,
        tags: Default::default(),
        favorite: false,
        terminal: Default::default(),
    };
    let a = SshConnectionRecord {
        profile_id: a_id,
        host: "127.0.0.1".into(),
        port: listener.local_addr().unwrap().port(),
        username: "operator".into(),
        auth_method: Default::default(),
        private_key_path: None,
        certificate_path: None,
        agent_backend: Default::default(),
        agent_identity: None,
        route: SshRoute::Direct,
        options: cshell_domain::SshConnectionOptions {
            keyboard_interactive: true,
            ..Default::default()
        },
    };
    let mut b = a.clone();
    b.profile_id = b_id;
    b.host = "server-b".into();
    b.route = SshRoute::Jump { profile_id: a_id };
    b.options.jump_reuse_policy = JumpReusePolicy::OnlyExisting;
    let repository = cshell_storage::SqliteProfileRepository::open(root.path().join("profiles.db"))
        .await
        .unwrap();
    let service = cshell_application::ProfileService::new(repository);
    service
        .apply_batch(
            0,
            &[
                cshell_application::CatalogChange::UpsertProfile(profile(a_id, "A")),
                cshell_application::CatalogChange::UpsertProfile(profile(b_id, "B")),
                cshell_application::CatalogChange::UpsertSshConnection(a.clone()),
                cshell_application::CatalogChange::UpsertSshConnection(b.clone()),
            ],
        )
        .await
        .unwrap();
    let service = SessionIpcService::new(Arc::new(
        LocalSessionRegistry::new(root.path().join("journals"), 8).unwrap(),
    ))
    .with_profiles(Arc::new(ProfileIpcService::new(service.into_repository())));
    let error = service
        .spawn_profile(
            b_id.as_uuid().as_bytes(),
            24,
            80,
            None,
            u64::MAX,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, SessionFailureCode::ProtocolRejected);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    let plan = crate::ssh_route::SshConnectionPlan::new("B".into(), b.clone(), &[a]).unwrap();
    let override_once = JumpConnectionOverride {
        policy: Some(JumpReusePolicy::AlwaysNew),
        candidate: None,
    };
    let resolved = plan
        .with_jump_override(Some(&override_once))
        .unwrap_or_else(|failure| panic!("{}", failure.detail));
    assert_eq!(
        resolved.target.options.jump_reuse_policy,
        JumpReusePolicy::AlwaysNew
    );
    assert_eq!(
        plan.target.options.jump_reuse_policy,
        JumpReusePolicy::OnlyExisting
    );
    let wire = cshell_ipc::JumpConnectionOverrideData {
        policy: Some(99),
        candidate: None,
    };
    let error = service
        .spawn_profile(
            b_id.as_uuid().as_bytes(),
            24,
            80,
            None,
            u64::MAX,
            None,
            Some(&wire),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, SessionFailureCode::InvalidConfiguration);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn candidate_protocol_reports_unavailable_not_an_empty_success() {
    let root = tempfile::tempdir().unwrap();
    let service =
        SessionIpcService::new(Arc::new(LocalSessionRegistry::new(root.path(), 8).unwrap()));
    for (id, status) in [
        (
            ProfileId::new().as_uuid().as_bytes().to_vec(),
            cshell_ipc::JumpCandidatesStatus::Unsupported,
        ),
        (
            vec![0; 16],
            cshell_ipc::JumpCandidatesStatus::InvalidRequest,
        ),
    ] {
        let result = service
            .handle_request(Envelope {
                request_id: 42,
                payload: Some(envelope::Payload::JumpCandidatesRequest(
                    cshell_ipc::JumpCandidatesRequest {
                        target_profile_id: id,
                    },
                )),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.request_id, 42);
        let Some(envelope::Payload::JumpCandidatesResponse(reply)) = result.payload else {
            panic!("candidate response")
        };
        assert_eq!(reply.status, status as i32);
        assert!(!reply.execution_supported);
        assert!(reply.candidates.is_empty());
    }
}
