use cshell_ipc::{
    Envelope, VaultControlStatus as Status, VaultOperation as Op, VaultRequest, VaultResponse,
    envelope, features, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::{error::Error, sync::Arc};

async fn exchange(
    service: &SessionIpcService,
    request: VaultRequest,
    bits: u64,
) -> Result<VaultResponse, Box<dyn Error>> {
    let (mut client, server) = tokio::io::duplex(1024 * 1024);
    let client_work = async move {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 42,
                payload: Some(envelope::Payload::VaultRequest(request)),
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
    assert_eq!(response.request_id, 42);
    let Some(envelope::Payload::VaultResponse(response)) = response.payload else {
        return Err("missing Vault response".into());
    };
    Ok(response)
}

#[tokio::test]
async fn framed_packages_require_both_capabilities_and_reject_mixed_or_oversized_requests()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("profiles.db")).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(profiles);
    let bits = features::VAULT_CONTROL | features::CREDENTIAL_PACKAGE;
    for op in [Op::ExportPackage, Op::PreviewPackage, Op::CommitPackage] {
        for missing in [features::VAULT_CONTROL, features::CREDENTIAL_PACKAGE, 0] {
            let response = exchange(&service, VaultRequest::new(op), missing).await?;
            assert_eq!(response.status, Status::Unsupported as i32);
            assert!(response.package.is_empty());
        }
        let mut mixed = VaultRequest::new(op);
        mixed.profile_id = vec![1; 16];
        mixed.secret = b"SECRET_SENTINEL".to_vec();
        assert_eq!(
            exchange(&service, mixed, bits).await?.status,
            Status::Invalid as i32
        );
    }
    let mut oversized = VaultRequest::new(Op::PreviewPackage);
    oversized.secret = b"SECRET_SENTINEL".to_vec();
    oversized.package = vec![0; cshell_ipc::MAX_CREDENTIAL_PACKAGE_BYTES + 1];
    assert_eq!(
        exchange(&service, oversized, bits).await?.status,
        Status::Invalid as i32
    );
    let mut mixed_status = VaultRequest::new(Op::Status);
    mixed_status.package = vec![1];
    assert_eq!(
        exchange(&service, mixed_status, bits).await?.status,
        Status::Invalid as i32
    );
    let response = exchange(
        &service,
        VaultRequest::new(Op::Status),
        features::VAULT_CONTROL,
    )
    .await?;
    assert_eq!(response.status, Status::Ok as i32);
    assert!(!response.exists);
    Ok(())
}
