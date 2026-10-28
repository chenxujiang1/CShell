use super::*;
use cshell_application::*;
use std::collections::BTreeSet;
mod editing;
mod review;
mod files;
#[derive(Clone, Debug, Default)]
pub(super) struct Workspace {
    locks: Arc<Mutex<BTreeMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>,
    live: Arc<Mutex<BTreeSet<SessionId>>>,
    editors: Arc<Mutex<BTreeSet<SessionId>>>,
}
struct Live {
    workspace: Workspace,
    id: SessionId,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.workspace
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}
impl Workspace {
    fn lock(&self, id: SessionId) -> Result<Arc<tokio::sync::Mutex<()>>, &'static str> {
        let mut locks = self.locks.lock().map_err(|_| "workspace unavailable")?;
        if locks.len() >= 1024 {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        if locks.len() >= 1024 && !locks.contains_key(&id) {
            return Err("workspace lock capacity reached");
        }
        Ok(locks.entry(id).or_default().clone())
    }
    fn is_live(&self, id: SessionId) -> bool {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id)
    }
    fn own(&self, id: SessionId) -> Live {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
        Live {
            workspace: self.clone(),
            id,
        }
    }
}
fn id(bytes: &[u8]) -> Result<SessionId, &'static str> {
    let id = SessionId::from_bytes(bytes.try_into().map_err(|_| "invalid workspace identity")?);
    if id.as_uuid().is_nil() {
        return Err("nil workspace identity");
    }
    Ok(id)
}
fn canonical(
    request: &SftpRequest,
    document: bool,
    mutation: bool,
    identity: bool,
) -> Result<(), &'static str> {
    let canonical = SftpRequest {
        operation: request.operation,
        session_id: request.session_id.clone(),
        document_json: if document {
            request.document_json.clone()
        } else {
            Vec::new()
        },
        expected_revision: if mutation {
            request.expected_revision
        } else {
            0
        },
        confirmed: mutation,
        transfer_id: if identity {
            request.transfer_id.clone()
        } else {
            Vec::new()
        },
        ..Default::default()
    };
    if canonical != *request
        || mutation && request.expected_revision == 0
        || request.document_json.len() > MAX_SFTP_WORKSPACE_DOCUMENT
    {
        return Err("invalid workspace request shape");
    }
    Ok(())
}
async fn cancelled(control: &TransferControl) {
    loop {
        if control.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
impl SftpIpcService {
    pub(super) async fn workspace_request(
        &self,
        request: SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        let result = match operation {
            SftpOperation::EditReview|SftpOperation::EditRebase=>self.edit_review(&request,session,operation,sessions).await,
            SftpOperation::QueueAddMany | SftpOperation::QueueEnqueueAndStart => {
                self.queue_many(&request, session, sessions).await
            }
            SftpOperation::PeerGet => {
                if let Err(e) = canonical(&request, false, false, false) {
                    Err(e)
                } else {
                    sessions
                        .get_sftp(session)
                        .ok()
                        .filter(|s| s.running())
                        .and_then(|s| s.clipboard_trust().cloned())
                        .map(|peer| SftpWorkspaceReply::Peer { session, peer })
                        .ok_or("verified peer unavailable")
                }
            }
            SftpOperation::WorkspaceList => {
                if let Err(e) = canonical(&request, true, false, false) {
                    Err(e)
                } else {
                    match serde_json::from_slice::<SftpWorkspacePage>(&request.document_json) {
                        Ok(page) => super::workspace_store::history(session, page).await,
                        Err(_) => Err("invalid history page"),
                    }
                }
            }
            SftpOperation::WorkspaceForget => self.forget_workspace(&request, session).await,
            SftpOperation::EditRebind => self.rebind_edit(&request, session, sessions).await,
            SftpOperation::FileInspect
            | SftpOperation::FileApply
            | SftpOperation::FileJobGet
            | SftpOperation::FileJobCancel => {
                self.file_request(&request, session, operation, sessions)
                    .await
            }
            SftpOperation::EditOpen
            | SftpOperation::EditGet
            | SftpOperation::EditLaunch
            | SftpOperation::EditSave
            | SftpOperation::EditClose => {
                self.edit_request(&request, session, operation, sessions)
                    .await
            }
            _ => Err("unknown workspace operation"),
        };
        match result {
            Ok(value) => match serde_json::to_vec(&value) {
                Ok(bytes) if bytes.len() <= MAX_SFTP_WORKSPACE_DOCUMENT => SftpResponse {
                    document_json: bytes,
                    ..Default::default()
                },
                _ => SftpResponse::with_status(SftpStatus::Failed, "Workspace reply exceeds limit"),
            },
            Err(detail) => SftpResponse::with_status(SftpStatus::Conflict, detail),
        }
    }
    async fn queue_many(
        &self,
        request: &SftpRequest,
        session: SessionId,
        sessions: &SshSessionRegistry,
    ) -> Result<SftpWorkspaceReply, &'static str> {
        canonical(request, true, true, false)?;
        if request.document_json.len() > 1024 * 1024 {
            return Err("bulk queue document limit");
        }
        let drafts: Vec<SftpQueueDraft> = serde_json::from_slice(&request.document_json)
            .map_err(|_| "invalid bulk queue drafts")?;
        if drafts.is_empty() || drafts.len() > MAX_SFTP_QUEUE_ITEMS {
            return Err("bulk queue item limit");
        }
        let target = sessions
            .get_sftp(session)
            .map_err(|_| "SSH session unavailable")?;
        if !target.running() {
            return Err("SSH session unavailable");
        }
        let peer = target
            .clipboard_trust()
            .cloned()
            .ok_or("verified peer unavailable")?;
        for draft in &drafts {
            draft.validate()?;
            if !draft.verified {
                return Err("bulk queue requires verified mode");
            }
            if draft.direction == SftpQueueDirection::Upload {
                let path = if draft.follow_links {
                    tokio::fs::canonicalize(&draft.local_path)
                        .await
                        .map_err(|_| "dropped source link unavailable")?
                } else {
                    PathBuf::from(&draft.local_path)
                };
                let _source = super::verified::LocalSource::open(&path).await?;
            } else {
                let client = target.open_sftp().await.map_err(|_| "SFTP unavailable")?;
                let metadata = client
                    .metadata(
                        draft.remote_path.clone(),
                        if draft.follow_links {
                            cshell_sftp::SymlinkPolicy::Follow
                        } else {
                            cshell_sftp::SymlinkPolicy::NoFollow
                        },
                    )
                    .await;
                let _ = client.close().await;
                if metadata
                    .map_err(|_| "download source unavailable")?
                    .file_type
                    != RemoteFileType::File
                {
                    return Err("drop supports regular files; navigate folders through the tree");
                }
            }
        }
        let lock = self.queues.lock(session).map_err(|_| "queue unavailable")?;
        let _guard = lock.lock().await;
        let mut queue = super::queue::Queues::load_async(session)
            .await
            .map_err(|_| "queue unavailable")?;
        let start = request.operation == SftpOperation::QueueEnqueueAndStart as i32;
        if queue.revision != request.expected_revision {
            return Err("queue revision changed");
        }
        if queue.migrated_to.is_some()
            || queue.peer.as_ref().is_some_and(|p| p != &peer)
            || queue.items.len() + drafts.len() > MAX_SFTP_QUEUE_ITEMS
        {
            return Err("queue changed, migrated or full");
        }
        if let Some(origin) = &queue.migration_origin {
            let source = super::queue::Queues::load_async(origin.session)
                .await
                .map_err(|_| "migration source unavailable")?;
            if source.migrated_to != Some(session) || source.migration_digest != Some(origin.digest)
            {
                return Err("migration incomplete");
            }
        }
        queue.peer = Some(peer);
        // Immediate browser transfers use three workers; explicit settings remain authoritative.
        queue.concurrency.get_or_insert(3);
        if !start {
            queue.paused = true;
        } else {
            queue.paused = false;
        }
        for draft in drafts {
            queue.items.push(SftpQueueItem {
                verified: None,
                id: cshell_domain::SftpQueueItemId::new(),
                draft,
                state: SftpQueueState::Pending,
                transfer_id: None,
                receipt: None,
            });
        }
        queue
            .changed()
            .map_err(|_| "invalid parallel queue or duplicate destination")?;
        super::queue::Queues::save_async(&queue)
            .await
            .map_err(|_| "bulk insertion unconfirmed")?;
        if start {
            self.ensure_queue_runner(session, target)
                .map_err(|_| "files saved; queue start unavailable")?;
        }
        Ok(SftpWorkspaceReply::Queue(queue))
    }
    async fn forget_workspace(
        &self,
        request: &SftpRequest,
        session: SessionId,
    ) -> Result<SftpWorkspaceReply, &'static str> {
        canonical(request, true, true, true)?;
        let options: SftpWorkspaceForget =
            serde_json::from_slice(&request.document_json).map_err(|_| "invalid forget options")?;
        let id = id(&request.transfer_id)?;
        let lock = self.workspace.lock(id)?;
        let _guard = lock.lock().await;
        if self.workspace.is_live(id)
            || self
                .workspace
                .editors
                .lock()
                .map_err(|_| "editor ownership unavailable")?
                .contains(&id)
        {
            return Err("workspace operation or external editor is still running");
        }
        let retained = if options.editor {
            let edit = super::workspace_store::read::<SftpEditDocument>(id).await?;
            if edit.session != session
                || edit.revision != request.expected_revision
                || !edit.closed
                || options.discard_private_files && edit.queued_item.is_some()
            {
                return Err("close the editor and reconcile its queued save before forgetting");
            }
            super::workspace_store::forget(edit, options.discard_private_files).await?
        } else {
            let job = super::workspace_store::read::<SftpFileJob>(id).await?;
            if job.session != session
                || job.revision != request.expected_revision
                || matches!(
                    job.state,
                    SftpFileState::Inspecting | SftpFileState::Applying
                )
            {
                return Err("filesystem job is not terminal or revision changed");
            }
            super::workspace_store::forget(job, false).await?
        };
        Ok(SftpWorkspaceReply::Forgotten {
            session,
            id,
            retained_files: retained,
        })
    }
    fn finish_workspace_native(
        &self,
        native: SessionId,
        succeeded: bool,
        committed: bool,
        detail: &str,
    ) {
        if let Some(job) = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(native.as_uuid().as_bytes().as_slice())
        {
            job.snapshot.state = if succeeded {
                SftpTransferState::Succeeded as i32
            } else {
                SftpTransferState::Failed as i32
            };
            job.snapshot.destination_committed = committed;
            job.snapshot.detail = detail.to_owned();
        }
    }
}
