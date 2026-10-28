use super::super::queue::Queues;
use super::super::verified::LocalSource;
use super::super::workspace_store as store;
use super::*;
use cshell_domain::SftpQueueItemId;
use sha2::{Digest, Sha256};
impl SftpIpcService {
    pub(super) async fn rebind_edit(
        &self,
        request: &SftpRequest,
        session: SessionId,
        sessions: &SshSessionRegistry,
    ) -> Result<SftpWorkspaceReply, &'static str> {
        canonical(request, false, true, true)?;
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
            return Err("editor operation or process still active");
        }
        let mut document = store::read::<SftpEditDocument>(id).await?;
        if document.revision != request.expected_revision
            || !matches!(document.state, SftpEditState::Ready | SftpEditState::Closed)
        {
            return Err("editor is not eligible for session rebinding");
        }
        let target = sessions
            .get_sftp(session)
            .map_err(|_| "target SSH session unavailable")?;
        if !target.running() || target.clipboard_trust() != Some(&document.peer) {
            return Err("target host, account, key or route differs");
        }
        validate_working(&document).await?;
        if document.queued_item.is_some() {
            let queue = latest_queue(&document).await?;
            if queue.session_id != session
                || !queue.items.iter().any(|i| {
                    Some(i.id) == document.queued_item
                        && document.queued_draft.as_ref() == Some(&i.draft)
                })
            {
                return Err("migrate the save queue to this session first");
            }
            reconcile(&mut document).await?;
        } else {
            let client = target
                .open_sftp()
                .await
                .map_err(|_| "target SFTP unavailable")?;
            let probe = client
                .inspect_entry(&document.remote_path, MAX_SFTP_EDIT_BYTES as u64)
                .await;
            let _ = client.close().await;
            if probe
                .map_err(|_| "remote editor base unavailable")?
                .and_then(|s| s.content)
                .map(|c| SftpVerifiedContent {
                    size: c.size,
                    sha256: c.sha256,
                })
                != Some(document.base)
            {
                return Err("remote editor base changed; open a fresh copy and merge explicitly");
            }
        }
        let previous = document.revision;
        document.revision += 1;
        document.session = session;
        document.detail =
            "Editor rebound to the reviewed verified session; no save dispatched".into();
        store::write(&document, Some(previous)).await?;
        Ok(SftpWorkspaceReply::Edit {
            document,
            text: None,
        })
    }
    pub(super) async fn edit_request(
        &self,
        request: &SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> Result<SftpWorkspaceReply, &'static str> {
        if operation == SftpOperation::EditOpen {
            canonical(request, true, false, false)?;
            if request.document_json.len() > 16 * 1024 {
                return Err("editor path limit");
            }
            let options: SftpBrowseOptions = serde_json::from_slice(&request.document_json)
                .map_err(|_| "invalid editor open options")?;
            if options.local || !sftp_remote_path(&options.path, false) {
                return Err("remote editor requires a remote file");
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
            store::check_capacity().await?;
            let id = SessionId::new();
            let native = SessionId::new();
            let control = TransferControl::default();
            let root = tokio::task::spawn_blocking(move || store::directory(id))
                .await
                .map_err(|_| "editor directory worker unavailable")??;
            let name = options
                .path
                .rsplit('/')
                .next()
                .ok_or("invalid remote name")?;
            let extension = name
                .rsplit_once('.')
                .map(|p| p.1)
                .filter(|s| {
                    !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_alphanumeric())
                })
                .unwrap_or("bin");
            let local_path = root.join(format!("working-{id}.{extension}"));
            let empty = SftpVerifiedContent {
                size: 0,
                sha256: Sha256::digest([]).into(),
            };
            let document = SftpEditDocument {
                native_transfer: Some(native),
                base_permissions: None,
                peer,
                state: SftpEditState::Preparing,
                queued_draft: None,
                format: "cshell.sftp-edit".into(),
                version: 1,
                id,
                session,
                revision: 1,
                remote_path: options.path.clone(),
                local_path: local_path
                    .to_str()
                    .ok_or("editor path is not UTF-8")?
                    .to_owned(),
                base: empty,
                current: empty,
                queued_item: None,
                closed: false,
                detail: "Downloading a verified private working copy".into(),
            };
            document.validate()?;
            self.register_transfer(
                session,
                native.as_uuid().as_bytes().to_vec(),
                control.clone(),
                1,
            )
            .map_err(|_| "SFTP capacity reached")?;
            if let Err(error) = store::write(&document, None).await {
                self.finish_workspace_native(native, false, false, error);
                return Err(error);
            }
            let owner = self.workspace.own(id);
            let service = self.clone();
            let response = document.clone();
            tokio::spawn(async move {
                let _owner = owner;
                service
                    .prepare_edit(document, options.links, target, native, control)
                    .await;
            });
            return Ok(SftpWorkspaceReply::Edit {
                document: response,
                text: None,
            });
        }
        canonical(
            request,
            matches!(
                operation,
                SftpOperation::EditLaunch | SftpOperation::EditSave
            ),
            operation != SftpOperation::EditGet,
            true,
        )?;
        let id = id(&request.transfer_id)?;
        let lock = self.workspace.lock(id)?;
        let _guard = lock.lock().await;
        let mut document = store::read::<SftpEditDocument>(id).await?;
        if document.session != session {
            return Err("editor session mismatch");
        }
        validate_working(&document).await?;
        if document.state == SftpEditState::Preparing && !self.workspace.is_live(id) {
            let previous = document.revision;
            document.revision += 1;
            document.state = SftpEditState::Unknown;
            document.detail = "Download interrupted; open a new reviewed copy".into();
            store::write(&document, Some(previous)).await?;
        }
        if operation == SftpOperation::EditGet {
            let before = document.clone();
            if document.state == SftpEditState::Ready && !document.closed {
                document.current = open_edit_source(std::path::Path::new(&document.local_path))
                    .await?
                    .digest()
                    .await?;
            }
            if document.queued_item.is_some() {
                reconcile(&mut document).await?;
            }
            if document != before {
                document.revision += 1;
                store::write(&document, Some(before.revision)).await?;
            }
            let text = if document.state == SftpEditState::Ready && !document.closed {
                read_text(&document).await?
            } else {
                None
            };
            return Ok(SftpWorkspaceReply::Edit { document, text });
        }
        if request.expected_revision != document.revision {
            return Err("editor revision changed; reload while retaining local draft");
        }
        if operation == SftpOperation::EditClose {
            if self.workspace.is_live(id) {
                return Err("editor download is still running; cancel its native operation first");
            }
            let previous = document.revision;
            document.revision += 1;
            document.closed = true;
            document.state = SftpEditState::Closed;
            document.detail =
                "Editor closed; private working copy and queued transfer retained".into();
            store::write(&document, Some(previous)).await?;
            return Ok(SftpWorkspaceReply::Edit {
                document,
                text: None,
            });
        }
        if document.closed || document.state != SftpEditState::Ready {
            return Err("editor is not ready");
        }
        let target = sessions
            .get_sftp(session)
            .map_err(|_| "SSH session unavailable")?;
        if !target.running() || target.clipboard_trust() != Some(&document.peer) {
            return Err("verified editor peer changed");
        }
        match operation {
            SftpOperation::EditLaunch => {
                let launch: SftpEditorLaunch = serde_json::from_slice(&request.document_json)
                    .map_err(|_| "invalid editor launch")?;
                if !std::path::Path::new(&launch.executable).is_absolute()
                    || launch.executable.len() > 4096
                    || launch.executable.chars().any(char::is_control)
                    || launch.arguments.len() > 32
                    || launch
                        .arguments
                        .iter()
                        .any(|a| a.len() > 4096 || a.contains('\0'))
                {
                    return Err(
                        "editor requires an absolute executable and bounded literal arguments",
                    );
                }
                {
                    let mut editors = self
                        .workspace
                        .editors
                        .lock()
                        .map_err(|_| "editor ownership unavailable")?;
                    if editors.len() >= 16 || editors.contains(&id) {
                        return Err("editor already running or process limit reached");
                    }
                    editors.insert(id);
                }
                let mut command = tokio::process::Command::new(&launch.executable);
                let mut has_file = false;
                for argument in launch.arguments {
                    if argument == "{file}" {
                        command.arg(&document.local_path);
                        has_file = true;
                    } else {
                        command.arg(argument);
                    }
                }
                if !has_file {
                    command.arg(&document.local_path);
                }
                command
                    .current_dir(
                        std::path::Path::new(&document.local_path)
                            .parent()
                            .ok_or("invalid working parent")?,
                    )
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                #[cfg(windows)]
                command.creation_flags(0x0800_0000);
                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(_) => {
                        self.workspace
                            .editors
                            .lock()
                            .map_err(|_| "editor ownership unavailable")?
                            .remove(&id);
                        return Err("external editor could not be started");
                    }
                };
                let workspace = self.workspace.clone();
                tokio::spawn(async move {
                    let _exit = child.wait().await;
                    workspace
                        .editors
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id);
                });
                let previous = document.revision;
                document.revision += 1;
                document.detail = "External editor launched; changed content is detected on reload and is never uploaded automatically".into();
                store::write(&document, Some(previous)).await?;
                Ok(SftpWorkspaceReply::Edit {
                    document,
                    text: None,
                })
            }
            SftpOperation::EditSave => {
                let save: SftpEditSave = serde_json::from_slice(&request.document_json)
                    .map_err(|_| "invalid editor save")?;
                if !save.accept_exclusive_replace
                    || save.queue_revision == 0
                    || save
                        .content
                        .as_ref()
                        .is_some_and(|t| t.len() > MAX_SFTP_EDIT_BYTES)
                {
                    return Err(
                        "save requires reviewed queue revision and explicit exclusive backup replacement",
                    );
                }
                if document.queued_item.is_none() {
                    let epoch=target.generation();
                    let client=target.open_sftp().await.map_err(|_|"保存前无法读取服务器版本；本地草稿保留")?;
                    let checked=client.inspect_entry(&document.remote_path,MAX_SFTP_EDIT_BYTES as u64).await;
                    let _=client.close().await;
                    let checked=checked.map_err(|_|"保存前无法核对服务器版本；本地草稿保留")?;
                    if !target.running()||target.generation()!=epoch || checked.as_ref().is_none_or(|entry|
                        entry.content.map(|c|SftpVerifiedContent{size:c.size,sha256:c.sha256})!=Some(document.base)
                        || entry.metadata.permissions.map(|p|p&0o7777)!=document.base_permissions) {
                        return Err("服务器文件已变化，尚未上传。请点击“查看差异 / 冲突 / 备份”，核对后接受保存基线；本地草稿保留");
                    }
                    let actual = open_edit_source(std::path::Path::new(&document.local_path))
                        .await?
                        .digest()
                        .await?;
                    if actual != document.current {
                        return Err("working copy changed outside this view; reload before saving");
                    }
                    let bytes = if let Some(text) = save.content {
                        let bytes = text.into_bytes();
                        write_working(&document, bytes.clone()).await?;
                        bytes
                    } else {
                        read_working(&document).await?
                    };
                    let content = fingerprint(&bytes);
                    let item = SftpQueueItemId::new();
                    let directory = store::directory(id)?;
                    let frozen = directory.join(format!("upload-{item}.bin"));
                    persist_new(frozen.clone(), bytes).await?;
                    let draft = SftpQueueDraft {
                        destination_permissions: document.base_permissions.map(|p| p & 0o777),
                        expected_permissions: document.base_permissions,
                        verified: true,
                        direction: SftpQueueDirection::Upload,
                        local_path: frozen
                            .to_str()
                            .ok_or("frozen editor path is not UTF-8")?
                            .to_owned(),
                        remote_path: document.remote_path.clone(),
                        publication_policy: Some(FilePublicationPolicy::BackupReplace),
                        expected_source: Some(content),
                        expected_destination: Some(document.base),
                        ..Default::default()
                    };
                    let previous = document.revision;
                    document.revision += 1;
                    document.current = content;
                    document.queued_item = Some(item);
                    document.queued_draft = Some(draft);
                    document.detail =
                        "Remote save intent recorded; inspect queue if reply is lost".into();
                    store::write(&document, Some(previous)).await?;
                }
                let item = document
                    .queued_item
                    .ok_or("missing editor upload identity")?;
                let draft = document
                    .queued_draft
                    .clone()
                    .ok_or("missing editor upload draft")?;
                let queue_lock = self
                    .queues
                    .lock(session)
                    .map_err(|_| "editor queue unavailable")?;
                let queue = {
                    let _queue = queue_lock.lock().await;
                    let mut queue = Queues::load_async(session)
                        .await
                        .map_err(|_| "editor queue unavailable")?;
                    if queue.migrated_to.is_some()
                        || queue.revision != save.queue_revision
                        || queue.peer.as_ref().is_some_and(|p| p != &document.peer)
                    {
                        return Err(
                            "editor queue changed; reload before finishing the same save intent",
                        );
                    }
                    if let Some(row) = queue.items.iter().find(|r| r.id == item) {
                        if row.draft != draft || row.state != SftpQueueState::Pending {
                            return Err(
                                "existing editor save is already dispatched; inspect its queue item",
                            );
                        }
                    } else {
                        if queue.items.len() >= MAX_SFTP_QUEUE_ITEMS {
                            return Err("editor queue full");
                        }
                        queue.peer = Some(document.peer.clone());
                        queue.paused = true;
                        queue.items.push(SftpQueueItem {
                            verified: None,
                            id: item,
                            draft,
                            state: SftpQueueState::Pending,
                            transfer_id: None,
                            receipt: None,
                        });
                        queue.changed().map_err(|_| "invalid editor queue")?;
                        Queues::save_async(&queue)
                            .await
                            .map_err(|_| "editor queue insertion unconfirmed")?;
                    }
                    queue
                };
                let start = SftpRequest {
                    operation: SftpOperation::QueueStartOne as i32,
                    session_id: session.as_uuid().as_bytes().to_vec(),
                    transfer_id: item.as_uuid().as_bytes().to_vec(),
                    expected_revision: queue.revision,
                    confirmed: true,
                    ..Default::default()
                };
                let reply = self
                    .queue(start, session, SftpOperation::QueueStartOne, sessions)
                    .await;
                let previous = document.revision;
                document.revision += 1;
                document.detail = if reply.status == SftpStatus::Ok as i32 { "Verified remote save started; other queue items remain paused" } else { "Upload is saved in the queue; explicitly start this item after other work finishes" }.into();
                store::write(&document, Some(previous)).await?;
                Ok(SftpWorkspaceReply::Edit {
                    document,
                    text: None,
                })
            }
            _ => Err("unsupported editor operation"),
        }
    }
    async fn prepare_edit(
        &self,
        mut document: SftpEditDocument,
        links: SftpLinkPolicy,
        target: Arc<SshSession>,
        native: SessionId,
        control: TransferControl,
    ) {
        let operation = async {
            let client = target.open_sftp().await.map_err(|_| "SFTP unavailable")?;
            let remote_path = if links == SftpLinkPolicy::Follow {
                client
                    .resolve_path(&document.remote_path)
                    .await
                    .map_err(|_| "remote link unavailable")?
            } else {
                document.remote_path.clone()
            };
            let permissions = client
                .metadata(remote_path.clone(), cshell_sftp::SymlinkPolicy::NoFollow)
                .await
                .map_err(|_| "remote permissions unavailable")?
                .permissions
                .map(|p| p & 0o7777);
            let mut source = client.pinned_read();
            let result = async {
                let content = source
                    .open(&remote_path, MAX_SFTP_EDIT_BYTES as u64)
                    .await
                    .map_err(|_| "remote edit source unavailable or too large")?;
                let mut bytes = Vec::with_capacity(content.size as usize);
                let mut offset = 0;
                while offset < content.size {
                    let n = (content.size - offset).min(32 * 1024) as usize;
                    let chunk = source
                        .read(offset, n)
                        .await
                        .map_err(|_| "remote edit read failed")?;
                    if chunk.is_empty() {
                        return Err("remote edit source changed");
                    }
                    offset += chunk.len() as u64;
                    bytes.extend_from_slice(&chunk);
                }
                let content = SftpVerifiedContent {
                    size: content.size,
                    sha256: content.sha256,
                };
                if fingerprint(&bytes) != content
                    || source
                        .digest()
                        .await
                        .map_err(|_| "remote source recheck failed")?
                        .sha256
                        != content.sha256
                {
                    return Err("remote edit digest mismatch");
                }
                super::review::save_base(document.id,&bytes).await?;
                persist_new(PathBuf::from(&document.local_path), bytes).await?;
                Ok((remote_path, content, permissions))
            }
            .await;
            source.close().await;
            source.abort();
            result
        };
        let result = tokio::select! { _ = cancelled(&control) => Err("Remote edit preparation cancelled"), _ = tokio::time::sleep(std::time::Duration::from_secs(10 * 60)) => Err("Remote edit preparation timed out"), result = operation => result };
        let previous = document.revision;
        document.revision += 1;
        match result {
            Ok((path, content, permissions)) => {
                document.base_permissions = permissions;
                document.remote_path = path;
                document.base = content;
                document.current = content;
                document.state = SftpEditState::Ready;
                document.detail =
                    "Verified working copy ready; remote saves use an independent backup".into();
            }
            Err(error) => {
                document.state = SftpEditState::Unknown;
                document.detail = error.into();
            }
        }
        let saved = if let Ok(lock) = self.workspace.lock(document.id) {
            let _guard = lock.lock().await;
            store::write(&document, Some(previous)).await.is_ok()
        } else {
            false
        };
        self.finish_workspace_native(
            native,
            saved && document.state == SftpEditState::Ready,
            false,
            &document.detail,
        );
    }
}
async fn open_edit_source(path: &std::path::Path) -> Result<LocalSource, &'static str> {
    LocalSource::open_bounded(path, MAX_SFTP_EDIT_BYTES as u64).await
}
async fn latest_queue(document: &SftpEditDocument) -> Result<SftpQueueSnapshot, &'static str> {
    let mut queue = Queues::load_async(document.session)
        .await
        .map_err(|_| "editor queue unavailable")?;
    let mut seen = BTreeSet::new();
    for _ in 0..128 {
        if queue.peer.as_ref() != Some(&document.peer) || !seen.insert(queue.session_id) {
            return Err("editor queue peer or migration chain changed");
        }
        let Some(next) = queue.migrated_to else {
            return Ok(queue);
        };
        let target = Queues::load_async(next)
            .await
            .map_err(|_| "editor migration target unavailable")?;
        if target.migration_origin.as_ref().is_none_or(|o| {
            o.session != queue.session_id || Some(o.digest) != queue.migration_digest
        }) {
            return Err("editor queue migration is incomplete");
        }
        queue = target;
    }
    Err("editor migration chain limit")
}
async fn reconcile(document: &mut SftpEditDocument) -> Result<(), &'static str> {
    let queue = latest_queue(document).await?;
    let Some(row) = queue
        .items
        .iter()
        .find(|i| Some(i.id) == document.queued_item)
    else {
        document.detail = "Save intent has no confirmed queue entry; explicitly finish the same insertion or retain its private files".into();
        return Ok(());
    };
    if row.state == SftpQueueState::Succeeded {
        document.base_permissions = row
            .draft
            .destination_permissions
            .or(document.base_permissions);
        document.base = row
            .verified
            .as_ref()
            .and_then(|v| v.content)
            .ok_or("missing editor commit")?;
        document.queued_item = None;
        document.queued_draft = None;
        document.detail = "Remote save confirmed; previous target backup retained".into();
    } else if row.state == SftpQueueState::Unknown
        && row
            .verified
            .as_ref()
            .is_some_and(|v| v.cleaned && !v.committed)
    {
        document.queued_item = None;
        document.queued_draft = None;
        document.detail = "Uncommitted transfer cleaned; local changes retained".into();
    } else {
        document.detail = format!(
            "Save queue item {:?}; recover it before another save",
            row.state
        );
    }
    Ok(())
}
async fn validate_working(document: &SftpEditDocument) -> Result<(), &'static str> {
    let id = document.id;
    let directory = tokio::task::spawn_blocking(move || store::directory(id))
        .await
        .map_err(|_| "editor directory unavailable")??;
    let path = std::path::Path::new(&document.local_path);
    if path.parent() != Some(directory.as_path())
        || !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("working-"))
    {
        return Err("editor working path is not controlled");
    }
    Ok(())
}
fn fingerprint(bytes: &[u8]) -> SftpVerifiedContent {
    SftpVerifiedContent {
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
    }
}
async fn read_working(document: &SftpEditDocument) -> Result<Vec<u8>, &'static str> {
    let before = open_edit_source(std::path::Path::new(&document.local_path))
        .await?
        .digest()
        .await?;
    if before.size > MAX_SFTP_EDIT_BYTES as u64 {
        return Err("working file limit");
    }
    let mut source = open_edit_source(std::path::Path::new(&document.local_path)).await?;
    let mut bytes = Vec::with_capacity(before.size as usize);
    let mut offset = 0;
    while offset < before.size {
        let chunk = source
            .read(offset, (before.size - offset).min(32 * 1024) as usize)
            .await?;
        if chunk.is_empty() {
            return Err("working copy changed while reading");
        }
        offset += chunk.len() as u64;
        bytes.extend_from_slice(&chunk);
    }
    if fingerprint(&bytes) != before
        || open_edit_source(std::path::Path::new(&document.local_path))
            .await?
            .digest()
            .await?
            != before
    {
        return Err("working copy changed while reading");
    }
    Ok(bytes)
}
async fn read_text(document: &SftpEditDocument) -> Result<Option<String>, &'static str> {
    if document.current.size > MAX_SFTP_INLINE_EDIT_BYTES as u64 {
        return Ok(None);
    }
    let bytes = read_working(document).await?;
    if fingerprint(&bytes) != document.current {
        return Err("working copy changed after editor metadata capture");
    }
    let text = String::from_utf8(bytes).ok();
    Ok(text.filter(|t| {
        serde_json::to_vec(t).is_ok_and(|b| b.len() <= MAX_SFTP_WORKSPACE_DOCUMENT - 32 * 1024)
    }))
}
async fn persist_new(path: PathBuf, bytes: Vec<u8>) -> Result<(), &'static str> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let directory = path.parent().ok_or("invalid controlled file parent")?;
        let count = std::fs::read_dir(directory)
            .map_err(|_| "editor directory unavailable")?
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("upload-"))
            })
            .count();
        if count >= 16 {
            return Err("editor snapshot limit; close and explicitly forget old private snapshots");
        }
        let mut temporary = tempfile::NamedTempFile::new_in(directory)
            .map_err(|_| "controlled temporary unavailable")?;
        temporary
            .write_all(&bytes)
            .map_err(|_| "controlled file write failed")?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| "controlled file sync failed")?;
        temporary
            .persist_noclobber(&path)
            .map_err(|_| "controlled file already exists or publication unconfirmed")?;
        #[cfg(unix)]
        std::fs::File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| "controlled directory sync unconfirmed")?;
        Ok(())
    })
    .await
    .map_err(|_| "controlled file worker unavailable")?
}
async fn write_working(document: &SftpEditDocument, bytes: Vec<u8>) -> Result<(), &'static str> {
    let path = PathBuf::from(&document.local_path);
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let directory = path.parent().ok_or("invalid controlled file parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)
            .map_err(|_| "editor temporary unavailable")?;
        temporary
            .write_all(&bytes)
            .map_err(|_| "editor write failed")?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| "editor sync failed")?;
        temporary
            .persist(&path)
            .map_err(|_| "editor publication unconfirmed")?;
        #[cfg(unix)]
        std::fs::File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| "editor parent sync failed")?;
        Ok(())
    })
    .await
    .map_err(|_| "editor file worker unavailable")?
}
