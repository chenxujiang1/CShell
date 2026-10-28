use cshell_application::*;
use cshell_domain::HostId;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TaskOperation, TaskRequest, TaskResponse,
    TaskStatus, client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::{collections::BTreeSet, time::Duration};
use tokio::io::AsyncReadExt;
use zeroize::Zeroizing;
async fn bounded(path: &str) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "cannot open mixed review/status document")?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_TASK_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "cannot read mixed document")?;
    if bytes.len() > MAX_TASK_SNAPSHOT_BYTES {
        return Err("mixed document exceeds limit");
    }
    Ok(bytes)
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args
        .first()
        .is_some_and(|command| command == "mixed-retry-confirmation")
    {
        if !(4..=6).contains(&args.len()) {
            return Err(crate::USAGE);
        }
        let flags: BTreeSet<_> = args[4..].iter().map(String::as_str).collect();
        if flags.len() != args.len() - 4
            || flags
                .iter()
                .any(|flag| !matches!(*flag, "--confirm-unknown" | "--confirm-files"))
        {
            return Err(crate::USAGE);
        }
        let plan = decode_mixed_snapshot(&bounded(&args[1]).await?)
            .map_err(|_| "invalid mixed review snapshot")?;
        let page = decode_mixed_status(&bounded(&args[2]).await?)
            .map_err(|_| "invalid mixed status page")?;
        page.check_snapshot(&plan)
            .map_err(|_| "mixed status does not match review")?;
        if page.active {
            return Err("parent has an active worker; reload after it stops");
        }
        let mut hosts = BTreeSet::new();
        for value in args[3].split(',') {
            let host = HostId::from_bytes(
                cshell_ipc::decode_id(&crate::id(value)?).map_err(|_| "invalid host ID")?,
            );
            if !hosts.insert(host) {
                return Err("duplicate selected host");
            }
        }
        let mut targets = Vec::new();
        let mut unknown = false;
        for host in hosts {
            let target = page
                .targets
                .iter()
                .find(|target| target.run.host_id == host)
                .ok_or("selected host absent from the supplied status page")?;
            if let Some(reason) = mixed_retry_block(&plan, &target.run) {
                return Err(match reason {
                    MixedRetryBlock::NotStopped => "selected target has not stopped",
                    MixedRetryBlock::Succeeded => "successful targets cannot be retried",
                    MixedRetryBlock::CommandNotIdempotent => {
                        "previously dispatched commands require original idempotent declarations"
                    }
                    MixedRetryBlock::UncertainFile => {
                        "repair uncertain file receipts in the original run first"
                    }
                    MixedRetryBlock::ActiveFile => "file intent is still active",
                });
            }
            unknown |= target.run.state == MixedTaskState::Unknown;
            targets.push(MixedTargetRevision {
                host_id: host,
                revision: target.run.revision,
            });
        }
        if unknown && !flags.contains("--confirm-unknown") {
            return Err("Unknown command replay needs --confirm-unknown");
        }
        if plan.retry_has_files() && !flags.contains("--confirm-files") {
            return Err(
                "file replay needs --confirm-files; this is not resuming old file receipts",
            );
        }
        let confirmation = MixedRetryConfirmation {
            format: "cshell.mixed-retry".into(),
            version: 1,
            run_id: plan.id(),
            snapshot_sha256: *plan.fingerprint(),
            targets,
            confirm_unknown: unknown,
            confirm_file_replay: plan.retry_has_files(),
        };
        confirmation
            .validate()
            .map_err(|_| "invalid retry confirmation")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&confirmation)
                .map_err(|_| "invalid retry confirmation")?
        );
        return Ok(());
    }
    if args.len() != 3 || args[0] != "mixed-retry" || args[2] != "--confirm" {
        return Err(crate::USAGE);
    }
    let confirmation = decode_mixed_retry_confirmation(&crate::document(&args[1]).await?)
        .map_err(|_| "invalid retry confirmation")?;
    let mut review = TaskRequest::default();
    review.operation = TaskOperation::MixedReview as i32;
    review.run_id = confirmation.run_id.as_uuid().as_bytes().to_vec();
    let reply = send(review).await?;
    let parent = decode_mixed_snapshot(&reply.document_json)
        .map_err(|_| "invalid source frozen snapshot")?;
    if parent.id() != confirmation.run_id || parent.fingerprint() != &confirmation.snapshot_sha256 {
        return Err("retry source snapshot changed");
    }
    let mut query = TaskRequest::default();
    query.operation = TaskOperation::MixedRetry as i32;
    query.run_id = confirmation.run_id.as_uuid().as_bytes().to_vec();
    query.confirmed = true;
    query.expected_targets_json =
        serde_json::to_vec(&confirmation).map_err(|_| "invalid retry confirmation")?;
    let reply = send(query).await?;
    let next = decode_mixed_snapshot(&reply.document_json)
        .map_err(|_| "new retry reply invalid; inspect the run list, do not replay")?;
    if next.id().as_uuid().as_bytes().as_slice() != reply.run_id
        || next
            .check_retry_confirmation(&parent, &confirmation)
            .is_err()
    {
        return Err("new retry provenance or frozen plan mismatch; inspect the run list");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&next)
            .map_err(|_| "new run serialization failed; inspect the run list")?
    );
    Ok(())
}
async fn send(query: TaskRequest) -> Result<TaskResponse, &'static str> {
    let retry = query.operation == TaskOperation::MixedRetry as i32;
    let expected = query.run_id.clone();
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "no authenticated daemon discovery record")?;
    tokio::time::timeout(Duration::from_secs(10),async{
        #[cfg(windows)]let mut stream=cshell_ipc::transport::connect(&discovery.endpoint.to_string_lossy()).await.map_err(|_|"cannot connect to daemon")?;
        #[cfg(unix)]let mut stream=cshell_ipc::transport::connect(std::path::Path::new(&discovery.endpoint)).await.map_err(|_|"cannot connect to daemon")?;
        let mut handshake=Handshake::new(discovery.daemon_instance_id.to_vec(),discovery.instance_token.to_vec());let required=features::TASK_CONTROL|features::MIXED_TASK_CONTROL|features::MIXED_TASK_RETRY;handshake.feature_bits=required;
        let negotiated=client_handshake(&mut stream,1,handshake).await.map_err(|_|"daemon authentication failed")?;if negotiated.feature_bits&required!=required{return Err("daemon does not support mixed new-run retry");}
        write_envelope(&mut stream,&Envelope{request_id:2,payload:Some(envelope::Payload::TaskRequest(query)),..Default::default()}).await.map_err(|_|"retry send interrupted; inspect saved runs before another mutation")?;
        let reply=read_envelope(&mut stream).await.map_err(|_|"retry reply unavailable; inspect saved runs before another mutation")?;
        if reply.request_id!=2{return Err("retry reply identity mismatch");}let Some(envelope::Payload::TaskResponse(reply))=reply.payload else{return Err("retry reply type mismatch");};
        if reply.status!=TaskStatus::Ok as i32{return Err(match TaskStatus::try_from(reply.status){Ok(TaskStatus::Conflict)=>"source versions changed, parent active, or whole-plan replay conditions do not hold",Ok(TaskStatus::Missing)=>"source run missing",Ok(TaskStatus::Limit)=>"retry exceeds limit",_=>"mixed retry request rejected"});}
        if reply.revision!=0||!reply.output.is_empty()||reply.output_length!=0||reply.document_json.len()>MAX_TASK_SNAPSHOT_BYTES
            ||!retry&&reply.run_id!=expected||retry&&(reply.run_id==expected||cshell_ipc::decode_id(&reply.run_id).is_err()){return Err("unexpected retry reply fields");}Ok(reply)
    }).await.map_err(|_|"retry request timed out; inspect saved runs, no automatic retry")?
}
