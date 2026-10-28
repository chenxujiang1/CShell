use cshell_application::{
    MAX_TUNNEL_DOCUMENT, TunnelDedicatedConfiguration, TunnelDefinition, TunnelReplyExpectation,
    decode_tunnels,
};
use cshell_domain::{SessionId, TunnelId};
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TunnelOperation, TunnelRequest,
    TunnelStatus, client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::{io::Read, time::Duration};
fn id(value: &str) -> Result<[u8; 16], &'static str> {
    let id: SessionId = serde_json::from_value(serde_json::Value::String(value.into()))
        .map_err(|_| "invalid tunnel/session UUID")?;
    if id.as_uuid().is_nil() {
        return Err("nil identity");
    }
    Ok(*id.as_uuid().as_bytes())
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args.is_empty() {
        return Err(crate::USAGE);
    }
    let mut request = TunnelRequest::default();
    match args[0].as_str() {
        "get" if args.len() == 1 => {}
        "create" if args.len() == 4 && args[3] == "--confirm" => {
            request.operation = TunnelOperation::Create as i32;
            request.tunnel_id = TunnelId::new().as_uuid().as_bytes().to_vec();
            request.expected_revision = args[1].parse().map_err(|_| "invalid catalog revision")?;
            request.document_json = definition(&args[2])?;
        }
        "update" if args.len() == 5 && args[4] == "--confirm" => {
            request.operation = TunnelOperation::Update as i32;
            request.tunnel_id = id(&args[1])?.to_vec();
            request.expected_revision = args[2].parse().map_err(|_| "invalid catalog revision")?;
            request.document_json = definition(&args[3])?;
        }
        "start" if args.len() == 5 && args[4] == "--confirm" => {
            request.operation = TunnelOperation::Start as i32;
            request.tunnel_id = id(&args[1])?.to_vec();
            request.expected_revision = args[2].parse().map_err(|_| "invalid catalog revision")?;
            request.session_id = id(&args[3])?.to_vec();
        }
        "configure-dedicated" if args.len() == 6 && args[5] == "--confirm" => {
            request.operation = TunnelOperation::ConfigureDedicated as i32;
            request.tunnel_id = id(&args[1])?.to_vec();
            request.expected_revision = args[2].parse().map_err(|_| "invalid catalog revision")?;
            request.session_id = id(&args[3])?.to_vec();
            request.document_json = configuration(&args[4])?;
        }
        "stop" | "remove" | "start-dedicated" | "clear-dedicated"
            if args.len() == 4 && args[3] == "--confirm" =>
        {
            request.operation = match args[0].as_str() {
                "stop" => TunnelOperation::Stop,
                "remove" => TunnelOperation::Remove,
                "start-dedicated" => TunnelOperation::StartDedicated,
                _ => TunnelOperation::ClearDedicated,
            } as i32;
            request.tunnel_id = id(&args[1])?.to_vec();
            request.expected_revision = args[2].parse().map_err(|_| "invalid catalog revision")?;
        }
        _ => return Err(crate::USAGE),
    }
    if request.operation != 0 {
        if request.expected_revision == 0 || request.expected_revision > i64::MAX as u64 {
            return Err("revision must be positive and bounded");
        }
        request.confirmed = true;
    }
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "authenticated daemon discovery missing")?;
    let reply = tokio::time::timeout(Duration::from_secs(40), async {
        #[cfg(windows)]
        let mut stream = cshell_ipc::transport::connect(&discovery.endpoint.to_string_lossy())
            .await
            .map_err(|_| "daemon connection failed")?;
        #[cfg(unix)]
        let mut stream = cshell_ipc::transport::connect(std::path::Path::new(&discovery.endpoint))
            .await
            .map_err(|_| "daemon connection failed")?;
        let mut handshake = Handshake::new(
            discovery.daemon_instance_id.to_vec(),
            discovery.instance_token.to_vec(),
        );
        let required = features::TUNNEL_CONTROL
            | if request.operation >= 6 {
                features::DEDICATED_TUNNELS
            } else {
                0
            };
        handshake.feature_bits = features::TUNNEL_CONTROL | features::DEDICATED_TUNNELS;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & required != required {
            return Err("daemon lacks tunnel management");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TunnelRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "tunnel send uncertain; get before another mutation")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "tunnel reply uncertain; get before another mutation")?;
        if reply.request_id != 2 {
            return Err("tunnel reply identity mismatch");
        }
        let Some(envelope::Payload::TunnelResponse(reply)) = reply.payload else {
            return Err("unexpected tunnel reply");
        };
        if reply.document_json.len() > MAX_TUNNEL_DOCUMENT || reply.detail.len() > 1024 {
            return Err("tunnel reply limit");
        }
        if reply.status != TunnelStatus::Ok as i32 {
            eprintln!("{}", reply.detail);
            return Err("tunnel request refused or uncertain; inspect without replaying");
        }
        let snapshot = decode_tunnels(&reply.document_json)?;
        if request.operation != 0 {
            let id = TunnelId::from_bytes(
                request
                    .tunnel_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| "invalid tunnel reply scope")?,
            );
            let definition: Option<TunnelDefinition> = if matches!(
                TunnelOperation::try_from(request.operation),
                Ok(TunnelOperation::Create | TunnelOperation::Update)
            ) {
                Some(
                    serde_json::from_slice(&request.document_json)
                        .map_err(|_| "invalid definition")?,
                )
            } else {
                None
            };
            let expected = match TunnelOperation::try_from(request.operation)
                .map_err(|_| "invalid operation")?
            {
                TunnelOperation::Create | TunnelOperation::Update => TunnelReplyExpectation::Saved(
                    id,
                    definition.as_ref().ok_or("missing definition")?,
                ),
                TunnelOperation::Start => TunnelReplyExpectation::Starting(
                    id,
                    SessionId::from_bytes(
                        request
                            .session_id
                            .as_slice()
                            .try_into()
                            .map_err(|_| "invalid session")?,
                    ),
                ),
                TunnelOperation::Stop => TunnelReplyExpectation::Stopping(id),
                TunnelOperation::Remove => TunnelReplyExpectation::Removed(id),
                TunnelOperation::ConfigureDedicated => {
                    let configuration: TunnelDedicatedConfiguration =
                        serde_json::from_slice(&request.document_json)
                            .map_err(|_| "invalid dedicated configuration")?;
                    snapshot.confirm_reply(
                        request.expected_revision,
                        TunnelReplyExpectation::DedicatedConfigured(id, &configuration),
                    )?;
                    if snapshot
                        .items
                        .iter()
                        .find(|r| r.id == id)
                        .and_then(|r| r.dedicated.as_ref())
                        .is_none_or(|p| {
                            p.reference_session.as_uuid().as_bytes().as_slice()
                                != request.session_id.as_slice()
                        })
                    {
                        return Err("dedicated reference scope mismatch");
                    }
                    return Ok(snapshot);
                }
                TunnelOperation::StartDedicated => TunnelReplyExpectation::DedicatedStarting(id),
                TunnelOperation::ClearDedicated => TunnelReplyExpectation::DedicatedCleared(id),
                _ => return Err("invalid mutation"),
            };
            snapshot.confirm_reply(request.expected_revision, expected)?;
        }
        Ok(snapshot)
    })
    .await
    .map_err(|_| "tunnel request timed out; get before another mutation")??;
    println!(
        "{}",
        serde_json::to_string_pretty(&reply).map_err(|_| "tunnel encoding failed")?
    );
    Ok(())
}
fn definition(path: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = input(path)?;
    let value: TunnelDefinition =
        serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
            .map_err(|_| "invalid tunnel JSON")?;
    value.validate()?;
    serde_json::to_vec(&value).map_err(|_| "definition encoding failed")
}
fn configuration(path: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = input(path)?;
    let value: TunnelDedicatedConfiguration =
        serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
            .map_err(|_| "invalid dedicated configuration JSON")?;
    value.validate()?;
    serde_json::to_vec(&value).map_err(|_| "dedicated configuration encoding failed")
}
fn input(path: &str) -> Result<Vec<u8>, &'static str> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| "definition unavailable")?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 16384 {
        return Err("definition must be a bounded regular JSON file");
    }
    let file = std::fs::File::open(path).map_err(|_| "definition unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "definition unavailable")?
        .is_file()
    {
        return Err("invalid definition file");
    }
    let mut bytes = Vec::new();
    file.take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| "definition read failed")?;
    if bytes.len() > 16384 {
        return Err("definition limit");
    }
    Ok(bytes)
}
