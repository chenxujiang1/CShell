use cshell_application::{SftpQueueDirection, SftpQueueDraft, decode_sftp_queue};
use cshell_domain::SessionId;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, SftpOperation, SftpRequest, SftpResponse,
    SftpStatus, client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::time::Duration;
mod workspace;
fn session(value: &str) -> Result<SessionId, &'static str> {
    let id = uuid::Uuid::parse_str(value).map_err(|_| "invalid session UUID")?;
    if id.is_nil() {
        return Err("nil UUID is not allowed");
    }
    Ok(SessionId::from_uuid(id))
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args.first().is_some_and(|s| workspace::handles(s)) {
        return workspace::run(args).await;
    }
    if args.len() < 2 {
        return Err(crate::USAGE);
    }
    let id = session(&args[1])?;
    let mut request = SftpRequest {
        session_id: id.as_uuid().as_bytes().to_vec(),
        ..Default::default()
    };
    match args[0].as_str() {
        "local-browse" if args.len() == 2 || args.len() == 3 => {
            request.operation = SftpOperation::LocalBrowse as i32;
            request.local_path = args.get(2).cloned().unwrap_or_default();
            request.max_entries = 512;
        }
        "remote-browse" if args.len() == 3 => {
            request.operation = SftpOperation::RemoteBrowse as i32;
            request.remote_path = args[2].clone();
            request.max_entries = 512;
        }
        "browse-next" | "browse-close" if args.len() == 6 => {
            request.operation = if args[0] == "browse-next" {
                SftpOperation::BrowseNext
            } else {
                SftpOperation::BrowseClose
            } as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
            request.expected_revision = args[3].parse().map_err(|_| "invalid cursor revision")?;
            if request.expected_revision == 0 || !matches!(args[4].as_str(), "local" | "remote") {
                return Err(crate::USAGE);
            }
            request.max_entries = if args[0] == "browse-next" { 512 } else { 0 };
        }
        "queue-get" if args.len() == 2 => request.operation = SftpOperation::QueueGet as i32,
        "queue-add" | "queue-add-verified" if args.len() == 7 && args[6] == "--confirm" => {
            request.operation = if args[0] == "queue-add-verified" {
                SftpOperation::QueueAddVerified
            } else {
                SftpOperation::QueueAdd
            } as i32;
            let direction = match args[3].as_str() {
                "upload" => SftpQueueDirection::Upload,
                "download" => SftpQueueDirection::Download,
                _ => return Err(crate::USAGE),
            };
            let draft = SftpQueueDraft {
                verified: args[0] == "queue-add-verified",
                direction,
                local_path: args[4].clone(),
                remote_path: args[5].clone(),
                ..Default::default()
            };
            draft.validate()?;
            request.document_json =
                serde_json::to_vec(&draft).map_err(|_| "queue draft encoding failed")?;
        }
        "queue-start" | "queue-start-verified" | "queue-pause"
            if args.len() == 4 && args[3] == "--confirm" =>
        {
            request.operation = match args[0].as_str() {
                "queue-start" => SftpOperation::QueueStart,
                "queue-start-verified" => SftpOperation::QueueStartVerified,
                _ => SftpOperation::QueuePause,
            } as i32;
        }
        "queue-cancel" | "queue-remove" | "queue-resume" | "queue-adopt" | "queue-cleanup"
            if args.len() == 5 && args[4] == "--confirm" =>
        {
            request.operation = match args[0].as_str() {
                "queue-cancel" => SftpOperation::QueueCancelPending,
                "queue-remove" => SftpOperation::QueueRemove,
                "queue-resume" => SftpOperation::QueueResumeVerified,
                "queue-adopt" => SftpOperation::QueueAdoptVerified,
                _ => SftpOperation::QueueCleanupVerified,
            } as i32;
            if args[3] != "all" || args[0] != "queue-cancel" {
                request.transfer_id = session(&args[3])?.as_uuid().as_bytes().to_vec();
            }
        }
        "local-list" if args.len() == 2 || args.len() == 3 => {
            request.operation = SftpOperation::LocalList as i32;
            request.local_path = args.get(2).cloned().unwrap_or_default();
            request.max_entries = 512;
        }
        "remote-list" if args.len() == 3 => {
            request.operation = SftpOperation::List as i32;
            request.remote_path = args[2].clone();
            request.max_entries = 512;
        }
        "transfer-get" if args.len() == 3 => {
            request.operation = SftpOperation::TransferStatus as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
        }
        "transfer-cancel" if args.len() == 4 && args[3] == "--confirm" => {
            request.operation = SftpOperation::CancelTransfer as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
        }
        _ => return Err(crate::USAGE),
    }
    let mutation = matches!(
        SftpOperation::try_from(request.operation),
        Ok(SftpOperation::QueueAdd
            | SftpOperation::QueueStart
            | SftpOperation::QueuePause
            | SftpOperation::QueueCancelPending
            | SftpOperation::QueueRemove)
            | Ok(SftpOperation::QueueAddVerified
                | SftpOperation::QueueStartVerified
                | SftpOperation::QueueResumeVerified
                | SftpOperation::QueueAdoptVerified
                | SftpOperation::QueueCleanupVerified)
    );
    if mutation {
        request.expected_revision = args[2].parse().map_err(|_| "invalid queue revision")?;
        request.confirmed = true;
        if request.expected_revision == 0 {
            return Err("queue revision must be positive");
        }
    }
    let extended = request.operation >= SftpOperation::LocalList as i32;
    let expected_revision = request.expected_revision;
    let browse = (SftpOperation::RemoteBrowse as i32..=SftpOperation::BrowseClose as i32)
        .contains(&request.operation);
    let cursor = request.transfer_id.clone();
    let browse_local = args[0] == "local-browse" || args.get(4).is_some_and(|s| s == "local");
    let browse_directory = if args[0].starts_with("browse-") {
        args.get(5)
    } else {
        args.get(2)
    }
    .cloned()
    .filter(|p| !p.is_empty());
    let reply = send(request, extended).await?;
    if reply.status != SftpStatus::Ok as i32 {
        return Err("SFTP request rejected; inspect saved state before another mutation");
    }
    if browse {
        if reply.transfer.is_some()
            || !reply.document_json.is_empty()
            || reply.browse_id.len() != 16
            || reply.browse_id.iter().all(|b| *b == 0)
            || reply.browse_revision
                != expected_revision
                    .checked_add(1)
                    .ok_or("cursor revision overflow")?
            || !cursor.is_empty() && cursor != reply.browse_id
            || reply.browse_local != browse_local
            || browse_directory
                .as_ref()
                .is_some_and(|d| *d != reply.browse_directory)
            || reply.browse_directory.len() > 4096
            || reply.browse_directory.chars().any(char::is_control)
            || browse_local && !std::path::Path::new(&reply.browse_directory).is_absolute()
            || !browse_local && !reply.browse_directory.starts_with('/')
            || args[0] == "browse-close" && (!reply.entries.is_empty() || reply.truncated)
            || !reply.entries.iter().all(|e| {
                !e.name.is_empty()
                    && e.name != "."
                    && e.name != ".."
                    && !e.name.contains(['/', '\\'])
                    && !e.name.chars().any(char::is_control)
                    && e.name.len() <= 4096
                    && e.path.len() <= 4096
                    && !e.path.chars().any(char::is_control)
                    && (1..=4).contains(&e.kind)
                    && if browse_local {
                        std::path::Path::new(&e.path).parent()
                            == Some(std::path::Path::new(&reply.browse_directory))
                    } else {
                        e.path
                            == format!(
                                "{}/{}",
                                reply.browse_directory.trim_end_matches('/'),
                                e.name
                            )
                    }
            })
        {
            return Err(
                "directory cursor reply mismatch; reload instead of replaying continuation",
            );
        }
        let cursor_id =
            uuid::Uuid::from_slice(&reply.browse_id).map_err(|_| "invalid cursor identity")?;
        let entries: Vec<_> = reply.entries.iter().map(|e| serde_json::json!({"name":e.name,"path":e.path,"kind":e.kind,"size":e.size,"modified":e.modified_unix_seconds})).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"session":id,"cursor":cursor_id,"revision":reply.browse_revision,"directory":reply.browse_directory,"local":reply.browse_local,"more":reply.truncated,"entries":entries})).map_err(|_| "directory encoding failed")?);
    } else if args[0].starts_with("queue-") {
        let queue = decode_sftp_queue(&reply.document_json)?;
        if queue.session_id != id
            || mutation && expected_revision.checked_add(1) != Some(queue.revision)
        {
            return Err("queue reply session/revision mismatch");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&queue).map_err(|_| "queue encoding failed")?
        );
    } else if args[0] == "local-list" || args[0] == "remote-list" {
        if reply.entries.len() > 512 || reply.transfer.is_some() {
            return Err("invalid directory reply");
        }
        let directory = if args[0] == "local-list" {
            Some(
                serde_json::from_slice::<String>(&reply.document_json)
                    .map_err(|_| "invalid local directory")?,
            )
        } else {
            args.get(2).cloned()
        };
        let entries: Vec<_> = reply.entries.iter().map(|entry| serde_json::json!({"name":entry.name, "path":entry.path, "kind":entry.kind, "size":entry.size, "modified":entry.modified_unix_seconds})).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"directory":directory,"entries":entries,"truncated":reply.truncated})).map_err(|_| "directory encoding failed")?);
    } else {
        let transfer = reply.transfer.ok_or("missing transfer reply")?;
        let transfer_id = session(&args[2])?;
        if transfer.transfer_id.as_slice() != transfer_id.as_uuid().as_bytes().as_slice() {
            return Err("transfer identity mismatch");
        }
        println!(
            "{}",
            serde_json::json!({"id":transfer_id,"state":transfer.state,"bytes":transfer.bytes_transferred,"total":transfer.total_bytes,
            "committed":transfer.destination_committed,"cleanup_path":transfer.cleanup_path,"cleanup_location":transfer.cleanup_location})
        );
    }
    Ok(())
}
async fn send(request: SftpRequest, extended: bool) -> Result<SftpResponse, &'static str> {
    let browsing = (SftpOperation::RemoteBrowse as i32..=SftpOperation::BrowseClose as i32)
        .contains(&request.operation)
        || request.operation == SftpOperation::BrowseManaged as i32;
    let workspace = request.operation >= SftpOperation::QueueEdit as i32;
    let verified = request.operation >= SftpOperation::QueueAddVerified as i32;
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
        let mut handshake = Handshake::new(
            discovery.daemon_instance_id.to_vec(),
            discovery.instance_token.to_vec(),
        );
        let required = features::SFTP_CONTROL
            | if workspace {
                features::SFTP_WORKSPACE
            } else {
                0
            }
            | if verified {
                features::SFTP_VERIFIED_QUEUE
            } else {
                0
            }
            | if browsing {
                features::SFTP_DIRECTORY_CURSOR
            } else {
                0
            }
            | if extended {
                features::SFTP_BROWSER_QUEUE
            } else {
                0
            };
        handshake.feature_bits = required;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & required != required {
            return Err("daemon lacks requested SFTP feature");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::SftpRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "SFTP request send uncertain; do not replay")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "SFTP reply uncertain; do not replay")?;
        if reply.request_id != 2 {
            return Err("SFTP reply identity mismatch");
        }
        let Some(envelope::Payload::SftpResponse(reply)) = reply.payload else {
            return Err("invalid SFTP reply");
        };
        if reply.document_json.len()
            > if workspace {
                cshell_application::MAX_SFTP_WORKSPACE_DOCUMENT
            } else {
                cshell_application::MAX_SFTP_QUEUE_DOCUMENT
            }
            || reply.entries.len() > 512
        {
            return Err("SFTP reply limit");
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "SFTP request timed out; inspect queue/transfer before another mutation")?
}
