use cshell_application::*;
use cshell_domain::ProfileId;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, ProfileBatchOperation as Op,
    ProfileBatchRequest as Request, ProfileBatchStatus as Status, RuntimePaths, client_handshake,
    envelope, features, read_envelope, write_envelope,
};
use std::{
    io::{Read, Write},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn read(path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "cannot open batch input")?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect batch input")?
        .is_file()
    {
        return Err("batch input must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read batch input")?;
    if bytes.len() > limit {
        return Err("batch input size limit");
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes.drain(..3);
    }
    Ok(bytes)
}
fn publish(path: &str, bytes: &[u8]) -> Result<(), &'static str> {
    let path = std::path::Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| "cannot stage batch preview")?;
    temporary
        .write_all(bytes)
        .map_err(|_| "preview write failed")?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| "preview sync failed")?;
    temporary
        .persist_noclobber(path)
        .map_err(|_| "preview exists or publication failed")?;
    Ok(())
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    let mut draft = None;
    let mut review = None;
    let mut output = None;
    let request = match args {
        [op] if op == "reload" => Request {
            operation: Op::Reload as i32,
            ..Default::default()
        },
        [op, path, destination] if op == "preview" => {
            let parsed = decode_profile_batch(&read(path, MAX_PROFILE_BATCH_BYTES)?)?;
            let document_json = serde_json::to_vec(&parsed).map_err(|_| "batch encoding failed")?;
            let revision = parsed.revision;
            draft = Some(parsed);
            output = Some(destination);
            Request {
                operation: Op::Preview as i32,
                expected_revision: revision,
                document_json,
                ..Default::default()
            }
        }
        [op, path, confirm] if op == "apply" && confirm == "--confirm" => {
            let value: ProfileBatchReply =
                serde_json::from_slice(&read(path, MAX_PROFILE_BATCH_REPLY)?)
                    .map_err(|_| "invalid batch preview file")?;
            let ProfileBatchReply::Preview(preview) = value else {
                return Err("file must contain a batch preview");
            };
            let sha = preview
                .plan
                .fingerprint()
                .map_err(|_| "invalid batch review plan")?;
            if preview.token == [0; 32]
                || SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| "preview clock unavailable")?
                    .as_secs()
                    >= preview.expires_unix
            {
                return Err("batch preview expired or invalid; preview again");
            }
            let request = Request {
                operation: Op::Apply as i32,
                expected_revision: preview.plan.revision,
                preview_token: preview.token.to_vec(),
                review_sha256: sha.to_vec(),
                confirmed: true,
                ..Default::default()
            };
            review = Some(preview);
            request
        }
        [op, profile] if op == "inspect" => inspect(profile, 0)?,
        [op, profile, revision] if op == "inspect" => {
            inspect(profile, revision.parse().map_err(|_| "invalid revision")?)?
        }
        _ => return Err(crate::USAGE),
    };
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
        hello.feature_bits = features::PROFILE_BATCH;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & features::PROFILE_BATCH == 0 {
            return Err("daemon lacks Profile batch capability");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::ProfileBatchRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(
            |_| "batch send outcome unknown; reload and create a new preview, do not replay apply",
        )?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "batch reply outcome unknown; reload before a new preview")?;
        if response.request_id != 2 {
            return Err("batch reply identity mismatch");
        }
        let Some(envelope::Payload::ProfileBatchResponse(response)) = response.payload else {
            return Err("unexpected batch reply");
        };
        if response.detail.len() > 4096 {
            return Err("batch reply detail limit");
        }
        if response.status != Status::Ok as i32 {
            eprintln!("{}", response.detail);
            return Err("batch request refused or uncertain; reload and prepare a new preview");
        }
        let expectation =
            match Op::try_from(request.operation).map_err(|_| "invalid batch operation")? {
                Op::Preview => ProfileBatchExpectation::Preview {
                    draft: draft.as_ref().ok_or("missing batch scope")?,
                    snapshot: None,
                },
                Op::Apply => {
                    ProfileBatchExpectation::Applied(review.as_ref().ok_or("missing batch review")?)
                }
                Op::Reload => ProfileBatchExpectation::Authority,
                Op::Inspect => ProfileBatchExpectation::Settings {
                    profile: ProfileId::from_bytes(
                        request
                            .profile_id
                            .as_slice()
                            .try_into()
                            .map_err(|_| "invalid Profile scope")?,
                    ),
                    revision: (request.expected_revision != 0).then_some(request.expected_revision),
                },
            };
        decode_profile_batch_reply(&response.document_json, expectation)
            .map_err(|_| "batch reply scope/effect mismatch")
    })
    .await
    .map_err(|_| "batch operation timed out; reload, do not replay apply")??;
    let bytes = serde_json::to_vec_pretty(&reply).map_err(|_| "batch reply encoding failed")?;
    if let Some(path) = output {
        publish(path, &bytes)?;
    } else {
        println!(
            "{}",
            std::str::from_utf8(&bytes).map_err(|_| "batch reply encoding failed")?
        );
    }
    Ok(())
}
fn inspect(value: &str, revision: u64) -> Result<Request, &'static str> {
    let id: ProfileId = serde_json::from_value(serde_json::Value::String(value.into()))
        .map_err(|_| "invalid Profile UUID")?;
    if id.as_uuid().is_nil() {
        return Err("nil Profile UUID");
    }
    Ok(Request {
        operation: Op::Inspect as i32,
        profile_id: id.as_uuid().as_bytes().to_vec(),
        expected_revision: revision,
        ..Default::default()
    })
}
