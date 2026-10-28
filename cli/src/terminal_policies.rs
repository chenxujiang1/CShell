use cshell_application::*;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TerminalPolicyOperation as Op,
    TerminalPolicyRequest as Request, TerminalPolicyStatus as Status, client_handshake, envelope,
    features, read_envelope, write_envelope,
};
use std::{
    io::{Read, Write},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn read(path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "cannot open policy input")?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect policy input")?
        .is_file()
    {
        return Err("policy input must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read policy input")?;
    if bytes.len() > limit {
        return Err("policy input size limit");
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
        tempfile::NamedTempFile::new_in(parent).map_err(|_| "cannot stage policy preview")?;
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
    if let [op, path, rows, cols, confirm] = args
        && op == "launch"
        && confirm == "--confirm"
    {
        return launch(path, rows, cols).await;
    }
    let mut proposal = None;
    let mut review = None;
    let mut context = None;
    let mut output = None;
    let request = match args {
        [op] if op == "get" => Request {
            operation: Op::Get as i32,
            ..Default::default()
        },
        [op, path, destination] if op == "preview" => {
            let parsed: TerminalPolicyProposal =
                serde_json::from_slice(&read(path, MAX_TERMINAL_POLICY_BYTES + 1024)?)
                    .map_err(|_| "invalid policy proposal")?;
            parsed.fingerprint()?;
            let request = Request {
                operation: Op::Preview as i32,
                expected_revision: parsed.catalog.revision,
                profile_revision: parsed.profile_revision,
                host_revision: parsed.host_revision,
                document_json: serde_json::to_vec(&parsed).map_err(|_| "policy encoding failed")?,
                ..Default::default()
            };
            proposal = Some(parsed);
            output = Some(destination);
            request
        }
        [op, path, confirm] if op == "apply" && confirm == "--confirm" => {
            let value: TerminalPolicyReply =
                serde_json::from_slice(&read(path, MAX_TERMINAL_POLICY_REPLY)?)
                    .map_err(|_| "invalid policy preview")?;
            let TerminalPolicyReply::Preview(preview) = value else {
                return Err("file must contain a policy preview");
            };
            let sha = preview.fingerprint()?;
            if SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "clock unavailable")?
                .as_secs()
                >= preview.expires_unix
            {
                return Err("policy preview expired; preview again");
            }
            let request = Request {
                operation: Op::Apply as i32,
                expected_revision: preview.proposal.catalog.revision,
                profile_revision: preview.proposal.profile_revision,
                host_revision: preview.proposal.host_revision,
                preview_token: preview.token.to_vec(),
                review_sha256: sha.to_vec(),
                confirmed: true,
                ..Default::default()
            };
            review = Some(preview);
            request
        }
        [op, path, policy_rev, profile_rev, host_rev] if op == "resolve" => {
            let parsed: TerminalResolutionContext = serde_json::from_slice(&read(path, 8192)?)
                .map_err(|_| "invalid terminal context")?;
            parsed.validate()?;
            let request = Request {
                operation: Op::Resolve as i32,
                expected_revision: policy_rev.parse().map_err(|_| "invalid policy revision")?,
                profile_revision: profile_rev
                    .parse()
                    .map_err(|_| "invalid Profile revision")?,
                host_revision: host_rev.parse().map_err(|_| "invalid Host revision")?,
                document_json: serde_json::to_vec(&parsed)
                    .map_err(|_| "context encoding failed")?,
                ..Default::default()
            };
            context = Some(parsed);
            request
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
        hello.feature_bits = features::TERMINAL_POLICIES;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & features::TERMINAL_POLICIES == 0 {
            return Err("daemon lacks terminal policy capability");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TerminalPolicyRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(
            |_| "policy send outcome unknown; reload and create a new preview, do not replay apply",
        )?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "policy reply outcome unknown; reload before a new preview")?;
        if response.request_id != 2 {
            return Err("policy reply identity mismatch");
        }
        let Some(envelope::Payload::TerminalPolicyResponse(response)) = response.payload else {
            return Err("unexpected policy reply");
        };
        if response.detail.len() > 4096 {
            return Err("policy reply detail limit");
        }
        if response.status != Status::Ok as i32 {
            eprintln!("{}", response.detail);
            return Err("policy request refused or uncertain; reload and prepare a new preview");
        }
        let expectation =
            match Op::try_from(request.operation).map_err(|_| "invalid policy operation")? {
                Op::Get => TerminalPolicyExpectation::Catalog,
                Op::Preview => TerminalPolicyExpectation::Preview(
                    proposal.as_ref().ok_or("missing policy proposal")?,
                ),
                Op::Apply => TerminalPolicyExpectation::Saved(
                    review.as_ref().ok_or("missing policy review")?,
                ),
                Op::Resolve => TerminalPolicyExpectation::Resolved {
                    context: context.as_ref().ok_or("missing terminal context")?,
                    profile_revision: request.profile_revision,
                    host_revision: request.host_revision,
                    policy_revision: request.expected_revision,
                },
            };
        decode_terminal_policy_reply(&response.document_json, expectation)
            .map_err(|_| "policy reply scope/effect mismatch")
    })
    .await
    .map_err(|_| "policy operation timed out; reload, do not replay apply")??;
    let bytes = serde_json::to_vec_pretty(&reply).map_err(|_| "policy reply encoding failed")?;
    if let Some(path) = output {
        publish(path, &bytes)?;
    } else {
        println!(
            "{}",
            std::str::from_utf8(&bytes).map_err(|_| "policy reply encoding failed")?
        );
    }
    Ok(())
}

async fn launch(path: &str, rows: &str, cols: &str) -> Result<(), &'static str> {
    let bytes = read(path, MAX_TERMINAL_POLICY_REPLY)?;
    let report = decode_terminal_resolution_report(&bytes)
        .or_else(|_| {
            let reply: TerminalPolicyReply =
                serde_json::from_slice(&bytes).map_err(|_| "invalid resolution review")?;
            let TerminalPolicyReply::Resolved(report) = reply else {
                return Err("file must contain a resolution report".into());
            };
            decode_terminal_resolution_report(
                &serde_json::to_vec(&report).map_err(|_| "report encoding failed")?,
            )
        })
        .map_err(|_| "invalid resolution review")?;
    let rows: u32 = rows.parse().map_err(|_| "invalid rows")?;
    let cols: u32 = cols.parse().map_err(|_| "invalid columns")?;
    if rows == 0
        || cols == 0
        || rows > u16::MAX as u32
        || cols > u16::MAX as u32
        || u64::from(rows) * u64::from(cols) > 1_000_000
    {
        return Err("terminal dimensions exceed the limit");
    }
    let request = cshell_ipc::TerminalContextLaunchRequest {
        connection_diagnostics:false,
        report_json: serde_json::to_vec(&report).map_err(|_| "report encoding failed")?,
        rows,
        cols,
        confirmed: true,
    };
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "authenticated daemon discovery missing")?;
    let session = tokio::time::timeout(Duration::from_secs(300), async {
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
        hello.feature_bits = features::TERMINAL_CONTEXT_LAUNCH
            | features::VAULT_CONTROL
            | features::LOCAL_PROFILE
            | features::SSH_PROFILE_SESSION;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & features::TERMINAL_CONTEXT_LAUNCH == 0 {
            return Err("daemon lacks terminal context launch capability");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TerminalContextLaunchRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "launch outcome unknown; list sessions before any manual retry")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "launch outcome unknown; list sessions before any manual retry")?;
        if response.request_id != 2 {
            return Err("launch reply identity mismatch; outcome unknown");
        }
        let Some(envelope::Payload::TerminalContextLaunchResponse(response)) = response.payload
        else {
            return Err("launch reply type mismatch; outcome unknown");
        };
        let result = response
            .result
            .ok_or("launch receipt missing; outcome unknown")?;
        if result.detail.len() > 4096 {
            return Err("launch reply detail limit; outcome unknown");
        }
        let Some(session) = result.session else {
            if !response.report_json.is_empty() {
                return Err("launch failure receipt invalid; outcome unknown");
            }
            eprintln!("{}", result.detail);
            return Err("launch refused or failed; review current sessions before retrying");
        };
        let receipt = decode_terminal_resolution_report(&response.report_json)
            .map_err(|_| "launch context receipt invalid; outcome unknown")?;
        if receipt != report
            || result.failure_code != cshell_ipc::SessionFailureCode::None as i32
            || !result.detail.is_empty()
            || session.profile_id.as_deref()
                != Some(report.context.profile_id.as_uuid().as_bytes().as_slice())
            || session.title != report.profile.name
            || session.session_id.len() != 16
            || session.session_id.iter().all(|b| *b == 0)
        {
            return Err("launch receipt scope/effect mismatch; outcome unknown");
        }
        Ok(session)
    })
    .await
    .map_err(|_| "launch timed out; outcome unknown, list sessions before any manual retry")??;
    let receipt = serde_json::json!({"session_id":cshell_domain::SessionId::from_bytes(session.session_id.as_slice().try_into().map_err(|_|"invalid session ID")?),"title":session.title,"resolution":report});
    println!(
        "{}",
        serde_json::to_string_pretty(&receipt).map_err(|_| "launch receipt encoding failed")?
    );
    Ok(())
}
