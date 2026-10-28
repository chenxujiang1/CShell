//! Bounded, single-use directory cursors. Expiry closes native handles.
use super::*;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const CURSOR_TTL: Duration = Duration::from_secs(120);
const MAX_CURSORS: usize = 16;
#[derive(Clone, Default)]
pub(super) struct Browsers {
    cursors: Arc<tokio::sync::Mutex<BTreeMap<Vec<u8>, Slot>>>,
}
impl std::fmt::Debug for Browsers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browsers").finish_non_exhaustive()
    }
}
#[derive(Clone)]
struct Slot {
    session: SessionId,
    state: Arc<tokio::sync::Mutex<Option<Cursor>>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}
struct Cursor {
    session: SessionId,
    revision: u64,
    directory: String,
    expires: Instant,
    source: Source,
}
enum Source {
    Remote {
        reader: cshell_sftp::DirectoryCursor,
        session: Arc<SshSession>,
    },
    Local(Option<LocalCursor>),
}
struct LocalCursor {
    reader: std::fs::ReadDir,
    path: PathBuf,
    identity: std::fs::Metadata,
    pending: VecDeque<SftpDirectoryEntry>,
    eof: bool,
}
fn regular_directory(path: &std::path::Path) -> Result<std::fs::Metadata, SftpStatus> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(SftpStatus::InvalidRequest);
    }
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor).map_err(|_| SftpStatus::Failed)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(SftpStatus::Failed);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(SftpStatus::Failed);
            }
        }
    }
    std::fs::symlink_metadata(path).map_err(|_| SftpStatus::Failed)
}
fn same_directory(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.dev() != right.dev() || left.ino() != right.ino() {
            return false;
        }
    }
    left.is_dir()
        && right.is_dir()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}
