//! Capture and export one stopped mixed run through immutable result pages.
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TaskOperation, TaskRequest, TaskResponse,
    TaskStatus, client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::{path::PathBuf, time::Duration};
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    let command = args.first().map(String::as_str).ok_or(crate::USAGE)?;
    let manifest_file = command == "mixed-result-page";
    let (run_id, loaded, export) = match command {
        "mixed-results" if args.len() == 2 => (
            TaskRunId::from_bytes(
                cshell_ipc::decode_id(&crate::id(&args[1])?).map_err(|_| "invalid run ID")?,
            ),
            None,
            None,
        ),
        "mixed-result-page" if args.len() == 2 || args.len() == 3 => {
            let manifest = decode_mixed_result_manifest(&crate::document(&args[1]).await?)
                .map_err(|_| "invalid mixed result manifest")?;
            (manifest.header.run_id, Some(manifest), None)
        }
        "mixed-export" if args.len() == 4 => {
            let format = match args[2].as_str() {
                "json" => TaskResultExportFormat::Json,
                "csv" => TaskResultExportFormat::Csv,
                _ => return Err("select json or csv"),
            };
            let path = PathBuf::from(&args[3]);
            if !path.is_absolute() {
                return Err("export needs a new absolute file path");
            }
            (
                TaskRunId::from_bytes(
                    cshell_ipc::decode_id(&crate::id(&args[1])?).map_err(|_| "invalid run ID")?,
                ),
                None,
                Some((path, format)),
            )
        }
        _ => return Err(crate::USAGE),
    };
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "no authenticated daemon discovery record")?;
    let manifest = if let Some(manifest) = loaded {
        manifest
    } else {
        let reply = send(&discovery, query(TaskOperation::MixedResults, run_id)).await?;
        let manifest = decode_mixed_result_manifest(&reply.document_json)
            .map_err(|_| "invalid mixed result manifest")?;
        if manifest.header.run_id != run_id {
            return Err("mixed capture identity mismatch");
        }
        manifest
    };
    if manifest_file {
        let after = if args.len() == 3 {
            Some(HostId::from_bytes(
                cshell_ipc::decode_id(&crate::id(&args[2])?).map_err(|_| "invalid host ID")?,
            ))
        } else {
            None
        };
        let reply = send(&discovery, page_query(&manifest, after)).await?;
        let page = decode_mixed_result_page(&reply.document_json, &manifest)
            .map_err(|_| "invalid mixed result page")?;
        if page.after != after || page.limit != MAX_MIXED_STATUS_TARGETS {
            return Err("mixed result cursor mismatch");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&page).map_err(|_| "invalid mixed result page")?
        );
        return Ok(());
    }
    let Some((path, format)) = export else {
        println!(
            "{}",
            serde_json::to_string_pretty(&manifest).map_err(|_| "invalid mixed result manifest")?
        );
        return Ok(());
    };
    let mut collector =
        MixedResultCollector::new(manifest.clone()).map_err(|_| "invalid mixed result manifest")?;
    loop {
        let reply = send(&discovery, page_query(&manifest, collector.after())).await?;
        if collector
            .push(&reply.document_json)
            .map_err(|_| "mixed result page mismatch or capture exceeds limit")?
        {
            break;
        }
    }
    let document = collector
        .finish()
        .map_err(|_| "mixed result digest or summary mismatch")?;
    tokio::task::spawn_blocking(move || {
        cshell_ipc::publish_export_file(&path, |file| {
            write_mixed_result_export(&document, format, file)
        })
    })
    .await
    .map_err(|_| "export worker failed; inspect the destination")?
    .map_err(|_| "export failed; inspect the destination before trying again")?;
    println!("mixed result capture exported to a new file");
    Ok(())
}
fn query(operation: TaskOperation, run: TaskRunId) -> TaskRequest {
    let mut query = TaskRequest::default();
    query.operation = operation as i32;
    query.run_id = run.as_uuid().as_bytes().to_vec();
    query
}
fn page_query(manifest: &MixedResultManifest, after: Option<HostId>) -> TaskRequest {
    let mut request = query(TaskOperation::MixedResultPage, manifest.header.run_id);
    request.result_id = manifest.result_id.as_uuid().as_bytes().to_vec();
    request.limit = MAX_MIXED_STATUS_TARGETS as u32;
    if let Some(after) = after {
        request.host_id = after.as_uuid().as_bytes().to_vec();
    }
    request
}
async fn send(
    discovery: &DiscoveryRecord,
    query: TaskRequest,
) -> Result<TaskResponse, &'static str> {
    let expected = query.run_id.clone();
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
            features::TASK_CONTROL | features::MIXED_TASK_CONTROL | features::MIXED_RESULTS;
        handshake.feature_bits = required;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & required != required {
            return Err("daemon does not support mixed result captures");
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
        .map_err(|_| "mixed result request send failed")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "mixed result reply unavailable")?;
        if reply.request_id != 2 {
            return Err("mixed result reply identity mismatch");
        }
        let Some(envelope::Payload::TaskResponse(reply)) = reply.payload else {
            return Err("mixed result reply type mismatch");
        };
        if reply.status != TaskStatus::Ok as i32 {
            return Err(match TaskStatus::try_from(reply.status) {
                Ok(TaskStatus::Missing) => {
                    "capture expired or was evicted, or run missing; explicitly capture again"
                }
                Ok(TaskStatus::Conflict) => {
                    "all targets must be stopped and no mixed worker may be active"
                }
                Ok(TaskStatus::Limit) => "mixed result capture exceeds limit",
                _ => "mixed result capture request rejected",
            });
        }
        if reply.run_id != expected
            || reply.revision != 0
            || !reply.output.is_empty()
            || reply.output_length != 0
        {
            return Err("unexpected mixed result response fields");
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "mixed result request timed out; no automatic retry")?
}
