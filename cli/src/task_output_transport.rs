//! Authenticated observation jobs, sent once with bounded replies and deadlines.
use cshell_application::{MAX_OUTPUT_ANALYSIS_DOCUMENT, OutputAnalysisKind};
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TaskOperation, TaskRequest, TaskResponse,
    TaskStatus, client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::time::Duration;
pub(crate) async fn send(
    query: TaskRequest,
    kind: OutputAnalysisKind,
) -> Result<TaskResponse, &'static str> {
    let operation =
        TaskOperation::try_from(query.operation).map_err(|_| "invalid output operation")?;
    if !operation.is_output_analysis()
        && !operation.is_output_search()
        && !operation.is_output_merge()
        && !operation.is_output_archive()
        && !operation.is_output_workspace()
    {
        return Err("invalid output observation operation");
    }
    let expected = query.run_id.clone();
    let required = features::TASK_CONTROL
        | if operation.is_output_workspace() {
            features::OUTPUT_WORKSPACE
        } else if operation.is_output_archive() {
            features::TASK_OUTPUT_ARCHIVE
        } else if operation.is_output_merge() {
            features::TASK_OUTPUT_MERGE
        } else if operation.is_output_search() {
            features::TASK_OUTPUT_SEARCH
        } else {
            features::TASK_OUTPUT_ANALYSIS
        }
        | if kind == OutputAnalysisKind::Mixed {
            features::MIXED_TASK_CONTROL
        } else {
            0
        };
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
        handshake.feature_bits = required;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & required != required {
            return Err("daemon does not support the requested output observation");
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
        .map_err(|_| "output request send failed; no automatic retry")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "output reply unavailable; no automatic retry")?;
        if response.request_id != 2 {
            return Err("output reply identity mismatch");
        }
        let Some(envelope::Payload::TaskResponse(reply)) = response.payload else {
            return Err("output reply type mismatch");
        };
        if reply.status != TaskStatus::Ok as i32 {
            return Err(match TaskStatus::try_from(reply.status) {
                Ok(TaskStatus::Busy) => "output observation workers are busy",
                Ok(TaskStatus::Conflict) => "scope revision changed or command not stopped",
                Ok(TaskStatus::Missing) if operation.is_output_merge() => {
                    "chronology journal unavailable (legacy output or missing files)"
                }
                Ok(TaskStatus::Missing) => "output job missing, expired or daemon restarted",
                _ => "output observation request rejected",
            });
        }
        if reply.run_id != expected
            || reply.revision != 0
            || if operation == TaskOperation::OutputArchiveRead
                || operation == TaskOperation::OutputWorkspaceRead
            {
                reply.output.len() > cshell_application::MAX_OUTPUT_ARCHIVE_CHUNK
                    || reply.output_length != reply.output.len() as u64
            } else if operation.is_output_merge() {
                reply.output.len() > cshell_application::MAX_OUTPUT_MERGE_BYTES
                    || reply.output_length != reply.output.len() as u64
            } else {
                !reply.output.is_empty() || reply.output_length != 0
            }
            || reply.document_json.len()
                > if operation.is_output_workspace() {
                    cshell_application::MAX_WORKSPACE_DOCUMENT
                } else {
                    MAX_OUTPUT_ANALYSIS_DOCUMENT
                }
        {
            return Err("unexpected output reply fields");
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "output request timed out; no automatic retry")?
}
