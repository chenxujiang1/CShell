use super::*;
use cshell_application::*;
use std::io::Read;
pub(super) fn handles(command: &str) -> bool {
    matches!(
        command,
        "peer-get"
            | "browse"
            | "queue-add-managed"
            | "queue-add-many"
            | "queue-edit"
            | "queue-configure"
            | "queue-start-managed"
            | "queue-start-one"
            | "queue-migrate"
            | "file-inspect"
            | "file-apply"
            | "file-get"
            | "file-cancel"
            | "edit-open"
            | "edit-get"
            | "edit-launch"
            | "edit-save"
            | "edit-close"
            | "edit-rebind"
            | "workspace-list"
            | "workspace-forget"
    )
}
fn revision(value: &str) -> Result<u64, &'static str> {
    let n = value.parse::<u64>().map_err(|_| "invalid revision")?;
    if n == 0 || n > i64::MAX as u64 {
        return Err("revision must be positive and bounded");
    }
    Ok(n)
}
fn bytes(path: &str, maximum: usize) -> Result<Vec<u8>, &'static str> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "input metadata unavailable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum as u64 {
        return Err("input must be a bounded regular file");
    }
    let mut file = std::fs::File::open(path).map_err(|_| "input file unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "input metadata unavailable")?
        .is_file()
    {
        return Err("input must be a regular file");
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "input read failed")?;
    if bytes.len() > maximum {
        return Err("input file exceeds limit");
    }
    Ok(bytes)
}
fn json<T: serde::de::DeserializeOwned>(path: &str, maximum: usize) -> Result<T, &'static str> {
    let bytes = bytes(path, maximum)?;
    serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
        .map_err(|_| "invalid JSON input")
}
pub(super) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args.len() < 2 {
        return Err(crate::USAGE);
    }
    let scope = session(&args[1])?;
    let mut request = SftpRequest {
        session_id: scope.as_uuid().as_bytes().to_vec(),
        ..Default::default()
    };
    let mut plain_queue = false;
    let mut browse = false;
    match args[0].as_str() {
        "peer-get" if args.len() == 2 => request.operation = SftpOperation::PeerGet as i32,
        "browse" if args.len() == 5 => {
            let local = match args[2].as_str() {
                "local" => true,
                "remote" => false,
                _ => return Err(crate::USAGE),
            };
            let links = match args[4].as_str() {
                "nofollow" => SftpLinkPolicy::NoFollow,
                "follow" => SftpLinkPolicy::Follow,
                _ => return Err(crate::USAGE),
            };
            request.operation = SftpOperation::BrowseManaged as i32;
            request.max_entries = 512;
            request.document_json = serde_json::to_vec(&SftpBrowseOptions {
                local,
                path: args[3].clone(),
                links,
            })
            .map_err(|_| "browse encoding failed")?;
            browse = true;
        }
        "queue-add-managed" if args.len() == 5 && args[4] == "--confirm" => {
            let mut draft: SftpQueueDraft = json(&args[3], 16 * 1024)?;
            draft.verified = true;
            draft.validate()?;
            request.operation = SftpOperation::QueueAddManaged as i32;
            request.expected_revision = revision(&args[2])?;
            request.document_json =
                serde_json::to_vec(&draft).map_err(|_| "draft encoding failed")?;
            plain_queue = true;
        }
        "queue-add-many" if args.len() == 5 && args[4] == "--confirm" => {
            let mut drafts: Vec<SftpQueueDraft> = json(&args[3], 1024 * 1024)?;
            if drafts.is_empty() || drafts.len() > MAX_SFTP_QUEUE_ITEMS {
                return Err("bulk item limit");
            }
            for draft in &mut drafts {
                draft.verified = true;
                draft.validate()?;
            }
            request.operation = SftpOperation::QueueAddMany as i32;
            request.expected_revision = revision(&args[2])?;
            request.document_json =
                serde_json::to_vec(&drafts).map_err(|_| "bulk encoding failed")?;
        }
        "queue-edit" if args.len() == 6 && args[5] == "--confirm" => {
            let mut draft: SftpQueueDraft = json(&args[4], 16 * 1024)?;
            draft.verified = true;
            draft.validate()?;
            request.operation = SftpOperation::QueueEdit as i32;
            request.expected_revision = revision(&args[2])?;
            request.transfer_id = session(&args[3])?.as_uuid().as_bytes().to_vec();
            request.document_json =
                serde_json::to_vec(&draft).map_err(|_| "draft encoding failed")?;
            plain_queue = true;
        }
        "queue-configure" if args.len() == 5 && args[4] == "--confirm" => {
            let concurrency = args[3].parse::<u32>().map_err(|_| "invalid concurrency")?;
            if !(1..=4).contains(&concurrency) {
                return Err("concurrency must be 1..4");
            }
            request.operation = SftpOperation::QueueConfigure as i32;
            request.expected_revision = revision(&args[2])?;
            request.document_json = serde_json::to_vec(&SftpQueueSettings { concurrency })
                .map_err(|_| "settings encoding failed")?;
            plain_queue = true;
        }
        "queue-start-managed" if args.len() == 4 && args[3] == "--confirm" => {
            request.operation = SftpOperation::QueueStartManaged as i32;
            request.expected_revision = revision(&args[2])?;
            plain_queue = true;
        }
        "queue-start-one" if args.len() == 5 && args[4] == "--confirm" => {
            request.operation = SftpOperation::QueueStartOne as i32;
            request.expected_revision = revision(&args[2])?;
            request.transfer_id = session(&args[3])?.as_uuid().as_bytes().to_vec();
            plain_queue = true;
        }
        "queue-migrate" if args.len() == 7 && args[6] == "--confirm" => {
            let target = session(&args[3])?;
            let input = bytes(&args[5], 16 * 1024)?;
            let input = input.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&input);
            let peer =
                if let Ok(peer) = serde_json::from_slice::<cshell_domain::ClipboardTrust>(&input) {
                    peer
                } else {
                    match decode_sftp_workspace(&input)? {
                        SftpWorkspaceReply::Peer { session, peer } if session == target => peer,
                        _ => return Err("peer file must describe the target session"),
                    }
                };
            request.operation = SftpOperation::QueueMigrate as i32;
            request.expected_revision = revision(&args[2])?;
            request.document_json = serde_json::to_vec(&SftpQueueMigration {
                target,
                target_revision: revision(&args[4])?,
                peer,
            })
            .map_err(|_| "migration encoding failed")?;
        }
        "file-inspect" if args.len() == 5 || args.len() == 6 => {
            let local = match args[2].as_str() {
                "local" => true,
                "remote" => false,
                _ => return Err(crate::USAGE),
            };
            let action = match args[3].as_str() {
                "inspect" => SftpFileAction::Inspect,
                "mkdir" => SftpFileAction::Mkdir,
                "rename" => SftpFileAction::Rename,
                "delete" => SftpFileAction::Delete,
                "chmod" => SftpFileAction::Chmod,
                "symlink" => SftpFileAction::Symlink,
                _ => return Err(crate::USAGE),
            };
            let extra = args.get(5);
            let draft = SftpFileDraft {
                local,
                action,
                path: args[4].clone(),
                target: if matches!(action, SftpFileAction::Rename | SftpFileAction::Symlink) {
                    extra.ok_or(crate::USAGE)?.clone()
                } else {
                    String::new()
                },
                permissions: if matches!(action, SftpFileAction::Mkdir | SftpFileAction::Chmod) {
                    Some(
                        u32::from_str_radix(extra.ok_or(crate::USAGE)?, 8)
                            .map_err(|_| "invalid octal permissions")?,
                    )
                } else {
                    None
                },
            };
            if extra.is_some() && matches!(action, SftpFileAction::Inspect | SftpFileAction::Delete)
            {
                return Err(crate::USAGE);
            }
            draft.validate()?;
            request.operation = SftpOperation::FileInspect as i32;
            request.document_json =
                serde_json::to_vec(&draft).map_err(|_| "file draft encoding failed")?;
        }
        "file-get" | "edit-get" if args.len() == 3 => {
            request.operation = if args[0] == "file-get" {
                SftpOperation::FileJobGet
            } else {
                SftpOperation::EditGet
            } as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
        }
        "file-apply" | "file-cancel" | "edit-close" | "edit-rebind"
            if args.len() == 5 && args[4] == "--confirm" =>
        {
            request.operation = match args[0].as_str() {
                "file-apply" => SftpOperation::FileApply,
                "file-cancel" => SftpOperation::FileJobCancel,
                "edit-close" => SftpOperation::EditClose,
                _ => SftpOperation::EditRebind,
            } as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
            request.expected_revision = revision(&args[3])?;
        }
        "edit-open" if args.len() == 3 || args.len() == 4 => {
            let links = match args.get(3).map(String::as_str).unwrap_or("nofollow") {
                "nofollow" => SftpLinkPolicy::NoFollow,
                "follow" => SftpLinkPolicy::Follow,
                _ => return Err(crate::USAGE),
            };
            if !sftp_remote_path(&args[2], false) {
                return Err("invalid remote edit path");
            }
            request.operation = SftpOperation::EditOpen as i32;
            request.document_json = serde_json::to_vec(&SftpBrowseOptions {
                local: false,
                path: args[2].clone(),
                links,
            })
            .map_err(|_| "editor encoding failed")?;
        }
        "edit-launch" if args.len() == 7 && args[6] == "--confirm" => {
            let arguments: Vec<String> = json(&args[5], 16 * 1024)?;
            let launch = SftpEditorLaunch {
                executable: args[4].clone(),
                arguments,
            };
            if !std::path::Path::new(&launch.executable).is_absolute() {
                return Err("editor executable must be absolute");
            }
            request.operation = SftpOperation::EditLaunch as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
            request.expected_revision = revision(&args[3])?;
            request.document_json =
                serde_json::to_vec(&launch).map_err(|_| "editor launch encoding failed")?;
        }
        "edit-save" if args.len() == 8 && args[6] == "--exclusive" && args[7] == "--confirm" => {
            let content = if args[5] == "--working" {
                None
            } else {
                Some(String::from_utf8(bytes(&args[5],MAX_SFTP_EDIT_BYTES)?).map_err(|_|"inline edit text must be UTF-8; use the working file for binary content")?)
            };
            request.operation = SftpOperation::EditSave as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
            request.expected_revision = revision(&args[3])?;
            request.document_json = serde_json::to_vec(&SftpEditSave {
                content,
                queue_revision: revision(&args[4])?,
                accept_exclusive_replace: true,
            })
            .map_err(|_| "save encoding failed")?;
        }
        "workspace-list" if args.len() == 4 => {
            let all_sessions = match args[3].as_str() {
                "current" => false,
                "all" => true,
                _ => return Err(crate::USAGE),
            };
            request.operation = SftpOperation::WorkspaceList as i32;
            request.document_json = serde_json::to_vec(&SftpWorkspacePage {
                offset: args[2].parse().map_err(|_| "invalid history offset")?,
                all_sessions,
            })
            .map_err(|_| "history encoding failed")?;
        }
        "workspace-forget"
            if (args.len() == 6 || args.len() == 7)
                && args.last().map(String::as_str) == Some("--confirm") =>
        {
            let editor = match args[4].as_str() {
                "file" => false,
                "edit" => true,
                _ => return Err(crate::USAGE),
            };
            let discard_private_files = args.len() == 7 && args[5] == "--discard-private";
            if args.len() == 7 && !discard_private_files {
                return Err(crate::USAGE);
            }
            request.operation = SftpOperation::WorkspaceForget as i32;
            request.transfer_id = session(&args[2])?.as_uuid().as_bytes().to_vec();
            request.expected_revision = revision(&args[3])?;
            request.document_json = serde_json::to_vec(&SftpWorkspaceForget {
                editor,
                discard_private_files,
            })
            .map_err(|_| "forget encoding failed")?;
        }
        _ => return Err(crate::USAGE),
    }
    request.confirmed = request.expected_revision > 0;
    if request.document_json.len() > MAX_SFTP_WORKSPACE_DOCUMENT {
        return Err("workspace request document limit");
    }
    let expected = request.expected_revision;
    let object = request.transfer_id.clone();
    let operation = request.operation;
    let reply = send(request, true).await?;
    if reply.status != SftpStatus::Ok as i32 {
        return Err(
            "SFTP workspace request refused or uncertain; reload saved state before any further mutation",
        );
    }
    if browse {
        return print_browse(scope, &args[2], &args[3], &args[4], reply);
    }
    if !reply.entries.is_empty() || reply.transfer.is_some() || !reply.browse_id.is_empty() {
        return Err("invalid workspace response shape");
    }
    if plain_queue {
        let queue = decode_sftp_queue(&reply.document_json)?;
        if queue.session_id != scope || expected.checked_add(1) != Some(queue.revision) {
            return Err("queue response scope/revision mismatch");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&queue).map_err(|_| "queue encoding failed")?
        );
        return Ok(());
    }
    let value = decode_sftp_workspace(&reply.document_json)?;
    let same_id = |id: SessionId| {
        object.is_empty() || id.as_uuid().as_bytes().as_slice() == object.as_slice()
    };
    let valid = match &value {
        SftpWorkspaceReply::Review(review)=>operation==SftpOperation::EditReview as i32&&review.session==scope&&same_id(review.id),
        SftpWorkspaceReply::Peer { session, .. } => {
            operation == SftpOperation::PeerGet as i32 && *session == scope
        }
        SftpWorkspaceReply::History {
            session, offset, ..
        } => {
            operation == SftpOperation::WorkspaceList as i32
                && *session == scope
                && Some(*offset) == args.get(2).and_then(|s| s.parse().ok())
        }
        SftpWorkspaceReply::Forgotten { session, id, .. } => {
            operation == SftpOperation::WorkspaceForget as i32 && *session == scope && same_id(*id)
        }
        SftpWorkspaceReply::Queue(q) => {
            if operation == SftpOperation::QueueMigrate as i32 {
                q.session_id == session(&args[3])?
            } else {
                operation == SftpOperation::QueueAddMany as i32
                    && q.session_id == scope
                    && expected.checked_add(1) == Some(q.revision)
            }
        }
        SftpWorkspaceReply::File(j) => {
            matches!(
                SftpOperation::try_from(operation),
                Ok(SftpOperation::FileInspect
                    | SftpOperation::FileApply
                    | SftpOperation::FileJobGet
                    | SftpOperation::FileJobCancel)
            ) && j.session == scope
                && same_id(j.id)
                && j.revision >= expected
        }
        SftpWorkspaceReply::Edit { document, .. } => {
            matches!(
                SftpOperation::try_from(operation),
                Ok(SftpOperation::EditOpen
                    | SftpOperation::EditGet
                    | SftpOperation::EditLaunch
                    | SftpOperation::EditSave
                    | SftpOperation::EditClose
                    | SftpOperation::EditRebind)
            ) && document.session == scope
                && same_id(document.id)
                && document.revision >= expected
        }
    };
    if !valid {
        return Err("workspace response identity or operation mismatch");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|_| "workspace encoding failed")?
    );
    Ok(())
}
fn print_browse(
    scope: SessionId,
    side: &str,
    path: &str,
    links: &str,
    reply: SftpResponse,
) -> Result<(), &'static str> {
    let local = side == "local";
    if reply.entries.len() > 512
        || reply.transfer.is_some()
        || !reply.document_json.is_empty()
        || reply.browse_local != local
        || reply.browse_revision != 1
        || reply.browse_id.len() != 16
        || reply.browse_id.iter().all(|b| *b == 0)
        || links == "nofollow" && !(local && path.is_empty()) && reply.browse_directory != path
        || local && !std::path::Path::new(&reply.browse_directory).is_absolute()
        || !local && !sftp_remote_path(&reply.browse_directory, true)
        || !reply.entries.iter().all(|e| {
            !e.name.is_empty()
                && !e.name.contains(['/', '\\'])
                && !e.name.chars().any(char::is_control)
                && e.path.len() <= 4096
                && (1..=4).contains(&e.kind)
                && if local {
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
        return Err("managed directory reply mismatch");
    }
    let cursor =
        uuid::Uuid::from_slice(&reply.browse_id).map_err(|_| "invalid directory cursor")?;
    let entries:Vec<_>=reply.entries.iter().map(|e|serde_json::json!({"name":e.name,"path":e.path,"kind":e.kind,"size":e.size,"modified":e.modified_unix_seconds})).collect();
    println!("{}",serde_json::to_string_pretty(&serde_json::json!({"session":scope,"cursor":cursor,"revision":reply.browse_revision,"directory":reply.browse_directory,"local":local,"more":reply.truncated,"entries":entries})).map_err(|_|"directory encoding failed")?);
    Ok(())
}
