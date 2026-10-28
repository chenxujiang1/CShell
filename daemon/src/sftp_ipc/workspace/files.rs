use super::super::queue::Queues;
use super::super::workspace_store as store;
use super::*;
use cshell_sftp::SftpClient;
impl SftpIpcService {
    pub(super) async fn file_request(
        &self,
        request: &SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> Result<SftpWorkspaceReply, &'static str> {
        if operation == SftpOperation::FileInspect {
            canonical(request, true, false, false)?;
            if request.document_json.len() > 16 * 1024 {
                return Err("file draft limit");
            }
            let draft: SftpFileDraft =
                serde_json::from_slice(&request.document_json).map_err(|_| "invalid file draft")?;
            draft.validate()?;
            let target = sessions
                .get_sftp(session)
                .map_err(|_| "SSH session unavailable")?;
            if !target.running() {
                return Err("SSH session unavailable");
            }
            let peer = target
                .clipboard_trust()
                .cloned()
                .ok_or("verified peer identity unavailable")?;
            store::check_capacity().await?;
            let id = SessionId::new();
            let native = SessionId::new();
            let control = TransferControl::default();
            self.register_transfer(
                session,
                native.as_uuid().as_bytes().to_vec(),
                control.clone(),
                1,
            )
            .map_err(|_| "SFTP capacity reached")?;
            let job = SftpFileJob {
                peer,
                native_transfer: native,
                format: "cshell.sftp-file-job".into(),
                version: 1,
                id,
                session,
                revision: 1,
                draft,
                state: SftpFileState::Inspecting,
                source: None,
                target: None,
                intent: false,
                acknowledged: false,
                detail: "Inspecting filesystem state".into(),
            };
            if let Err(error) = store::write(&job, None).await {
                self.finish_workspace_native(native, false, false, error);
                return Err(error);
            }
            let owner = self.workspace.own(id);
            let service = self.clone();
            let response = job.clone();
            tokio::spawn(async move {
                let _owner = owner;
                service.inspect_file(job, target, control).await;
            });
            return Ok(SftpWorkspaceReply::File(response));
        }
        canonical(request, false, operation != SftpOperation::FileJobGet, true)?;
        let id = id(&request.transfer_id)?;
        let lock = self.workspace.lock(id)?;
        let _guard = lock.lock().await;
        let mut job = store::read::<SftpFileJob>(id).await?;
        if job.session != session {
            return Err("file job session mismatch");
        }
        if !self.workspace.is_live(id)
            && matches!(
                job.state,
                SftpFileState::Inspecting | SftpFileState::Applying
            )
        {
            let previous = job.revision;
            job.revision += 1;
            job.state = SftpFileState::Unknown;
            job.detail = "Worker interrupted; inspect current filesystem with a new review".into();
            store::write(&job, Some(previous)).await?;
        }
        if operation == SftpOperation::FileJobGet {
            return Ok(SftpWorkspaceReply::File(job));
        }
        if job.revision != request.expected_revision {
            return Err("file job revision changed");
        }
        if operation == SftpOperation::FileJobCancel {
            if let Some(native) = self
                .jobs
                .lock()
                .map_err(|_| "native status unavailable")?
                .get(&job.native_transfer.as_uuid().as_bytes().to_vec())
            {
                if native.session_id == session {
                    native.control.cancel();
                }
            }
            return Ok(SftpWorkspaceReply::File(job));
        }
        if job.state != SftpFileState::Ready
            || job.draft.action == SftpFileAction::Inspect
            || self.workspace.is_live(id)
        {
            return Err("file job is not eligible for application");
        }
        let target = sessions
            .get_sftp(session)
            .map_err(|_| "SSH session unavailable")?;
        if !target.running() || target.clipboard_trust() != Some(&job.peer) {
            return Err("verified peer changed");
        }
        let queue_lock = self.queues.lock(session).map_err(|_| "queue unavailable")?;
        let _queue = queue_lock.lock().await;
        if Queues::load_async(session)
            .await
            .map_err(|_| "queue unavailable")?
            .migrated_to
            .is_some()
        {
            return Err("session queue was migrated");
        }
        let native = SessionId::new();
        let control = TransferControl::default();
        self.register_transfer(
            session,
            native.as_uuid().as_bytes().to_vec(),
            control.clone(),
            1,
        )
        .map_err(|_| "SFTP capacity reached")?;
        let previous = job.revision;
        job.revision += 1;
        job.native_transfer = native;
        job.state = SftpFileState::Applying;
        job.detail = "Rechecking reviewed source and destination".into();
        if let Err(error) = store::write(&job, Some(previous)).await {
            self.finish_workspace_native(native, false, false, error);
            return Err(error);
        }
        let owner = self.workspace.own(id);
        let response = job.clone();
        let service = self.clone();
        tokio::spawn(async move {
            let _owner = owner;
            service.apply_file(job, target, control).await;
        });
        Ok(SftpWorkspaceReply::File(response))
    }
    async fn inspect_file(
        &self,
        mut job: SftpFileJob,
        target: Arc<SshSession>,
        control: TransferControl,
    ) {
        let operation = async {
            let mut filesystem = Filesystem::open(job.draft.local, &target).await?;
            let source = filesystem.inspect(&job.draft.path).await?;
            let destination = if matches!(job.draft.action, SftpFileAction::Rename) {
                filesystem.inspect(&job.draft.target).await?
            } else {
                None
            };
            eligible(&job.draft, source.as_ref(), destination.as_ref())?;
            filesystem.close().await;
            Ok::<_, &'static str>((source, destination))
        };
        let result = tokio::select! { _ = cancelled(&control) => Err("Inspection cancelled"), _ = tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)) => Err("Inspection timed out"), result = operation => result };
        let previous = job.revision;
        job.revision += 1;
        match result {
            Ok((source, target)) => {
                job.source = source;
                job.target = target;
                job.state = SftpFileState::Ready;
                job.detail = "Review ready; applying requires separate confirmation".into();
            }
            Err(error) => {
                job.state = if control.is_cancelled() {
                    SftpFileState::Cancelled
                } else {
                    SftpFileState::Unknown
                };
                job.detail = error.into();
            }
        }
        let lock = self.workspace.lock(job.id);
        let saved = if let Ok(lock) = lock {
            let _guard = lock.lock().await;
            store::write(&job, Some(previous)).await.is_ok()
        } else {
            false
        };
        self.finish_workspace_native(
            job.native_transfer,
            saved && job.state == SftpFileState::Ready,
            false,
            &job.detail,
        );
    }
    async fn apply_file(
        &self,
        mut job: SftpFileJob,
        target: Arc<SshSession>,
        control: TransferControl,
    ) {
        let result = async {
            let namespace=self.namespaces.scope(job.draft.local,&job.peer)?;
            let _namespace=tokio::select!{_ = cancelled(&control)=>return Err("Cancelled while waiting for active transfers"),guard=namespace.write_owned()=>guard};
            let mut filesystem = Filesystem::open(job.draft.local, &target).await?;
            if filesystem.inspect(&job.draft.path).await? != job.source || job.draft.action == SftpFileAction::Rename && filesystem.inspect(&job.draft.target).await? != job.target { return Err("Reviewed filesystem state changed; no operation dispatched"); }
            if control.is_cancelled() { return Err("Cancelled before file mutation"); }
            let lock = self.workspace.lock(job.id)?; { let _guard = lock.lock().await; let previous = job.revision; job.revision += 1; job.intent = true; store::write(&job, Some(previous)).await?; }
            // Cancellation cannot prove that a sent filesystem mutation was stopped.
            let result = tokio::select! { _ = cancelled(&control) => Err("File mutation unconfirmed after cancellation"), _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => Err("File mutation unconfirmed after timeout"), result = filesystem.apply(&job.draft, job.source.as_ref()) => result };
            filesystem.close().await; result
        }.await;
        let previous = job.revision;
        job.revision += 1;
        match result {
            Ok(()) => {
                job.state = SftpFileState::Succeeded;
                job.acknowledged = true;
                job.detail = "Filesystem operation acknowledged".into();
            }
            Err(error) => {
                job.state = SftpFileState::Unknown;
                job.detail = error.into();
            }
        }
        let saved = if let Ok(lock) = self.workspace.lock(job.id) {
            let _guard = lock.lock().await;
            store::write(&job, Some(previous)).await.is_ok()
        } else {
            false
        };
        self.finish_workspace_native(
            job.native_transfer,
            saved && job.acknowledged,
            saved && job.acknowledged,
            &job.detail,
        );
    }
}
fn eligible(
    draft: &SftpFileDraft,
    source: Option<&SftpEntryStamp>,
    target: Option<&SftpEntryStamp>,
) -> Result<(), &'static str> {
    match draft.action {
        SftpFileAction::Mkdir | SftpFileAction::Symlink if source.is_some() => {
            Err("new path already exists")
        }
        SftpFileAction::Rename if source.is_none() || target.is_some() => {
            Err("rename source missing or destination exists")
        }
        SftpFileAction::Delete if source.is_none_or(|s| s.kind == 4) => {
            Err("delete requires a regular file, link or empty directory")
        }
        SftpFileAction::Chmod if source.is_none_or(|s| !matches!(s.kind, 1 | 2)) => {
            Err("chmod does not follow links")
        }
        _ => Ok(()),
    }
}
enum Filesystem {
    Local,
    Remote(SftpClient),
}
impl Drop for Filesystem {
    fn drop(&mut self) {
        if let Self::Remote(client) = self {
            client.abort_channel();
        }
    }
}
impl Filesystem {
    async fn open(local: bool, target: &SshSession) -> Result<Self, &'static str> {
        if local {
            Ok(Self::Local)
        } else {
            Ok(Self::Remote(
                target.open_sftp().await.map_err(|_| "SFTP unavailable")?,
            ))
        }
    }
    async fn inspect(&mut self, path: &str) -> Result<Option<SftpEntryStamp>, &'static str> {
        match self {
            Self::Remote(client) => {
                if path != "/" {
                    let parent = path.rsplit_once('/').ok_or("invalid remote parent")?.0;
                    client
                        .directory_path(if parent.is_empty() { "/" } else { parent }, false)
                        .await
                        .map_err(|_| "directory links refused")?;
                }
                Ok(client
                    .inspect_entry(path, MAX_SFTP_VERIFIED_BYTES)
                    .await
                    .map_err(|_| "remote inspection failed")?
                    .map(|s| SftpEntryStamp {
                        kind: match s.metadata.file_type {
                            RemoteFileType::File => 1,
                            RemoteFileType::Directory => 2,
                            RemoteFileType::Symlink => 3,
                            RemoteFileType::Other => 4,
                        },
                        size: s.metadata.size,
                        modified: s.metadata.modified_unix_seconds.map(u64::from),
                        permissions: s.metadata.permissions.map(|p| p & 0o7777),
                        content: s.content.map(|c| SftpVerifiedContent {
                            size: c.size,
                            sha256: c.sha256,
                        }),
                        link_target: s.link_target,
                    }))
            }
            Self::Local => local_inspect(PathBuf::from(path)).await,
        }
    }
    async fn apply(
        &mut self,
        draft: &SftpFileDraft,
        source: Option<&SftpEntryStamp>,
    ) -> Result<(), &'static str> {
        match self {
            Self::Remote(client) => match draft.action {
                SftpFileAction::Mkdir => {
                    client
                        .create_directory(
                            &draft.path,
                            draft.permissions.ok_or("missing permissions")?,
                        )
                        .await
                }
                SftpFileAction::Rename => client.rename_entry(&draft.path, &draft.target).await,
                SftpFileAction::Delete => {
                    client
                        .remove_entry(&draft.path, source.is_some_and(|s| s.kind == 2))
                        .await
                }
                SftpFileAction::Chmod => {
                    client
                        .chmod_entry(&draft.path, draft.permissions.ok_or("missing permissions")?)
                        .await
                }
                SftpFileAction::Symlink => client.symlink_entry(&draft.path, &draft.target).await,
                SftpFileAction::Inspect => return Err("inspection is read-only"),
            }
            .map_err(|_| "remote filesystem mutation unconfirmed"),
            Self::Local => {
                let draft = draft.clone();
                tokio::task::spawn_blocking(move || local_apply(&draft))
                    .await
                    .map_err(|_| "local filesystem worker unavailable")?
            }
        }
    }
    async fn close(&mut self) {
        if let Self::Remote(client) = self {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), client.close()).await;
            client.abort_channel();
        }
    }
}
fn local_parent(path: &std::path::Path) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("local path must be absolute");
    }
    for parent in path.parent().into_iter().flat_map(|p| p.ancestors()) {
        let m = std::fs::symlink_metadata(parent).map_err(|_| "local parent unavailable")?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err("local directory links refused");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if m.file_attributes() & 0x400 != 0 {
                return Err("local reparse directory refused");
            }
        }
    }
    Ok(())
}
async fn local_inspect(path: PathBuf) -> Result<Option<SftpEntryStamp>, &'static str> {
    let cloned = path.clone();
    let mut stamp = tokio::task::spawn_blocking(move || {
        local_parent(&cloned)?;
        let m = match std::fs::symlink_metadata(&cloned) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("local metadata unavailable"),
        };
        let mut link = m.file_type().is_symlink();
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            link |= m.file_attributes() & 0x400 != 0;
        }
        let permissions = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                Some(m.permissions().mode() & 0o7777)
            }
            #[cfg(not(unix))]
            {
                None
            }
        };
        let link_target = if link {
            Some(
                std::fs::read_link(&cloned)
                    .map_err(|_| "link target unavailable")?
                    .to_str()
                    .ok_or("link target is not UTF-8")?
                    .to_owned(),
            )
        } else {
            None
        };
        Ok(Some(SftpEntryStamp {
            kind: if link {
                3
            } else if m.is_file() {
                1
            } else if m.is_dir() {
                2
            } else {
                4
            },
            size: Some(m.len()),
            modified: m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| t.as_secs()),
            permissions,
            content: None,
            link_target,
        }))
    })
    .await
    .map_err(|_| "local metadata worker unavailable")??;
    if let Some(s) = &mut stamp
        && s.kind == 1
    {
        s.content = Some(
            super::super::verified::LocalSource::open(&path)
                .await?
                .digest()
                .await?,
        );
    }
    Ok(stamp)
}
fn local_apply(draft: &SftpFileDraft) -> Result<(), &'static str> {
    let path = std::path::Path::new(&draft.path);
    local_parent(path)?;
    match draft.action {
        SftpFileAction::Mkdir => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(draft.permissions.ok_or("missing permissions")?);
            }
            builder
                .create(path)
                .map_err(|_| "local mkdir unconfirmed")?;
        }
        SftpFileAction::Rename => {
            let target = std::path::Path::new(&draft.target);
            local_parent(target)?;
            if std::fs::symlink_metadata(target).is_ok() {
                return Err("rename destination exists");
            }
            rename_no_replace(path, target)?;
        }
        SftpFileAction::Delete => {
            let m = std::fs::symlink_metadata(path).map_err(|_| "local source unavailable")?;
            if m.is_dir() {
                std::fs::remove_dir(path)
                    .map_err(|_| "directory is not empty or removal unconfirmed")?;
            } else {
                std::fs::remove_file(path).map_err(|_| "local removal unconfirmed")?;
            }
        }
        SftpFileAction::Chmod => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let m = std::fs::symlink_metadata(path).map_err(|_| "local source unavailable")?;
                if m.file_type().is_symlink() {
                    return Err("chmod refuses links");
                }
                std::fs::set_permissions(
                    path,
                    std::fs::Permissions::from_mode(
                        draft.permissions.ok_or("missing permissions")?,
                    ),
                )
                .map_err(|_| "local chmod unconfirmed")?;
            }
            #[cfg(not(unix))]
            return Err("local POSIX chmod unsupported on this platform");
        }
        SftpFileAction::Symlink => {
            let target = std::path::Path::new(&draft.target);
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, path).map_err(|_| "local symlink unconfirmed")?;
            #[cfg(windows)]
            {
                if std::fs::metadata(target)
                    .map_err(|_| "symlink target unavailable")?
                    .is_dir()
                {
                    std::os::windows::fs::symlink_dir(target, path)
                        .map_err(|_| "local symlink unconfirmed")?;
                } else {
                    std::os::windows::fs::symlink_file(target, path)
                        .map_err(|_| "local symlink unconfirmed")?;
                }
            }
        }
        SftpFileAction::Inspect => return Err("inspection is read-only"),
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| "parent directory sync unconfirmed")?;
        if draft.action == SftpFileAction::Rename
            && let Some(target_parent) = std::path::Path::new(&draft.target).parent()
            && target_parent != parent
        {
            std::fs::File::open(target_parent)
                .and_then(|f| f.sync_all())
                .map_err(|_| "rename destination directory sync unconfirmed")?;
        }
    }
    Ok(())
}
#[allow(unsafe_code)]
fn rename_no_replace(
    source: &std::path::Path,
    target: &std::path::Path,
) -> Result<(), &'static str> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| "invalid rename source")?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| "invalid rename destination")?;
        // SAFETY: Both CString buffers are live, NUL terminated, and borrowed only for this call.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        // SAFETY: The same owned CString lifetimes cover this synchronous native call.
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        if result != 0 {
            return Err("exclusive local rename refused or unsupported");
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: Live, NUL-terminated UTF-16 buffers; zero flags exclude replacement/copy fallback.
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                0,
            )
        } == 0
        {
            return Err("exclusive local rename refused");
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (source, target);
        Err("exclusive rename unsupported on this platform")
    }
}
