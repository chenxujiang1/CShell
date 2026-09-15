//! Bounded SFTP filesystem and transfer primitives.

use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::{Config, RawSftpSession};
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const SFTP_MAX_PACKET_BYTES: u32 = 256 * 1024;
pub const SFTP_TRANSFER_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_DIRECTORY_PAGE_ENTRIES: usize = 4_096;
const SFTP_REQUEST_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Debug, Default)]
pub struct TransferControl {
    cancelled: Arc<AtomicBool>,
}

impl TransferControl {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferProgress {
    pub bytes_transferred: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferOutcome {
    pub bytes_transferred: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteFileType {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymlinkPolicy {
    Follow,
    NoFollow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteMetadata {
    pub file_type: RemoteFileType,
    pub size: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub permissions: Option<u32>,
    pub accessed_unix_seconds: Option<u32>,
    pub modified_unix_seconds: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteDirectoryEntry {
    pub name: String,
    pub path: String,
    pub metadata: RemoteMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryPage {
    pub entries: Vec<RemoteDirectoryEntry>,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum SftpError {
    #[error(transparent)]
    Protocol(#[from] russh_sftp::client::error::Error),
    #[error("local I/O failed: {0}")]
    LocalIo(#[from] std::io::Error),
    #[error("directory page limit must be between 1 and {MAX_DIRECTORY_PAGE_ENTRIES}")]
    InvalidDirectoryPageLimit,
    #[error("SFTP server returned an empty read before EOF")]
    EmptyRead,
    #[error("transfer was cancelled")]
    TransferCancelled,
    #[error("transfer size mismatch: expected {expected} bytes, received {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("remote server omitted the temporary file size")]
    MissingRemoteSize,
    #[error("destination already exists: {0}")]
    DestinationExists(String),
    #[error("invalid remote destination path: {0}")]
    InvalidRemotePath(String),
    #[error("invalid local destination path: {0}")]
    InvalidLocalPath(String),
    #[error("transfer offset overflowed u64")]
    OffsetOverflow,
    #[error("SFTP server advertised an unusable packet or transfer limit")]
    InvalidServerLimits,
    #[error("{cause}; additionally could not clean temporary file {path}: {reason}")]
    TemporaryCleanupFailed {
        path: String,
        cause: String,
        reason: String,
    },
    #[error("transfer committed, but temporary file cleanup failed for {path}: {reason}")]
    CommittedCleanupFailed { path: String, reason: String },
}

pub struct SftpClient {
    session: Arc<RawSftpSession>,
    limits: Limits,
}

impl std::fmt::Debug for SftpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SftpClient").finish_non_exhaustive()
    }
}

impl SftpClient {
    pub async fn connect<S>(stream: S) -> Result<Self, SftpError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = Config {
            max_packet_len: SFTP_MAX_PACKET_BYTES,
            max_concurrent_reads: 1,
            max_concurrent_writes: 1,
            max_write_packet_len: SFTP_TRANSFER_CHUNK_BYTES as u32,
            request_timeout_secs: SFTP_REQUEST_TIMEOUT_SECS,
        };
        let mut session = RawSftpSession::new_with_config(stream, config);
        let version = session.init().await?;
        let mut limits = Limits::default();
        if version
            .extensions
            .get(russh_sftp::extensions::LIMITS)
            .is_some_and(|version| version == "1")
        {
            limits = Limits::from(session.limits().await?);
            session.set_limits(limits);
        }
        Ok(Self {
            session: Arc::new(session),
            limits,
        })
    }

    pub async fn close(&self) -> Result<(), SftpError> {
        self.session.close_session()?;
        Ok(())
    }

    pub async fn metadata(
        &self,
        path: impl Into<String>,
        symlink_policy: SymlinkPolicy,
    ) -> Result<RemoteMetadata, SftpError> {
        let attributes = match symlink_policy {
            SymlinkPolicy::Follow => self.session.stat(path).await?.attrs,
            SymlinkPolicy::NoFollow => self.session.lstat(path).await?.attrs,
        };
        Ok(map_metadata(&attributes))
    }

    pub async fn list_directory(
        &self,
        path: impl Into<String>,
        max_entries: usize,
    ) -> Result<DirectoryPage, SftpError> {
        if !(1..=MAX_DIRECTORY_PAGE_ENTRIES).contains(&max_entries) {
            return Err(SftpError::InvalidDirectoryPageLimit);
        }
        let path = path.into();
        let handle = self.session.opendir(path.clone()).await?.handle;
        let operation = async {
            let mut entries = Vec::with_capacity(max_entries);
            loop {
                match self.session.readdir(handle.clone()).await {
                    Ok(names) => {
                        for file in names.files {
                            if file.filename == "." || file.filename == ".." {
                                continue;
                            }
                            if entries.len() == max_entries {
                                return Ok(DirectoryPage {
                                    entries,
                                    truncated: true,
                                });
                            }
                            entries.push(RemoteDirectoryEntry {
                                path: join_remote_path(&path, &file.filename),
                                name: file.filename,
                                metadata: map_metadata(&file.attrs),
                            });
                        }
                    }
                    Err(error) if is_status(&error, StatusCode::Eof) => {
                        return Ok(DirectoryPage {
                            entries,
                            truncated: false,
                        });
                    }
                    Err(error) => return Err(SftpError::Protocol(error)),
                }
            }
        }
        .await;
        let close = self.session.close(handle).await;
        match operation {
            Err(error) => Err(error),
            Ok(page) => {
                close?;
                Ok(page)
            }
        }
    }

    pub async fn download<W, P>(
        &self,
        remote_path: impl Into<String>,
        writer: &mut W,
        control: &TransferControl,
        mut progress: P,
    ) -> Result<TransferOutcome, SftpError>
    where
        W: AsyncWrite + Unpin + Send,
        P: FnMut(TransferProgress),
    {
        let remote_path = remote_path.into();
        let total = self.session.stat(remote_path.clone()).await?.attrs.size;
        let handle = self
            .session
            .open(remote_path, OpenFlags::READ, FileAttributes::default())
            .await?
            .handle;
        progress(TransferProgress {
            bytes_transferred: 0,
            total_bytes: total,
        });
        let operation = async {
            let mut offset = 0_u64;
            let read_chunk_bytes = self.read_chunk_bytes()?;
            loop {
                if control.is_cancelled() {
                    return Err(SftpError::TransferCancelled);
                }
                match self
                    .session
                    .read(handle.clone(), offset, read_chunk_bytes)
                    .await
                {
                    Ok(data) => {
                        if data.data.is_empty() {
                            return Err(SftpError::EmptyRead);
                        }
                        if control.is_cancelled() {
                            return Err(SftpError::TransferCancelled);
                        }
                        writer.write_all(&data.data).await?;
                        offset = offset
                            .checked_add(data.data.len() as u64)
                            .ok_or(SftpError::OffsetOverflow)?;
                        progress(TransferProgress {
                            bytes_transferred: offset,
                            total_bytes: total,
                        });
                    }
                    Err(error) if is_status(&error, StatusCode::Eof) => break,
                    Err(error) => return Err(SftpError::Protocol(error)),
                }
            }
            writer.flush().await?;
            if let Some(expected) = total
                && offset != expected
            {
                return Err(SftpError::SizeMismatch {
                    expected,
                    actual: offset,
                });
            }
            Ok(TransferOutcome {
                bytes_transferred: offset,
                total_bytes: total,
            })
        }
        .await;
        let close = self.session.close(handle).await;
        match operation {
            Err(error) => Err(error),
            Ok(outcome) => {
                close?;
                Ok(outcome)
            }
        }
    }

    pub async fn download_to_path_atomic<P>(
        &self,
        remote_path: impl Into<String>,
        destination: impl AsRef<Path>,
        control: &TransferControl,
        progress: P,
    ) -> Result<TransferOutcome, SftpError>
    where
        P: FnMut(TransferProgress),
    {
        let destination = destination.as_ref();
        if tokio::fs::try_exists(destination).await? {
            return Err(SftpError::DestinationExists(
                destination.display().to_string(),
            ));
        }
        let temporary = local_temporary_path(destination)?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        let result = self
            .download(remote_path, &mut file, control, progress)
            .await;
        if let Err(error) = result {
            drop(file);
            return fail_after_local_temporary(error, &temporary).await;
        }
        if let Err(error) = file.sync_all().await {
            drop(file);
            return fail_after_local_temporary(SftpError::LocalIo(error), &temporary).await;
        }
        drop(file);
        if let Err(error) = tokio::fs::hard_link(&temporary, destination).await {
            return fail_after_local_temporary(SftpError::LocalIo(error), &temporary).await;
        }
        let outcome = result?;
        match tokio::fs::remove_file(&temporary).await {
            Ok(()) => Ok(outcome),
            Err(error) => Err(SftpError::CommittedCleanupFailed {
                path: temporary.display().to_string(),
                reason: error.to_string(),
            }),
        }
    }

    pub async fn upload_atomic<R, P>(
        &self,
        reader: &mut R,
        remote_destination: impl Into<String>,
        expected_size: Option<u64>,
        control: &TransferControl,
        mut progress: P,
    ) -> Result<TransferOutcome, SftpError>
    where
        R: AsyncRead + Unpin + Send,
        P: FnMut(TransferProgress),
    {
        let destination = remote_destination.into();
        let temporary = remote_temporary_path(&destination)?;
        match self.session.stat(destination.clone()).await {
            Ok(_) => return Err(SftpError::DestinationExists(destination)),
            Err(error) if is_status(&error, StatusCode::NoSuchFile) => {}
            Err(error) => return Err(SftpError::Protocol(error)),
        }
        let handle = self
            .session
            .open(
                temporary.clone(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                FileAttributes::default(),
            )
            .await?
            .handle;
        progress(TransferProgress {
            bytes_transferred: 0,
            total_bytes: expected_size,
        });
        let operation = async {
            let write_chunk_bytes = self.write_chunk_bytes(&handle)?;
            let mut buffer = vec![0_u8; write_chunk_bytes];
            let mut offset = 0_u64;
            loop {
                if control.is_cancelled() {
                    return Err(SftpError::TransferCancelled);
                }
                let read = reader.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                if control.is_cancelled() {
                    return Err(SftpError::TransferCancelled);
                }
                self.session
                    .write(handle.clone(), offset, buffer[..read].to_vec())
                    .await?;
                offset = offset
                    .checked_add(read as u64)
                    .ok_or(SftpError::OffsetOverflow)?;
                if let Some(expected) = expected_size
                    && offset > expected
                {
                    return Err(SftpError::SizeMismatch {
                        expected,
                        actual: offset,
                    });
                }
                progress(TransferProgress {
                    bytes_transferred: offset,
                    total_bytes: expected_size,
                });
            }
            if let Some(expected) = expected_size
                && offset != expected
            {
                return Err(SftpError::SizeMismatch {
                    expected,
                    actual: offset,
                });
            }
            Ok(offset)
        }
        .await;
        let close = self.session.close(handle).await;
        if let Err(error) = operation {
            return self.fail_after_remote_temporary(error, &temporary).await;
        }
        if let Err(error) = close {
            return self
                .fail_after_remote_temporary(SftpError::Protocol(error), &temporary)
                .await;
        }
        let transferred = operation?;
        let remote_size = match self.session.stat(temporary.clone()).await {
            Ok(attributes) => attributes.attrs.size,
            Err(error) => {
                return self
                    .fail_after_remote_temporary(SftpError::Protocol(error), &temporary)
                    .await;
            }
        };
        if remote_size != Some(transferred) {
            let error = match remote_size {
                Some(actual) => SftpError::SizeMismatch {
                    expected: transferred,
                    actual,
                },
                None => SftpError::MissingRemoteSize,
            };
            return self.fail_after_remote_temporary(error, &temporary).await;
        }
        match self.session.stat(destination.clone()).await {
            Ok(_) => {
                return self
                    .fail_after_remote_temporary(
                        SftpError::DestinationExists(destination),
                        &temporary,
                    )
                    .await;
            }
            Err(error) if is_status(&error, StatusCode::NoSuchFile) => {}
            Err(error) => {
                return self
                    .fail_after_remote_temporary(SftpError::Protocol(error), &temporary)
                    .await;
            }
        }
        if let Err(error) = self.session.rename(temporary.clone(), destination).await {
            return self
                .fail_after_remote_temporary(SftpError::Protocol(error), &temporary)
                .await;
        }
        Ok(TransferOutcome {
            bytes_transferred: transferred,
            total_bytes: expected_size,
        })
    }

    pub async fn upload_from_path_atomic<P>(
        &self,
        local_source: impl AsRef<Path>,
        remote_destination: impl Into<String>,
        control: &TransferControl,
        progress: P,
    ) -> Result<TransferOutcome, SftpError>
    where
        P: FnMut(TransferProgress),
    {
        let local_source = local_source.as_ref();
        let metadata = tokio::fs::metadata(local_source).await?;
        if !metadata.is_file() {
            return Err(SftpError::InvalidLocalPath(
                local_source.display().to_string(),
            ));
        }
        let mut file = tokio::fs::File::open(local_source).await?;
        self.upload_atomic(
            &mut file,
            remote_destination,
            Some(metadata.len()),
            control,
            progress,
        )
        .await
    }

    async fn fail_after_remote_temporary<T>(
        &self,
        error: SftpError,
        temporary: &str,
    ) -> Result<T, SftpError> {
        match self.session.remove(temporary.to_owned()).await {
            Ok(_) => Err(error),
            Err(cleanup) if is_status(&cleanup, StatusCode::NoSuchFile) => Err(error),
            Err(cleanup) => Err(SftpError::TemporaryCleanupFailed {
                path: temporary.to_owned(),
                cause: error.to_string(),
                reason: cleanup.to_string(),
            }),
        }
    }

    fn read_chunk_bytes(&self) -> Result<u32, SftpError> {
        // DATA packet: length (4), type (1), id (4), data length (4).
        let packet_payload = self
            .limits
            .packet_len
            .unwrap_or(u64::from(SFTP_MAX_PACKET_BYTES))
            .min(u64::from(SFTP_MAX_PACKET_BYTES))
            .checked_sub(13)
            .ok_or(SftpError::InvalidServerLimits)?;
        let chunk = packet_payload
            .min(self.limits.read_len.unwrap_or(u64::MAX))
            .min(SFTP_TRANSFER_CHUNK_BYTES as u64);
        u32::try_from(chunk)
            .ok()
            .filter(|chunk| *chunk > 0)
            .ok_or(SftpError::InvalidServerLimits)
    }

    fn write_chunk_bytes(&self, handle: &str) -> Result<usize, SftpError> {
        // WRITE packet excluding handle bytes: length (4), type (1), id (4),
        // handle length (4), offset (8), data length (4).
        let overhead = 25_u64
            .checked_add(handle.len() as u64)
            .ok_or(SftpError::InvalidServerLimits)?;
        let packet_payload = self
            .limits
            .packet_len
            .unwrap_or(u64::from(SFTP_MAX_PACKET_BYTES))
            .min(u64::from(SFTP_MAX_PACKET_BYTES))
            .checked_sub(overhead)
            .ok_or(SftpError::InvalidServerLimits)?;
        usize::try_from(
            packet_payload
                .min(self.limits.write_len.unwrap_or(u64::MAX))
                .min(SFTP_TRANSFER_CHUNK_BYTES as u64),
        )
        .ok()
        .filter(|chunk| *chunk > 0)
        .ok_or(SftpError::InvalidServerLimits)
    }
}

fn map_metadata(attributes: &FileAttributes) -> RemoteMetadata {
    let file_type = match attributes.file_type() {
        FileType::Dir => RemoteFileType::Directory,
        FileType::File => RemoteFileType::File,
        FileType::Symlink => RemoteFileType::Symlink,
        FileType::Other => RemoteFileType::Other,
    };
    RemoteMetadata {
        file_type,
        size: attributes.size,
        uid: attributes.uid,
        gid: attributes.gid,
        permissions: attributes.permissions,
        accessed_unix_seconds: attributes.atime,
        modified_unix_seconds: attributes.mtime,
    }
}

fn is_status(error: &russh_sftp::client::error::Error, expected: StatusCode) -> bool {
    matches!(error, russh_sftp::client::error::Error::Status(status) if status.status_code == expected)
}

fn join_remote_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else if parent.ends_with('/') {
        format!("{parent}{child}")
    } else {
        format!("{parent}/{child}")
    }
}

fn remote_temporary_path(destination: &str) -> Result<String, SftpError> {
    let (parent, name) = destination
        .rsplit_once('/')
        .map_or(("", destination), |(parent, name)| (parent, name));
    if destination.contains('\0') || name.is_empty() || name == "." || name == ".." {
        return Err(SftpError::InvalidRemotePath(destination.to_owned()));
    }
    let temporary_name = format!(".{name}.cshell-{}.tmp", uuid::Uuid::now_v7());
    Ok(if parent.is_empty() {
        temporary_name
    } else if parent == "/" {
        format!("/{temporary_name}")
    } else {
        format!("{parent}/{temporary_name}")
    })
}

async fn fail_after_local_temporary<T>(error: SftpError, temporary: &Path) -> Result<T, SftpError> {
    match tokio::fs::remove_file(temporary).await {
        Ok(()) => Err(error),
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
        Err(cleanup) => Err(SftpError::TemporaryCleanupFailed {
            path: temporary.display().to_string(),
            cause: error.to_string(),
            reason: cleanup.to_string(),
        }),
    }
}

fn local_temporary_path(destination: &Path) -> Result<PathBuf, SftpError> {
    let file_name = destination
        .file_name()
        .ok_or_else(|| SftpError::InvalidLocalPath(destination.display().to_string()))?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(format!(".cshell-{}.tmp", uuid::Uuid::now_v7()));
    Ok(destination.with_file_name(temporary_name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        MAX_DIRECTORY_PAGE_ENTRIES, RemoteFileType, SFTP_TRANSFER_CHUNK_BYTES, SftpClient,
        SftpError, SymlinkPolicy, TransferControl,
    };
    use russh_sftp::client::RawSftpSession;
    use russh_sftp::client::rawsession::Limits;
    use russh_sftp::protocol::{
        Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct TestState {
        files: HashMap<String, Vec<u8>>,
        handles: HashMap<String, String>,
        directory_read: bool,
    }

    #[derive(Clone, Debug)]
    struct TestServer {
        state: Arc<Mutex<TestState>>,
    }

    impl TestServer {
        fn status(id: u32) -> Status {
            Status {
                id,
                status_code: StatusCode::Ok,
                error_message: String::new(),
                language_tag: String::new(),
            }
        }

        fn file_attributes(bytes: &[u8]) -> FileAttributes {
            FileAttributes {
                size: Some(bytes.len() as u64),
                permissions: Some(0o100644),
                mtime: Some(1_700_000_000),
                ..FileAttributes::default()
            }
        }
    }

    impl russh_sftp::server::Handler for TestServer {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            flags: OpenFlags,
            _attrs: FileAttributes,
        ) -> Result<Handle, Self::Error> {
            let mut state = self.state.lock().unwrap();
            if flags.contains(OpenFlags::EXCLUDE) && state.files.contains_key(&filename) {
                return Err(StatusCode::Failure);
            }
            if flags.contains(OpenFlags::CREATE) {
                state.files.entry(filename.clone()).or_default();
            }
            if !state.files.contains_key(&filename) {
                return Err(StatusCode::NoSuchFile);
            }
            let handle = format!("file-{id}");
            state.handles.insert(handle.clone(), filename);
            Ok(Handle { id, handle })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            self.state.lock().unwrap().handles.remove(&handle);
            Ok(Self::status(id))
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            let state = self.state.lock().unwrap();
            let path = state.handles.get(&handle).ok_or(StatusCode::Failure)?;
            let bytes = state.files.get(path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
            if start >= bytes.len() {
                return Err(StatusCode::Eof);
            }
            let end = start.saturating_add(len as usize).min(bytes.len());
            Ok(Data {
                id,
                data: bytes[start..end].to_vec(),
            })
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<Status, Self::Error> {
            let mut state = self.state.lock().unwrap();
            let path = state
                .handles
                .get(&handle)
                .ok_or(StatusCode::Failure)?
                .clone();
            let bytes = state.files.get_mut(&path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
            let end = start.checked_add(data.len()).ok_or(StatusCode::Failure)?;
            bytes.resize(end, 0);
            bytes[start..end].copy_from_slice(&data);
            Ok(Self::status(id))
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let state = self.state.lock().unwrap();
            let bytes = state.files.get(&path).ok_or(StatusCode::NoSuchFile)?;
            Ok(Attrs {
                id,
                attrs: Self::file_attributes(bytes),
            })
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.stat(id, path).await
        }

        async fn opendir(&mut self, id: u32, _path: String) -> Result<Handle, Self::Error> {
            self.state.lock().unwrap().directory_read = false;
            Ok(Handle {
                id,
                handle: "directory".to_owned(),
            })
        }

        async fn readdir(&mut self, id: u32, _handle: String) -> Result<Name, Self::Error> {
            let mut state = self.state.lock().unwrap();
            if state.directory_read {
                return Err(StatusCode::Eof);
            }
            state.directory_read = true;
            let files = (0..20)
                .map(|index| {
                    File::new(
                        format!("entry-{index:02}"),
                        Self::file_attributes(b"content"),
                    )
                })
                .collect();
            Ok(Name { id, files })
        }

        async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
            if self.state.lock().unwrap().files.remove(&filename).is_none() {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(Self::status(id))
        }

        async fn rename(
            &mut self,
            id: u32,
            oldpath: String,
            newpath: String,
        ) -> Result<Status, Self::Error> {
            let mut state = self.state.lock().unwrap();
            if state.files.contains_key(&newpath) {
                return Err(StatusCode::Failure);
            }
            let data = state.files.remove(&oldpath).ok_or(StatusCode::NoSuchFile)?;
            state.files.insert(newpath, data);
            Ok(Self::status(id))
        }
    }

    async fn client() -> (SftpClient, Arc<Mutex<TestState>>) {
        let state = Arc::new(Mutex::new(TestState::default()));
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        russh_sftp::server::run(
            server_stream,
            TestServer {
                state: state.clone(),
            },
        )
        .await;
        (SftpClient::connect(client_stream).await.unwrap(), state)
    }

    #[tokio::test]
    async fn directory_and_metadata_results_are_project_owned_and_bounded() {
        let (client, state) = client().await;
        state
            .lock()
            .unwrap()
            .files
            .insert("/remote.bin".to_owned(), b"content".to_vec());
        let metadata = client
            .metadata("/remote.bin", SymlinkPolicy::NoFollow)
            .await
            .unwrap();
        assert_eq!(metadata.file_type, RemoteFileType::File);
        assert_eq!(metadata.size, Some(7));

        let page = client.list_directory("/", 5).await.unwrap();
        assert_eq!(page.entries.len(), 5);
        assert!(page.truncated);
        assert_eq!(page.entries[0].path, "/entry-00");
        assert!(matches!(
            client
                .list_directory("/", MAX_DIRECTORY_PAGE_ENTRIES + 1)
                .await,
            Err(SftpError::InvalidDirectoryPageLimit)
        ));
        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn upload_is_chunked_verified_and_atomically_renamed() {
        let (client, state) = client().await;
        let payload = vec![0x5a; SFTP_TRANSFER_CHUNK_BYTES * 3 + 17];
        let mut reader = payload.as_slice();
        let mut progress = Vec::new();
        let outcome = client
            .upload_atomic(
                &mut reader,
                "/release.bin",
                Some(payload.len() as u64),
                &TransferControl::default(),
                |event| progress.push(event.bytes_transferred),
            )
            .await
            .unwrap();
        assert_eq!(outcome.bytes_transferred, payload.len() as u64);
        assert_eq!(progress.first(), Some(&0));
        assert!(progress.len() >= 5);
        let state = state.lock().unwrap();
        assert_eq!(state.files.get("/release.bin"), Some(&payload));
        assert!(
            state
                .files
                .keys()
                .all(|path| !path.contains(".cshell-") || !path.ends_with(".tmp"))
        );
    }

    #[tokio::test]
    async fn transfer_chunks_honor_server_packet_and_operation_limits() {
        let (stream, _peer) = tokio::io::duplex(4_096);
        let client = SftpClient {
            session: Arc::new(RawSftpSession::new(stream)),
            limits: Limits {
                packet_len: Some(1_024),
                read_len: Some(700),
                write_len: Some(600),
                open_handles: None,
            },
        };
        assert_eq!(client.read_chunk_bytes().unwrap(), 700);
        assert_eq!(client.write_chunk_bytes("handle").unwrap(), 600);
        let impossible = SftpClient {
            session: client.session,
            limits: Limits {
                packet_len: Some(10),
                ..Limits::default()
            },
        };
        assert!(matches!(
            impossible.read_chunk_bytes(),
            Err(SftpError::InvalidServerLimits)
        ));
    }

    #[tokio::test]
    async fn upload_size_mismatch_removes_temporary_without_publishing() {
        let (client, state) = client().await;
        let payload = vec![0x44; SFTP_TRANSFER_CHUNK_BYTES + 1];
        let mut reader = payload.as_slice();
        let result = client
            .upload_atomic(
                &mut reader,
                "/incorrect.bin",
                Some(payload.len() as u64 + 1),
                &TransferControl::default(),
                |_| {},
            )
            .await;
        assert!(matches!(result, Err(SftpError::SizeMismatch { .. })));
        let state = state.lock().unwrap();
        assert!(!state.files.contains_key("/incorrect.bin"));
        assert!(
            state
                .files
                .keys()
                .all(|path| !path.contains(".cshell-") || !path.ends_with(".tmp"))
        );
    }

    #[tokio::test]
    async fn local_file_upload_uses_metadata_size_and_atomic_remote_publish() {
        let (client, state) = client().await;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let payload = vec![0x61; SFTP_TRANSFER_CHUNK_BYTES + 9];
        tokio::fs::write(&source, &payload).await.unwrap();
        let outcome = client
            .upload_from_path_atomic(
                &source,
                "/from-local.bin",
                &TransferControl::default(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.total_bytes, Some(payload.len() as u64));
        assert_eq!(
            state.lock().unwrap().files.get("/from-local.bin"),
            Some(&payload)
        );
    }

    #[tokio::test]
    async fn cancelled_upload_removes_temporary_and_preserves_destination() {
        let (client, state) = client().await;
        state
            .lock()
            .unwrap()
            .files
            .insert("/stable.bin".to_owned(), b"stable".to_vec());
        let payload = vec![0x33; SFTP_TRANSFER_CHUNK_BYTES * 2];
        let mut reader = payload.as_slice();
        let control = TransferControl::default();
        let result = client
            .upload_atomic(
                &mut reader,
                "/new.bin",
                Some(payload.len() as u64),
                &control,
                |event| {
                    if event.bytes_transferred >= SFTP_TRANSFER_CHUNK_BYTES as u64 {
                        control.cancel();
                    }
                },
            )
            .await;
        assert!(matches!(result, Err(SftpError::TransferCancelled)));
        let state = state.lock().unwrap();
        assert_eq!(state.files.get("/stable.bin"), Some(&b"stable".to_vec()));
        assert!(!state.files.contains_key("/new.bin"));
        assert!(
            state
                .files
                .keys()
                .all(|path| !path.contains(".cshell-") || !path.ends_with(".tmp"))
        );
    }

    #[tokio::test]
    async fn download_streams_chunks_and_checks_remote_size() {
        let (client, state) = client().await;
        let payload = vec![0xa5; SFTP_TRANSFER_CHUNK_BYTES * 2 + 11];
        state
            .lock()
            .unwrap()
            .files
            .insert("/archive.bin".to_owned(), payload.clone());
        let mut downloaded = Vec::new();
        let mut progress = Vec::new();
        let outcome = client
            .download(
                "/archive.bin",
                &mut downloaded,
                &TransferControl::default(),
                |event| progress.push(event.bytes_transferred),
            )
            .await
            .unwrap();
        assert_eq!(downloaded, payload);
        assert_eq!(outcome.bytes_transferred, downloaded.len() as u64);
        assert_eq!(progress.first(), Some(&0));
        assert!(progress.len() >= 4);
    }

    #[tokio::test]
    async fn atomic_local_download_publishes_only_complete_content() {
        let (client, state) = client().await;
        let payload = vec![0x7c; SFTP_TRANSFER_CHUNK_BYTES * 2 + 29];
        state
            .lock()
            .unwrap()
            .files
            .insert("/snapshot.bin".to_owned(), payload.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("snapshot.bin");
        let outcome = client
            .download_to_path_atomic(
                "/snapshot.bin",
                &destination,
                &TransferControl::default(),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.bytes_transferred, payload.len() as u64);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), payload);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cancelled_atomic_local_download_removes_partial_file() {
        let (client, state) = client().await;
        let payload = vec![0x19; SFTP_TRANSFER_CHUNK_BYTES * 3];
        state
            .lock()
            .unwrap()
            .files
            .insert("/large.bin".to_owned(), payload);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("large.bin");
        let control = TransferControl::default();
        let result = client
            .download_to_path_atomic("/large.bin", &destination, &control, |event| {
                if event.bytes_transferred >= SFTP_TRANSFER_CHUNK_BYTES as u64 {
                    control.cancel();
                }
            })
            .await;
        assert!(matches!(result, Err(SftpError::TransferCancelled)));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
