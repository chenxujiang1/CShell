use cshell_application::*;
use cshell_domain::{ClipboardHost, HostAuthorityId};
use cshell_ipc::{
    CaOperation, CaRequest, CaStatus, DiscoveryRecord, Envelope, Handshake, RuntimePaths,
    client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::{io::Read, time::Duration};
fn id(value: &str) -> Result<HostAuthorityId, &'static str> {
    let id: HostAuthorityId = serde_json::from_value(serde_json::Value::String(value.into()))
        .map_err(|_| "invalid host CA UUID")?;
    if id.as_uuid().is_nil() {
        return Err("nil host CA UUID");
    }
    Ok(id)
}
fn revision(value: &str) -> Result<u64, &'static str> {
    let n = value
        .parse::<u64>()
        .map_err(|_| "invalid CA catalog revision")?;
    if n == 0 || n > i64::MAX as u64 {
        return Err("invalid CA revision");
    }
    Ok(n)
}
fn input(path: &str, maximum: usize) -> Result<Vec<u8>, &'static str> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| "CA input unavailable")?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > maximum as u64 {
        return Err("CA input must be a bounded regular file");
    }
    let file = std::fs::File::open(path).map_err(|_| "CA input unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "CA input unavailable")?
        .is_file()
    {
        return Err("invalid CA input file");
    }
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "CA input read failed")?;
    if bytes.len() > maximum {
        return Err("CA input limit");
    }
    Ok(bytes)
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args.is_empty() {
        return Err(crate::USAGE);
    }
    let mut request = CaRequest::default();
    let mut review = None;
    match args[0].as_str() {
        "get" if args.len() == 1 => {}
        "preview" if args.len() == 4 => {
            let bytes = input(&args[3], 24 * 1024)?;
            let mut draft: HostCaDraft =
                serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
                    .map_err(|_| "invalid CA draft JSON")?;
            draft.public_key = draft.public_key.trim().into();
            draft.validate()?;
            request.operation = CaOperation::PreviewSave as i32;
            request.authority_id = if args[1] == "new" {
                HostAuthorityId::new()
            } else {
                id(&args[1])?
            }
            .as_uuid()
            .as_bytes()
            .to_vec();
            request.expected_revision = revision(&args[2])?;
            request.document_json =
                serde_json::to_vec(&draft).map_err(|_| "CA draft encoding failed")?;
        }
        "preview-remove" if args.len() == 3 => {
            request.operation = CaOperation::PreviewRemove as i32;
            request.authority_id = id(&args[1])?.as_uuid().as_bytes().to_vec();
            request.expected_revision = revision(&args[2])?;
        }
        "apply" if args.len() == 4 && args[3] == "--confirm" => {
            let bytes = input(&args[1], MAX_HOST_CA_DOCUMENT)?;
            let value =
                decode_host_ca_reply(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))?;
            let HostCaReply::Preview(p) = value else {
                return Err("apply input must be a saved CA preview");
            };
            request.operation = if p.record.is_some() {
                CaOperation::ApplySave
            } else {
                CaOperation::ApplyRemove
            } as i32;
            request.authority_id = p.id.as_uuid().as_bytes().to_vec();
            request.expected_revision = p.catalog_revision;
            request.preview_token = p.token.to_vec();
            request.confirmed_fingerprint = args[2].clone();
            request.confirmed = true;
            review = Some(p);
        }
        "inspect-certificate" if args.len() == 2 || args.len() == 4 => {
            let bytes = input(&args[1], 65536)?;
            let certificate =
                String::from_utf8(bytes).map_err(|_| "certificate must be UTF-8 public text")?;
            let target = if args.len() == 4 {
                Some(
                    ClipboardHost::new(
                        &args[2],
                        args[3]
                            .parse()
                            .map_err(|_| "invalid certificate target port")?,
                    )
                    .ok_or("invalid certificate target")?,
                )
            } else {
                None
            };
            request.operation = CaOperation::InspectCertificate as i32;
            request.document_json = serde_json::to_vec(&HostCertificateRequest {
                certificate,
                target,
            })
            .map_err(|_| "certificate request encoding failed")?;
        }
        _ => return Err(crate::USAGE),
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
        let mut hello = Handshake::new(
            discovery.daemon_instance_id.to_vec(),
            discovery.instance_token.to_vec(),
        );
        hello.feature_bits = features::HOST_CA_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & features::HOST_CA_CONTROL == 0 {
            return Err("daemon lacks host CA management");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::CaRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "CA send uncertain; reload, do not replay an apply")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "CA reply uncertain; reload, do not replay an apply")?;
        if response.request_id != 2 {
            return Err("CA response identity mismatch");
        }
        let Some(envelope::Payload::CaResponse(response)) = response.payload else {
            return Err("unexpected CA response");
        };
        if response.detail.len() > 1024 {
            return Err("CA detail limit");
        }
        if response.status != CaStatus::Ok as i32 {
            eprintln!("{}", response.detail);
            return Err("CA request refused or uncertain; get and prepare a new preview");
        }
        let reply = decode_host_ca_reply(&response.document_json)?;
        let authority = if request.authority_id.is_empty() {
            None
        } else {
            Some(HostAuthorityId::from_bytes(
                request
                    .authority_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| "invalid authority scope")?,
            ))
        };
        match CaOperation::try_from(request.operation).map_err(|_| "invalid CA operation")? {
            CaOperation::Get => reply.confirm(HostCaReplyExpectation::Catalog)?,
            CaOperation::PreviewSave => {
                let draft: HostCaDraft = serde_json::from_slice(&request.document_json)
                    .map_err(|_| "invalid saved CA draft")?;
                reply.confirm(HostCaReplyExpectation::PreviewSave(
                    authority.ok_or("missing CA scope")?,
                    request.expected_revision,
                    &draft,
                ))?;
            }
            CaOperation::PreviewRemove => reply.confirm(HostCaReplyExpectation::PreviewRemove(
                authority.ok_or("missing CA scope")?,
                request.expected_revision,
            ))?,
            CaOperation::ApplySave | CaOperation::ApplyRemove => reply.confirm(
                HostCaReplyExpectation::Applied(review.as_ref().ok_or("missing CA review")?),
            )?,
            CaOperation::InspectCertificate => {
                let input: HostCertificateRequest = serde_json::from_slice(&request.document_json)
                    .map_err(|_| "invalid certificate scope")?;
                reply.confirm(HostCaReplyExpectation::Certificate(&input))?;
            }
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "CA request timed out; get and prepare a new preview, do not replay apply")??;
    println!(
        "{}",
        serde_json::to_string_pretty(&reply).map_err(|_| "CA reply encoding failed")?
    );
    Ok(())
}
