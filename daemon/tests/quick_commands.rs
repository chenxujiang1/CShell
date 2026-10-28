use cshell_application::{decode_quick_commands, encode_quick_commands};
use cshell_domain::{InputAction, QuickCommandCatalog, SessionId};
use cshell_ipc::{
    Envelope, QuickCommandOperation, QuickCommandRequest, QuickCommandStatus,
    TerminalControlStatus, TerminalInputRequest, envelope, features, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::{error::Error, sync::Arc};

async fn exchange(
    service: &SessionIpcService,
    payload: envelope::Payload,
    bits: u64,
) -> Result<Envelope, Box<dyn Error>> {
    let (mut client, server) = tokio::io::duplex(2 * 1024 * 1024);
    let request = async move {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 81,
                payload: Some(payload),
                ..Default::default()
            },
        )
        .await?;
        let response = read_envelope(&mut client).await?;
        assert_eq!(response.request_id, 81);
        Ok::<_, cshell_ipc::IpcError>(response)
    };
    let (served, response) = tokio::join!(
        service.serve_connection_with_features(server, bits),
        request
    );
    served?;
    Ok(response?)
}
#[tokio::test]
async fn quick_commands_require_capability_validate_payload_and_reject_stale_saves()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        directory.path().join("journals"),
        4,
    )?))
    .with_profiles(Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(directory.path().join("commands.sqlite")).await?,
    )));
    let save = || -> Result<QuickCommandRequest, Box<dyn Error>> {
        Ok(QuickCommandRequest {
            operation: QuickCommandOperation::Save as i32,
            expected_revision: 0,
            document_json: encode_quick_commands(&QuickCommandCatalog::default())?,
        })
    };
    let reply = exchange(&service, envelope::Payload::QuickCommandRequest(save()?), 0).await?;
    assert!(
        matches!(reply.payload,Some(envelope::Payload::QuickCommandResponse(ref response)) if response.status==QuickCommandStatus::Unsupported as i32)
    );
    let reply = exchange(
        &service,
        envelope::Payload::QuickCommandRequest(save()?),
        features::QUICK_COMMAND_CONTROL,
    )
    .await?;
    let Some(envelope::Payload::QuickCommandResponse(response)) = reply.payload else {
        return Err("missing saved catalog".into());
    };
    assert_eq!(response.status, QuickCommandStatus::Ok as i32);
    assert_eq!(response.revision, 1);
    assert_eq!(
        decode_quick_commands(&response.document_json)?,
        QuickCommandCatalog::default()
    );
    let reply = exchange(
        &service,
        envelope::Payload::QuickCommandRequest(save()?),
        features::QUICK_COMMAND_CONTROL,
    )
    .await?;
    assert!(
        matches!(reply.payload,Some(envelope::Payload::QuickCommandResponse(ref response)) if response.status==QuickCommandStatus::Conflict as i32)
    );
    let invalid = QuickCommandRequest {
        operation: QuickCommandOperation::Save as i32,
        expected_revision: 1,
        document_json: b"invalid".to_vec(),
    };
    let reply = exchange(
        &service,
        envelope::Payload::QuickCommandRequest(invalid),
        features::QUICK_COMMAND_CONTROL,
    )
    .await?;
    assert!(
        matches!(reply.payload,Some(envelope::Payload::QuickCommandResponse(ref response)) if response.status==QuickCommandStatus::Invalid as i32)
    );
    let reply = exchange(
        &service,
        envelope::Payload::QuickCommandRequest(QuickCommandRequest::default()),
        features::QUICK_COMMAND_CONTROL,
    )
    .await?;
    assert!(
        matches!(reply.payload,Some(envelope::Payload::QuickCommandResponse(ref response)) if response.status==QuickCommandStatus::Ok as i32 && response.revision==1)
    );
    let compose = TerminalInputRequest::from_action(
        SessionId::new(),
        &InputAction::Compose {
            text: "echo test".into(),
            append_enter: true,
        },
    )?;
    let reply = exchange(
        &service,
        envelope::Payload::TerminalInputRequest(compose),
        features::TERMINAL_CONTROL,
    )
    .await?;
    assert!(
        matches!(reply.payload,Some(envelope::Payload::TerminalControlResponse(ref response)) if response.status==TerminalControlStatus::InvalidRequest as i32)
    );
    Ok(())
}
