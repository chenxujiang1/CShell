use cshell_application::*;
use cshell_domain::TaskRunId;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TaskOperation, TaskRequest, TaskResponse,
    TaskStatus, client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::time::Duration;
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args
        .first()
        .is_some_and(|command| command == "mixed-rollout-approve")
    {
        if args.len() != 3 || args[2] != "--confirm" {
            return Err(crate::USAGE);
        }
        let document = decode_mixed_rollout(&crate::document(&args[1]).await?)
            .map_err(|_| "invalid rollout control document")?;
        let gate = document
            .progress
            .gate
            .as_ref()
            .ok_or("no gate to approve")?;
        if document.active {
            return Err("wait for the parent worker to release ownership");
        }
        let mut query = request(TaskOperation::MixedRolloutApprove, document.run_id);
        query.expected_revision = document.progress.revision;
        query.gate_id = gate.id.as_uuid().as_bytes().to_vec();
        query.confirmed = true;
        let reply = send(query).await?;
        let approved = decode_mixed_rollout(&reply.document_json)
            .map_err(|_| "approval reply invalid; reload control before another mutation")?;
        if approved.run_id != document.run_id
            || approved.snapshot_sha256 != document.snapshot_sha256
            || approved.policy != document.policy
            || approved.progress.revision != document.progress.revision + 1
            || approved.progress.gate.is_some()
            || !approved.progress.approved.contains(&(gate.next_batch - 1))
        {
            return Err("approval reply identity/state mismatch; reload control");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&approved).map_err(|_| "invalid control reply")?
        );
        return Ok(());
    }
    if args.len() != 2 {
        return Err(crate::USAGE);
    }
    let id = TaskRunId::from_bytes(
        cshell_ipc::decode_id(&crate::id(&args[1])?).map_err(|_| "invalid run ID")?,
    );
    let reply = send(request(TaskOperation::MixedRolloutGet, id)).await?;
    let control =
        decode_mixed_rollout(&reply.document_json).map_err(|_| "invalid rollout state")?;
    if control.run_id != id {
        return Err("rollout identity mismatch");
    }
    match args[0].as_str() {
        "mixed-rollout-get" => println!(
            "{}",
            serde_json::to_string_pretty(&control).map_err(|_| "invalid rollout state")?
        ),
        "mixed-rollout-confirmation" => {
            let mut query = request(TaskOperation::MixedGet, id);
            query.limit = 64;
            let reply = send(query).await?;
            let page =
                decode_mixed_status(&reply.document_json).map_err(|_| "invalid mixed status")?;
            if page.run_id != id || page.after.is_some() || page.limit != 64 {
                return Err("mixed status identity/cursor mismatch");
            }
            let confirmation = page.confirmation_bound(Some(&control)).map_err(
                |_| "control or target versions changed, gate pending or parent active; reload",
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&confirmation)
                    .map_err(|_| "invalid mixed confirmation")?
            );
        }
        _ => return Err(crate::USAGE),
    }
    Ok(())
}
fn request(operation: TaskOperation, id: TaskRunId) -> TaskRequest {
    let mut query = TaskRequest::default();
    query.operation = operation as i32;
    query.run_id = id.as_uuid().as_bytes().to_vec();
    query
}
async fn send(query: TaskRequest) -> Result<TaskResponse, &'static str> {
    let expected = query.run_id.clone();
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "no authenticated daemon discovery record")?;
    tokio::time::timeout(Duration::from_secs(10), async {
        #[cfg(windows)]
        let mut stream = cshell_ipc::transport::connect(&discovery.endpoint.to_string_lossy())
            .await
            .map_err(|_| "cannot connect to daemon")?;
        #[cfg(unix)]
        let mut stream = cshell_ipc::transport::connect(std::path::Path::new(&discovery.endpoint))
            .await
            .map_err(|_| "cannot connect to daemon")?;
        let mut handshake = Handshake::new(
            discovery.daemon_instance_id.to_vec(),
            discovery.instance_token.to_vec(),
        );
        let required =
            features::TASK_CONTROL | features::MIXED_TASK_CONTROL | features::MIXED_ROLLOUT;
        handshake.feature_bits = required;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & required != required {
            return Err("daemon does not support durable mixed rollout controls");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TaskRequest(query)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "control request send interrupted; no automatic retry")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "control reply unavailable; inspect control before another mutation")?;
        if reply.request_id != 2 {
            return Err("control reply identity mismatch");
        }
        let Some(envelope::Payload::TaskResponse(reply)) = reply.payload else {
            return Err("control reply type mismatch");
        };
        if reply.status != TaskStatus::Ok as i32 {
            return Err(match TaskStatus::try_from(reply.status) {
                Ok(TaskStatus::Conflict) => {
                    "gate/control versions changed, expired or parent active"
                }
                Ok(TaskStatus::Missing) => "run missing",
                _ => "rollout control rejected",
            });
        }
        if reply.run_id != expected
            || reply.revision != 0
            || !reply.output.is_empty()
            || reply.output_length != 0
        {
            return Err("unexpected control reply fields");
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "control request timed out; no automatic retry")?
}