impl LocalCursor {
    fn open(path: PathBuf) -> Result<Self, SftpStatus> {
        let identity = regular_directory(&path)?;
        let reader = std::fs::read_dir(&path).map_err(|_| SftpStatus::Failed)?;
        Ok(Self {
            reader,
            path,
            identity,
            pending: VecDeque::new(),
            eof: false,
        })
    }
    fn page(&mut self, limit: usize) -> Result<(Vec<SftpDirectoryEntry>, bool), SftpStatus> {
        if !same_directory(&regular_directory(&self.path)?, &self.identity) {
            return Err(SftpStatus::Conflict);
        }
        let mut entries = Vec::with_capacity(limit);
        loop {
            while entries.len() < limit {
                match self.pending.pop_front() {
                    Some(entry) => entries.push(entry),
                    None => break,
                }
            }
            if !self.pending.is_empty() || self.eof {
                break;
            }
            let Some(entry) = self.reader.next() else {
                self.eof = true;
                continue;
            };
            let entry = entry.map_err(|_| SftpStatus::Failed)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| SftpStatus::Failed)?;
            let path = entry.path().to_str().ok_or(SftpStatus::Failed)?.to_owned();
            if !valid_path(&name)
                || !valid_path(&path)
                || name.chars().any(char::is_control)
                || path.chars().any(char::is_control)
            {
                return Err(SftpStatus::Failed);
            }
            let meta = std::fs::symlink_metadata(entry.path()).map_err(|_| SftpStatus::Failed)?;
            let mut kind = if meta.file_type().is_symlink() {
                3
            } else if meta.is_dir() {
                2
            } else if meta.is_file() {
                1
            } else {
                4
            };
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    kind = 3;
                }
            }
            self.pending.push_back(SftpDirectoryEntry {
                name,
                path,
                kind,
                size: meta.is_file().then_some(meta.len()),
                modified_unix_seconds: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|t| u32::try_from(t.as_secs()).ok()),
                ..Default::default()
            });
        }
        if !same_directory(&regular_directory(&self.path)?, &self.identity) {
            return Err(SftpStatus::Conflict);
        }
        Ok((entries, !self.pending.is_empty() || !self.eof))
    }
}
impl Cursor {
    async fn close(&mut self) {
        match &mut self.source {
            Source::Remote { reader, .. } => {
                let _ = tokio::time::timeout(Duration::from_secs(2), reader.close()).await;
            }
            Source::Local(reader) => {
                reader.take();
            }
        }
    }
    async fn page(&mut self, limit: usize) -> Result<(Vec<SftpDirectoryEntry>, bool), SftpStatus> {
        match &mut self.source {
            Source::Remote { reader, session } => {
                if !session.running() {
                    return Err(SftpStatus::Unavailable);
                }
                let page = reader
                    .next_page(limit)
                    .await
                    .map_err(|_| SftpStatus::Failed)?;
                let entries = page
                    .entries
                    .into_iter()
                    .map(|e| SftpDirectoryEntry {
                        name: e.name,
                        path: e.path,
                        kind: match e.metadata.file_type {
                            RemoteFileType::File => 1,
                            RemoteFileType::Directory => 2,
                            RemoteFileType::Symlink => 3,
                            RemoteFileType::Other => 4,
                        },
                        size: e.metadata.size,
                        modified_unix_seconds: e.metadata.modified_unix_seconds,
                        permissions: e.metadata.permissions,
                        uid: e.metadata.uid,
                        gid: e.metadata.gid,
                    })
                    .collect();
                Ok((entries, page.truncated))
            }
            Source::Local(reader) => {
                let mut local = reader.take().ok_or(SftpStatus::Unavailable)?;
                let (local, result) = tokio::task::spawn_blocking(move || {
                    let page = local.page(limit);
                    (local, page)
                })
                .await
                .map_err(|_| SftpStatus::Failed)?;
                *reader = Some(local);
                result
            }
        }
    }
}
impl SftpIpcService {
    pub(super) async fn browse_managed(
        &self,
        request: SftpRequest,
        session: SessionId,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        self.browse(request, session, SftpOperation::BrowseManaged, sessions)
            .await
    }
    pub(super) async fn browse(
        &self,
        request: SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        match self
            .browse_inner(request, session, operation, sessions)
            .await
        {
            Ok(reply) => reply,
            Err(status) => SftpResponse::with_status(
                status,
                "Directory cursor unavailable, changed, expired or invalid; reload directory",
            ),
        }
    }
    async fn browse_inner(
        &self,
        request: SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> Result<SftpResponse, SftpStatus> {
        let request_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let managed = if operation == SftpOperation::BrowseManaged {
            if request.document_json.len() > 16 * 1024 {
                return Err(SftpStatus::InvalidRequest);
            }
            Some(
                serde_json::from_slice::<cshell_application::SftpBrowseOptions>(
                    &request.document_json,
                )
                .map_err(|_| SftpStatus::InvalidRequest)?,
            )
        } else {
            None
        };
        let begin = matches!(
            operation,
            SftpOperation::RemoteBrowse | SftpOperation::LocalBrowse | SftpOperation::BrowseManaged
        );
        let local =
            operation == SftpOperation::LocalBrowse || managed.as_ref().is_some_and(|m| m.local);
        let canonical = SftpRequest {
            operation: request.operation,
            session_id: request.session_id.clone(),
            remote_path: if operation == SftpOperation::RemoteBrowse {
                request.remote_path.clone()
            } else {
                String::new()
            },
            local_path: if operation == SftpOperation::LocalBrowse {
                request.local_path.clone()
            } else {
                String::new()
            },
            max_entries: if operation != SftpOperation::BrowseClose {
                request.max_entries
            } else {
                0
            },
            transfer_id: if !begin {
                request.transfer_id.clone()
            } else {
                Vec::new()
            },
            expected_revision: if !begin { request.expected_revision } else { 0 },
            document_json: if managed.is_some() {
                request.document_json.clone()
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        if canonical != request
            || session.as_uuid().is_nil()
            || operation != SftpOperation::BrowseClose
                && !(1..=MAX_SFTP_DIRECTORY_ENTRIES).contains(&request.max_entries)
            || !begin && (request.transfer_id.len() != 16 || request.expected_revision == 0)
        {
            return Err(SftpStatus::InvalidRequest);
        }
        let (id, slot, opened) = if begin {
            let id = SessionId::new().as_uuid().as_bytes().to_vec();
            let (shutdown, mut shutdown_receiver) = tokio::sync::watch::channel(false);
            let slot = Slot {
                session,
                state: Arc::new(tokio::sync::Mutex::new(None)),
                shutdown,
            };
            let expires = Instant::now() + CURSOR_TTL;
            {
                let mut pool = self.browsers.cursors.lock().await;
                if pool.len() >= MAX_CURSORS
                    || pool.values().filter(|s| s.session == session).count() >= 2
                {
                    return Err(SftpStatus::CapacityReached);
                }
                // Reserve before native I/O. Interrupted opens keep their bounded reservation until expiry.
                pool.insert(id.clone(), slot.clone());
            }
            let pool = self.browsers.cursors.clone();
            let expired_id = id.clone();
            let expired_slot = slot.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(expires)) => {},
                    _ = shutdown_receiver.changed() => return,
                }
                let mut state = expired_slot.state.lock().await;
                if let Some(mut cursor) = state.take() {
                    cursor.close().await;
                }
                pool.lock().await.remove(&expired_id);
            });
            let opened = tokio::time::timeout_at(request_deadline, async {
                let (directory, source) = if local {
                    let local_path = managed
                        .as_ref()
                        .map(|m| m.path.as_str())
                        .unwrap_or(&request.local_path);
                    let path = if local_path.is_empty() {
                        #[cfg(windows)]
                        let home = std::env::var_os("USERPROFILE");
                        #[cfg(not(windows))]
                        let home = std::env::var_os("HOME");
                        PathBuf::from(home.ok_or(SftpStatus::Unavailable)?)
                    } else {
                        PathBuf::from(local_path)
                    };
                    let path = if managed
                        .as_ref()
                        .is_some_and(|m| m.links == cshell_application::SftpLinkPolicy::Follow)
                    {
                        tokio::fs::canonicalize(path)
                            .await
                            .map_err(|_| SftpStatus::Failed)?
                    } else {
                        path
                    };
                    let directory = path.to_str().ok_or(SftpStatus::InvalidRequest)?.to_owned();
                    if !valid_path(&directory) || directory.chars().any(char::is_control) {
                        return Err(SftpStatus::InvalidRequest);
                    }
                    let reader = tokio::task::spawn_blocking(move || LocalCursor::open(path))
                        .await
                        .map_err(|_| SftpStatus::Failed)??;
                    (directory, Source::Local(Some(reader)))
                } else {
                    let directory = managed
                        .as_ref()
                        .map(|m| m.path.clone())
                        .unwrap_or_else(|| request.remote_path.clone());
                    if !valid_path(&directory)
                        || !directory.starts_with('/')
                        || directory.contains('\\')
                        || directory.chars().any(char::is_control)
                        || directory.split('/').any(|p| p == "." || p == "..")
                        || directory.ends_with("//")
                        || directory
                            .trim_end_matches('/')
                            .split('/')
                            .skip(1)
                            .any(str::is_empty)
                    {
                        return Err(SftpStatus::InvalidRequest);
                    }
                    let live = sessions.get_sftp(session).map_err(|_| SftpStatus::Unavailable)?;
                    if !live.running() {
                        return Err(SftpStatus::Unavailable);
                    }
                    let client = live.open_sftp().await.map_err(|_| SftpStatus::Failed)?;
                    let directory = if managed
                        .as_ref()
                        .is_some_and(|m| m.links == cshell_application::SftpLinkPolicy::Follow)
                    {
                        client
                            .resolve_path(&directory)
                            .await
                            .map_err(|_| SftpStatus::Failed)?
                    } else {
                        directory
                    };
                    let reader = client
                        .directory_cursor(directory.clone())
                        .await
                        .map_err(|_| SftpStatus::Failed)?;
                    (
                        directory,
                        Source::Remote {
                            reader,
                            session: live,
                        },
                    )
                };
                Ok(Cursor {
                    session,
                    revision: 0,
                    directory,
                    expires,
                    source,
                })
            })
            .await
            .unwrap_or(Err(SftpStatus::Failed));
            let cursor = match opened {
                Ok(cursor) => cursor,
                Err(error) => {
                    let _ = slot.shutdown.send(true);
                    self.browsers.cursors.lock().await.remove(&id);
                    return Err(error);
                }
            };
            (id, slot, Some(cursor))
        } else {
            let id = request.transfer_id.clone();
            let slot = self
                .browsers
                .cursors
                .lock()
                .await
                .get(&id)
                .cloned()
                .ok_or(SftpStatus::Conflict)?;
            if slot.session != session {
                return Err(SftpStatus::Conflict);
            }
            (id, slot, None)
        };
        // Native I/O is serialized per cursor, never under the global cache lock.
        let mut state = tokio::time::timeout_at(request_deadline, slot.state.lock())
            .await
            .map_err(|_| SftpStatus::Failed)?;
        if !begin
            && state
                .as_ref()
                .is_none_or(|c| c.session != session || c.revision != request.expected_revision)
        {
            return Err(SftpStatus::Conflict);
        }
        let mut cursor = opened
            .or_else(|| state.take())
            .ok_or(SftpStatus::Conflict)?;
        let is_local = matches!(&cursor.source, Source::Local(_));
        let still_bound = match &cursor.source {
            Source::Remote { session: bound, .. } => sessions
                .get_sftp(session)
                .is_ok_and(|live| Arc::ptr_eq(&live, bound) && live.running()),
            Source::Local(_) => true,
        };
        if cursor.expires <= Instant::now() || !still_bound {
            let _ = slot.shutdown.send(true);
            cursor.close().await;
            self.browsers.cursors.lock().await.remove(&id);
            return Err(SftpStatus::Conflict);
        }
        cursor.revision = cursor.revision.checked_add(1).ok_or(SftpStatus::Conflict)?;
        let result = if operation == SftpOperation::BrowseClose {
            Ok((Vec::new(), false))
        } else {
            let deadline = request_deadline.min(tokio::time::Instant::from_std(cursor.expires));
            tokio::time::timeout_at(deadline, cursor.page(request.max_entries as usize))
                .await
                .unwrap_or(Err(SftpStatus::Failed))
        };
        let (entries, more) = match result {
            Ok(page) if cursor.expires > Instant::now() => page,
            Ok(_) => {
                let _ = slot.shutdown.send(true);
                cursor.close().await;
                self.browsers.cursors.lock().await.remove(&id);
                return Err(SftpStatus::Conflict);
            }
            Err(error) => {
                let _ = slot.shutdown.send(true);
                cursor.close().await;
                self.browsers.cursors.lock().await.remove(&id);
                return Err(error);
            }
        };
        let reply = SftpResponse {
            entries,
            truncated: more,
            browse_id: id.clone(),
            browse_revision: cursor.revision,
            browse_directory: cursor.directory.clone(),
            browse_local: is_local,
            ..Default::default()
        };
        if more {
            *state = Some(cursor);
        } else {
            let _ = slot.shutdown.send(true);
            cursor.close().await;
            self.browsers.cursors.lock().await.remove(&id);
        }
        Ok(reply)
    }
}

impl SftpIpcService {
    pub(crate) async fn close_session_cursors(&self,session:SessionId) {
        let mut cursors=self.browsers.cursors.lock().await;
        let ids:Vec<_>=cursors.iter().filter(|(_,slot)|slot.session==session).map(|(id,_)|id.clone()).collect();
        for id in ids {if let Some(slot)=cursors.remove(&id) {let _=slot.shutdown.send(true);}}
    }
}
