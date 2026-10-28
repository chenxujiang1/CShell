use super::super::verified::LocalSource;
use super::super::workspace_store as store;
use super::*;
async fn remote_text(
    target: &Arc<SshSession>,
    path: &str,
) -> Result<(String, SftpVerifiedContent), &'static str> {
    let client = target
        .open_sftp()
        .await
        .map_err(|_| "SFTP connection unavailable")?;
    let mut source = client.pinned_read();
    let result = async {
        let content = source
            .open(path, MAX_SFTP_INLINE_EDIT_BYTES as u64)
            .await
            .map_err(|_| "file is unavailable, changed or exceeds 256 KiB text limit")?;
        let mut bytes = Vec::with_capacity(content.size as usize);
        while bytes.len() < (content.size as usize) {
            let part = source
                .read(
                    bytes.len() as u64,
                    (content.size as usize - bytes.len()).min(32768),
                )
                .await
                .map_err(|_| "comparison file changed")?;
            if part.is_empty() {
                return Err("comparison file changed");
            }
            bytes.extend_from_slice(&part);
        }
        let digest = sftp_digest(&bytes);
        let after = source
            .digest()
            .await
            .map_err(|_| "comparison file changed")?;
        if digest.sha256 != content.sha256 || digest.sha256 != after.sha256 {
            return Err("comparison file changed");
        }
        Ok((
            String::from_utf8(bytes).map_err(|_| "comparison requires UTF-8 text")?,
            digest,
        ))
    }
    .await;
    source.close().await;
    source.abort();
    result
}
pub(super) async fn save_base(id: SessionId, bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > MAX_SFTP_INLINE_EDIT_BYTES {
        return Ok(());
    }
    let directory = store::directory(id)?;
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file =
            tempfile::NamedTempFile::new_in(&directory).map_err(|_| "base snapshot unavailable")?;
        file.write_all(&bytes)
            .map_err(|_| "base snapshot write failed")?;
        file.as_file()
            .sync_all()
            .map_err(|_| "base snapshot sync failed")?;
        file.persist(directory.join("base.txt"))
            .map_err(|_| "base snapshot publication unconfirmed")?;
        Ok(())
    })
    .await
    .map_err(|_| "base snapshot worker failed")?
}
async fn base_text(document: &SftpEditDocument) -> Option<String> {
    let path = store::directory(document.id).ok()?.join("base.txt");
    let mut source = LocalSource::open_bounded(&path, MAX_SFTP_INLINE_EDIT_BYTES as u64)
        .await
        .ok()?;
    let digest = source.digest().await.ok()?;
    if digest != document.base {
        return None;
    }
    let mut bytes = Vec::new();
    while bytes.len() < digest.size as usize {
        let part = source
            .read(
                bytes.len() as u64,
                (digest.size as usize - bytes.len()).min(32768),
            )
            .await
            .ok()?;
        if part.is_empty() {
            return None;
        }
        bytes.extend_from_slice(&part);
    }
    if sftp_digest(&bytes) != document.base {
        return None;
    }
    String::from_utf8(bytes).ok()
}
impl SftpIpcService {
    pub(super) async fn edit_review(
        &self,
        request: &SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> Result<SftpWorkspaceReply, &'static str> {
        let mutation = operation == SftpOperation::EditRebase;
        canonical(request, true, mutation, true)?;
        let id = id(&request.transfer_id)?;
        let lock = self.workspace.lock(id)?;
        let _guard = lock.lock().await;
        let mut document = store::read::<SftpEditDocument>(id).await?;
        if document.session != session
            || document.closed
            || document.state != SftpEditState::Ready
            || document.queued_item.is_some()
            || self.workspace.is_live(id)
        {
            return Err("editor is not ready for comparison");
        }
        let target = sessions
            .get_sftp(session)
            .map_err(|_| "original SFTP session unavailable")?;
        let epoch = target.generation();
        if !target.running() || target.clipboard_trust() != Some(&document.peer) {
            return Err("editor peer changed");
        }
        let (remote, remote_content) = remote_text(&target, &document.remote_path).await?;
        let client = target
            .open_sftp()
            .await
            .map_err(|_| "server permissions unavailable")?;
        let metadata = client
            .metadata(
                document.remote_path.clone(),
                cshell_sftp::SymlinkPolicy::NoFollow,
            )
            .await;
        let _ = client.close().await;
        let remote_permissions = metadata
            .map_err(|_| "server permissions unavailable")?
            .permissions
            .map(|p| p & 0o7777);
        if !target.running() || target.generation() != epoch {
            return Err("connection changed during comparison");
        }
        if mutation {
            let value: SftpEditRebase = serde_json::from_slice(&request.document_json)
                .map_err(|_| "invalid baseline request")?;
            if document.revision != request.expected_revision
                || remote_content != value.remote
                || remote_permissions != value.permissions
            {
                return Err("server or editor version changed; compare again");
            }
            save_base(id, remote.as_bytes()).await?;
            let previous = document.revision;
            document.revision += 1;
            document.base = remote_content;
            document.base_permissions = remote_permissions;
            document.detail =
                "Reviewed server version accepted as save baseline; no upload performed".into();
            store::write(&document, Some(previous)).await?;
            return Ok(SftpWorkspaceReply::Edit {
                document,
                text: None,
            });
        }
        let value: SftpEditReviewRequest = serde_json::from_slice(&request.document_json)
            .map_err(|_| "invalid comparison request")?;
        if value.revision != document.revision {
            return Err("editor version changed; refresh comparison");
        }
        let mut base = base_text(&document).await;
        if remote_content == document.base {
            base = Some(remote.clone());
            save_base(id, remote.as_bytes()).await?;
        }
        let queue = super::super::queue::Queues::load_async(session)
            .await
            .map_err(|_| "save queue unavailable")?;
        if queue
            .peer
            .as_ref()
            .is_some_and(|peer| peer != &document.peer)
        {
            return Err("save queue peer changed");
        }
        let mut backups = Vec::new();
        for row in queue.items.iter().rev().filter(|r| {
            r.state == SftpQueueState::Succeeded
                && r.draft.direction == SftpQueueDirection::Upload
                && r.draft.remote_path == document.remote_path
        }) {
            if let Some(receipt) = row.verified.as_ref().and_then(|v| v.publication)
                && receipt.backup_verified
                && let Some(previous) = receipt.previous
            {
                backups.push(SftpEditBackup {
                    item: row.id,
                    path: row.backup_path()?,
                    content: SftpVerifiedContent {
                        size: previous.size,
                        sha256: previous.sha256,
                    },
                });
                if backups.len() == 8 {
                    break;
                }
            }
        }
        let backup_text = if let Some(selected) = value.backup {
            let backup = backups
                .iter()
                .find(|b| b.item == selected)
                .ok_or("backup is not a verified backup of this file")?;
            let (text, content) = remote_text(&target, &backup.path).await?;
            if content != backup.content {
                return Err("backup content changed; restore refused");
            }
            Some(text)
        } else {
            None
        };
        if !target.running() || target.generation() != epoch {
            return Err("original connection changed");
        }
        Ok(SftpWorkspaceReply::Review(SftpEditReview {
            session,
            id,
            revision: document.revision,
            base_content: document.base,
            base,
            remote_content,
            remote_permissions,
            remote,
            backups,
            selected_backup: value.backup,
            backup_text,
        }))
    }
}
