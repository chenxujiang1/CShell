//! Offline authoring and explicitly confirmed local execution, without daemon discovery.
use cshell_application::*;
use cshell_domain::*;
use cshell_script_runtime::*;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
};
use zeroize::Zeroizing;

fn read(path: &str) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "cannot open script draft input")?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect script draft input")?
        .is_file()
    {
        return Err("script draft input must be a regular file");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((MAX_SCRIPT_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read script draft input")?;
    if bytes.len() > MAX_SCRIPT_DOCUMENT_BYTES {
        return Err("script draft input limit");
    }
    Ok(bytes)
}
fn publish<T: serde::Serialize>(path: &str, value: &T) -> Result<(), &'static str> {
    let bytes =
        Zeroizing::new(serde_json::to_vec_pretty(value).map_err(|_| "cannot encode script draft")?);
    if bytes.len() > MAX_SCRIPT_DOCUMENT_BYTES {
        return Err("encoded script draft limit");
    }
    let path = std::path::Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| "cannot stage script draft output")?;
    file.write_all(&bytes)
        .map_err(|_| "script draft output write failed")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "script draft output sync failed")?;
    file.persist_noclobber(path)
        .map_err(|_| "output exists or script draft publication failed")?;
    Ok(())
}
fn error(error: ScriptDraftError) -> &'static str {
    eprintln!("{error}");
    "script draft operation refused"
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    match args {
        [op, input, selected, scope, output] if op == "live-preview" => {
            let document = decode_script_document(&read(input)?).map_err(error)?;
            let selected =
                serde_json::from_slice(&read(selected)?).map_err(|_| "invalid capabilities")?;
            let scope = serde_json::from_slice(&read(scope)?).map_err(|_| "invalid live scope")?;
            let proposal = ScriptLiveProposal {
                document,
                selected,
                scope,
            };
            validate_live_proposal(&proposal).map_err(worker_error)?;
            let reply = query(ScriptControlCommand::Preview { proposal }).await?;
            let ScriptControlReply::Review(review) = reply else {
                return Err("invalid review reply");
            };
            publish(output, &review)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&review).map_err(|_| "cannot encode live review")?
            );
        }
        [op, review, parameters, confirm] if op == "live-start" && confirm == "--confirm" => {
            let review: ScriptLiveReview =
                serde_json::from_slice(&read(review)?).map_err(|_| "invalid live review")?;
            if review.version != 1 {
                return Err("unsupported review version");
            }
            let parameters = serde_json::from_slice(&read(parameters)?)
                .map_err(|_| "invalid parameter document")?;
            let reply = query(ScriptControlCommand::Start {
                review: review.review,
                parameters,
                confirmed: true,
            })
            .await?;
            if !matches!(&reply, ScriptControlReply::Run(v) if v.document_sha256 == review.plan.document_sha256)
            {
                return Err("live start receipt mismatch; list runs, do not replay");
            }
            print_reply(&reply)?;
        }
        [op, run] if op == "live-get" || op == "live-revoke" => {
            let run = serde_json::from_value(serde_json::Value::String(run.clone()))
                .map_err(|_| "invalid run UUID")?;
            let command = if op == "live-get" {
                ScriptControlCommand::Get { run }
            } else {
                ScriptControlCommand::Revoke { run }
            };
            print_reply(&query(command).await?)?;
        }
        [op] if op == "live-list" => print_reply(&query(ScriptControlCommand::List).await?)?,
        [op, confirm] if op == "recover" && confirm == "--confirm" => {
            let report = recover_script_resources_bounded()
                .await
                .map_err(worker_error)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|_| "cannot encode recovery receipt")?
            );
            if report.unresolved != 0 {
                return Err("unproved script resources remain occupied; no signals were sent");
            }
        }
        [op, input] if op == "inspect" => {
            let document = decode_script_document(&read(input)?).map_err(error)?;
            let plan = plan_script_permissions(&document, &BTreeSet::new()).map_err(error)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).map_err(|_| "cannot encode permission plan")?
            );
        }
        [op, input, selected] if op == "permissions" => {
            let document = decode_script_document(&read(input)?).map_err(error)?;
            let capabilities: BTreeSet<ScriptCapability> = serde_json::from_slice(&read(selected)?)
                .map_err(|_| "capabilities must be a JSON array of known capability names")?;
            let plan = plan_script_permissions(&document, &capabilities).map_err(error)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).map_err(|_| "cannot encode permission plan")?
            );
        }
        [op, output] if op == "recording-new" => publish(output, &ScriptRecording::default())?,
        [op, input] if op == "recording-inspect" => {
            let recording = decode_script_recording(&read(input)?).map_err(error)?;
            let rendered = render_script_recording(&recording);
            println!(
                "{}",
                serde_json::json!({
                    "steps": recording.steps.len(), "truncated": recording.truncated,
                    "omissions": recording.steps.iter().filter(|s| matches!(s, ScriptRecordingStep::Omitted { .. })).count(),
                    "can_generate": rendered.is_ok(), "live_execution_enabled": false,
                })
            );
        }
        [op, input, output] if op == "recording-render" => {
            let recording = decode_script_recording(&read(input)?).map_err(error)?;
            let document = render_script_recording(&recording).map_err(error)?;
            publish(output, &document)?;
            let plan = plan_script_permissions(&document, &BTreeSet::new()).map_err(error)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).map_err(|_| "cannot encode permission plan")?
            );
        }
        [op, input, selected, parameters, confirm]
            if op == "run-offline" && confirm == "--confirm" =>
        {
            let document = decode_script_document(&read(input)?).map_err(error)?;
            let capabilities: BTreeSet<ScriptCapability> = serde_json::from_slice(&read(selected)?)
                .map_err(|_| "invalid capability selection")?;
            let parameters: ScriptParameterValues = serde_json::from_slice(&read(parameters)?)
                .map_err(|_| "invalid script parameter document")?;
            let plan = plan_script_permissions(&document, &capabilities).map_err(error)?;
            let request =
                OfflineScriptRun::review(document, plan, parameters).map_err(worker_error)?;
            let cancel = ScriptCancellation::default();
            let running = run_offline(request, cancel.clone());
            tokio::pin!(running);
            let report = tokio::select! {
                result = &mut running => result,
                _ = tokio::signal::ctrl_c() => {
                    cancel.cancel();
                    running.await
                }
            }
            .map_err(worker_error)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .map_err(|_| "cannot encode script run report")?
            );
            if report.status != ScriptRunStatus::Completed
                || !report.exit_confirmed
                || !report.sandbox_cleanup_confirmed
                || !report.effects_confirmed
            {
                return Err(
                    "offline script run or containment cleanup did not complete successfully; see run report",
                );
            }
        }
        _ => return Err(crate::USAGE),
    }
    Ok(())
}
async fn query(command: ScriptControlCommand) -> Result<ScriptControlReply, &'static str> {
    let request = command.request().map_err(worker_error)?;
    let reply = crate::terminal_tools::exchange(
        cshell_ipc::envelope::Payload::ScriptControlRequest(request),
        cshell_ipc::features::SCRIPT_CONTROL,
    )
    .await?;
    let cshell_ipc::envelope::Payload::ScriptControlResponse(reply) = reply else {
        return Err("invalid script response");
    };
    if reply.status != cshell_ipc::ScriptControlStatus::Ok as i32 {
        return Err("script request refused; read current state, do not replay start");
    }
    if reply.document_json.len() > 256 * 1024 {
        return Err("script response limit");
    }
    let reply = serde_json::from_slice(&reply.document_json)
        .map_err(|_| "invalid script response document")?;
    validate_script_control_reply(&command, &reply).map_err(worker_error)?;
    Ok(reply)
}
fn print_reply(reply: &ScriptControlReply) -> Result<(), &'static str> {
    println!(
        "{}",
        serde_json::to_string_pretty(reply).map_err(|_| "cannot encode script receipt")?
    );
    Ok(())
}
fn worker_error(error: ScriptWorkerError) -> &'static str {
    eprintln!("{error}");
    "offline script run refused"
}
