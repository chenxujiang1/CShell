use cshell_application::*;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, WorkspaceOperation as Op, WorkspaceRequest,
    WorkspaceStatus, client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::{
    io::{Read, Write},
    time::Duration,
};
fn read(path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "cannot open workspace input")?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect workspace input")?
        .is_file()
    {
        return Err("workspace input must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take((limit + 4) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read workspace input")?;
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes.drain(..3);
    }
    if bytes.len() > limit {
        return Err("workspace input limit");
    }
    Ok(bytes)
}
fn publish(path: &str, bytes: &[u8]) -> Result<(), &'static str> {
    let path = std::path::Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| "cannot stage workspace output")?;
    file.write_all(bytes)
        .map_err(|_| "workspace write failed")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "workspace sync failed")?;
    file.persist_noclobber(path)
        .map_err(|_| "output already exists or publication failed")?;
    Ok(())
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    match args {
        [op] if op == "get" => {
            let snapshot = query(WorkspaceRequest::default()).await?;
            let value = serde_json::json!({"revision":snapshot.as_ref().map_or(0,|s|s.revision),"document":snapshot.map(|s|s.document)});
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|_| "workspace encoding failed")?
            );
        }
        [op, path] if op == "export" => {
            let snapshot = query(WorkspaceRequest::default())
                .await?
                .ok_or("no saved workspace")?;
            publish(
                path,
                &encode_workspace(&snapshot.document).map_err(|_| "invalid stored workspace")?,
            )?;
        }
        [op, path, review] if op == "preview" => {
            let target = decode_workspace(&read(path, MAX_WORKSPACE_BYTES)?)
                .map_err(|_| "invalid target workspace")?;
            let current = query(WorkspaceRequest::default()).await?;
            let revision = current.as_ref().map_or(0, |s| s.revision);
            let plan = WorkspaceRestorePlan::new(
                revision,
                current.map_or_else(Default::default, |s| s.document),
                target,
            )
            .map_err(|_| "workspace recovery topology invalid or unsupported")?;
            plan.fingerprint().map_err(|_| "invalid workspace review")?;
            publish(
                review,
                &serde_json::to_vec_pretty(&plan)
                    .map_err(|_| "workspace review encoding failed")?,
            )?;
        }
        [op, path, confirm] if op == "restore" && confirm == "--confirm" => {
            let plan = decode_workspace_restore(&read(path, MAX_WORKSPACE_REVIEW_BYTES)?)
                .map_err(|_| "invalid workspace review")?;
            let current = query(WorkspaceRequest::default()).await?;
            if current.as_ref().map_or(0, |s| s.revision) != plan.storage_revision
                || current.as_ref().is_some_and(|s| s.document != plan.before)
            {
                return Err("workspace changed; create a new preview");
            }
            let saved = query(WorkspaceRequest {
                operation: Op::Save as i32,
                expected_revision: plan.storage_revision,
                document_json: encode_workspace(&plan.target)
                    .map_err(|_| "invalid target workspace")?,
            })
            .await?
            .ok_or("workspace save receipt missing; outcome unknown")?;
            if plan.storage_revision.checked_add(1) != Some(saved.revision)
                || saved.document != plan.target
            {
                return Err("workspace save receipt mismatch; outcome unknown, do not replay");
            }
            println!(
                "{}",
                serde_json::json!({"revision":saved.revision,"document":saved.document})
            );
        }
        _ => return Err(crate::USAGE),
    }
    Ok(())
}
async fn query(request: WorkspaceRequest) -> Result<Option<WorkspaceSnapshot>, &'static str> {
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "authenticated daemon discovery missing")?;
    tokio::time::timeout(Duration::from_secs(40), async {
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
        hello.feature_bits = features::WORKSPACE_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & features::WORKSPACE_CONTROL == 0 {
            return Err("workspace capability unavailable");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::WorkspaceRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "workspace request outcome unknown; read before any new preview")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "workspace outcome unknown; do not replay restore")?;
        if response.request_id != 2 {
            return Err("workspace reply identity mismatch");
        }
        let Some(envelope::Payload::WorkspaceResponse(response)) = response.payload else {
            return Err("workspace reply type mismatch");
        };
        if response.detail.len() > 4096 || response.document_json.len() > MAX_WORKSPACE_BYTES {
            return Err("workspace reply limit");
        }
        if response.status != WorkspaceStatus::Ok as i32 {
            eprintln!("{}", response.detail);
            return Err("workspace request refused or uncertain; read and preview again");
        }
        if response.document_json.is_empty() {
            return if response.revision == 0 {
                Ok(None)
            } else {
                Err("workspace receipt missing document")
            };
        }
        if response.revision == 0 {
            return Err("workspace document has no revision");
        }
        Ok(Some(WorkspaceSnapshot {
            revision: response.revision,
            document: decode_workspace(&response.document_json)
                .map_err(|_| "invalid workspace reply")?,
        }))
    })
    .await
    .map_err(|_| "workspace request timed out; outcome unknown, do not replay restore")?
}
