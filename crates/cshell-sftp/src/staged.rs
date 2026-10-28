//! Explicit staged-file operations; the application journals each mutation intent.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StagedUploadError {
    #[error("server lacks verified no-replace publication support")]
    Unsupported,
    #[error("remote destination or staging name already exists")]
    Exists,
    #[error("invalid staged file or path")]
    Invalid,
    #[error("SFTP transport or mutation reply is uncertain")]
    Uncertain,
    #[error("remote staged-file content changed")]
    Changed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StagedUploadContent {
    pub size: u64,
    pub sha256: [u8; 32],
}
pub struct StagedUpload {
    pub(super) client: SftpClient,
    pub(super) handle: Option<String>,
}
impl std::fmt::Debug for StagedUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedUpload")
            .field("handle_open", &self.handle.is_some())
            .finish()
    }
}
fn absent(error: &russh_sftp::client::error::Error) -> bool {
    is_status(error, StatusCode::NoSuchFile)
}
fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 4096
        && !path.ends_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
pub(super) async fn digest_content(
    client: &SftpClient,
    path: &str,
    limit: u64,
) -> Result<StagedUploadContent, StagedUploadError> {
    digest_content_with_progress(client,path,limit,None).await
}
async fn digest_content_with_progress(client:&SftpClient,path:&str,limit:u64,progress:Option<&ReadProgress>)->Result<StagedUploadContent,StagedUploadError> {
    if !valid_path(path) {
        return Err(StagedUploadError::Invalid);
    }
    let before = client
        .session
        .lstat(path)
        .await
        .map_err(|_| StagedUploadError::Uncertain)?
        .attrs;
    if before.file_type() != FileType::File || before.size.is_none_or(|size| size > limit) {
        return Err(StagedUploadError::Invalid);
    }
    let handle = client
        .session
        .open(path, OpenFlags::READ, FileAttributes::default())
        .await
        .map_err(|_| StagedUploadError::Uncertain)?
        .handle;
    let result = async {
        let attrs = client
            .session
            .fstat(handle.clone())
            .await
            .map_err(|_| StagedUploadError::Uncertain)?
            .attrs;
        if attrs.file_type() != FileType::File || attrs.size != before.size {
            return Err(StagedUploadError::Changed);
        }
        let expected = attrs.size.ok_or(StagedUploadError::Invalid)?;
        let mut hash = Sha256::new();
        let mut offset = 0;
        if let Some(progress)=progress {progress(0,expected);}
        while offset < expected {
            let count = (expected - offset).min(SFTP_TRANSFER_BATCH_BYTES as u64) as usize;
            let bytes = client
                .read_range(&handle, offset, count)
                .await
                .map_err(|_| StagedUploadError::Changed)?;
            offset += bytes.len() as u64;
            hash.update(&bytes);
            if let Some(progress)=progress {progress(offset,expected);}
        }
        match client.session.read(handle.clone(), offset, 1).await {
            Err(error) if is_status(&error, StatusCode::Eof) => {}
            _ => return Err(StagedUploadError::Changed),
        }
        let after = client
            .session
            .fstat(handle.clone())
            .await
            .map_err(|_| StagedUploadError::Uncertain)?
            .attrs;
        if offset != expected || after.size != Some(expected) {
            return Err(StagedUploadError::Changed);
        }
        Ok(StagedUploadContent {
            size: offset,
            sha256: hash.finalize().into(),
        })
    }
    .await;
    let closed = client
        .session
        .close(handle)
        .await
        .map_err(|_| StagedUploadError::Uncertain);
    match (result, closed) {
        (Ok(content), Ok(_)) => Ok(content),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
    }
}
impl SftpClient {
    #[must_use]
    pub fn staged_upload(self) -> StagedUpload {
        StagedUpload {
            client: self,
            handle: None,
        }
    }
}
impl Drop for StagedUpload {
    fn drop(&mut self) {
        self.client.abort_channel();
        // Only close the handle. Never infer ownership or delete a file on drop.
        if let Some(handle) = self.handle.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let session = self.client.session.clone();
            runtime.spawn(async move {
                let _closed = session.close(handle).await;
            });
        }
    }
}
impl StagedUpload {
    /// Clear a bounded namespace challenge before any user data is written.
    pub async fn reset_empty(&self) -> Result<(), StagedUploadError> {
        let handle=self.handle.as_ref().ok_or(StagedUploadError::Invalid)?;
        self.client.session.fsetstat(handle.clone(),FileAttributes{size:Some(0),..Default::default()}).await.map_err(|_|StagedUploadError::Uncertain)?;
        Ok(())
    }
    pub async fn size(&self,path:&str)->Result<Option<u64>,StagedUploadError> {
        if !valid_path(path) {return Err(StagedUploadError::Invalid);}
        match self.client.session.lstat(path).await {
            Ok(value) if value.attrs.file_type()==FileType::File=>value.attrs.size.map(Some).ok_or(StagedUploadError::Invalid),
            Ok(_)=>Err(StagedUploadError::Invalid),
            Err(error) if absent(&error)=>Ok(None),
            Err(_)=>Err(StagedUploadError::Uncertain),
        }
    }
    pub async fn check_size(&self,temporary:&str,expected:u64)->Result<(),StagedUploadError> {
        let handle=self.handle.as_ref().ok_or(StagedUploadError::Invalid)?;
        let opened=self.client.session.fstat(handle.clone()).await.map_err(|_|StagedUploadError::Uncertain)?.attrs;
        if opened.file_type()!=FileType::File || opened.size!=Some(expected) || self.size(temporary).await?!=Some(expected) {return Err(StagedUploadError::Changed);}
        Ok(())
    }
    pub async fn preflight_existing(
        &self,
        temporary: &str,
        destination: &str,
        backup: &str,
        replace: bool,
        maximum: u64,
    ) -> Result<Option<StagedUploadContent>, StagedUploadError> {
        if !self.client.verified_publish || replace && !self.client.verified_replace {
            return Err(StagedUploadError::Unsupported);
        }
        for path in [temporary, destination, backup] {
            if !valid_path(path)
                || path.rsplit_once('/').map(|p| p.0) != destination.rsplit_once('/').map(|p| p.0)
            {
                return Err(StagedUploadError::Invalid);
            }
        }
        if temporary == destination || temporary == backup || backup == destination {
            return Err(StagedUploadError::Invalid);
        }
        // Existing destination is permitted; staging and backup names must be fresh.
        self.preflight(temporary, backup).await?;
        self.probe(destination, maximum).await
    }
    async fn copy_backup(
        &self,
        destination: &str,
        backup: &str,
        expected: StagedUploadContent,
    ) -> Result<(), StagedUploadError> {
        let source = self
            .client
            .session
            .open(destination, OpenFlags::READ, FileAttributes::default())
            .await
            .map_err(|_| StagedUploadError::Uncertain)?
            .handle;
        let target = match self
            .client
            .session
            .open(
                backup,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                FileAttributes {
                    permissions: Some(0o600),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(opened) => opened.handle,
            Err(_) => {
                let _closed = self.client.session.close(source).await;
                return Err(StagedUploadError::Uncertain);
            }
        };
        let result = async {
            let read = self
                .client
                .read_chunk_bytes()
                .map_err(|_| StagedUploadError::Invalid)?;
            let write = self
                .client
                .write_chunk_bytes(&target)
                .map_err(|_| StagedUploadError::Invalid)?;
            let mut hash = Sha256::new();
            let mut offset = 0;
            while offset < expected.size {
                let n = u64::from(read).min(expected.size - offset) as u32;
                let data = self
                    .client
                    .session
                    .read(source.clone(), offset, n)
                    .await
                    .map_err(|_| StagedUploadError::Uncertain)?
                    .data;
                if data.is_empty() || data.len() > n as usize {
                    return Err(StagedUploadError::Changed);
                }
                hash.update(&data);
                for (index, chunk) in data.chunks(write).enumerate() {
                    self.client
                        .session
                        .write(
                            target.clone(),
                            offset + (index * write) as u64,
                            chunk.to_vec(),
                        )
                        .await
                        .map_err(|_| StagedUploadError::Uncertain)?;
                }
                offset += data.len() as u64;
            }
            let actual: [u8; 32] = hash.finalize().into();
            if actual != expected.sha256 {
                return Err(StagedUploadError::Changed);
            }
            self.client
                .session
                .fsync(target.clone())
                .await
                .map_err(|_| StagedUploadError::Uncertain)?;
            Ok(())
        }
        .await;
        let source_closed = self.client.session.close(source).await;
        let target_closed = self.client.session.close(target).await;
        if result.is_err() || source_closed.is_err() || target_closed.is_err() {
            return Err(StagedUploadError::Uncertain);
        }
        if self
            .probe(backup, expected.size)
            .await
            .map_err(|_| StagedUploadError::Uncertain)?
            != Some(expected)
        {
            return Err(StagedUploadError::Uncertain);
        }
        Ok(())
    }
    /// Replacement has no server-side compare-and-swap. The caller explicitly
    /// authorizes replacement under exclusive external access; keep the copy backup.
    pub async fn publish_existing(
        &self,
        temporary: &str,
        destination: &str,
        backup: &str,
        previous: Option<StagedUploadContent>,
        expected: StagedUploadContent,
        replace: bool,
        maximum: u64,
    ) -> Result<(bool, bool), StagedUploadError> {
        if !self.client.verified_publish || replace && !self.client.verified_replace {
            return Err(StagedUploadError::Unsupported);
        }
        for path in [temporary, destination, backup] {
            if !valid_path(path)
                || path.rsplit_once('/').map(|p| p.0) != destination.rsplit_once('/').map(|p| p.0)
            {
                return Err(StagedUploadError::Invalid);
            }
        }
        if temporary == destination || temporary == backup || backup == destination {
            return Err(StagedUploadError::Invalid);
        }
        if self.probe(destination, maximum).await? != previous {
            return Err(StagedUploadError::Changed);
        }
        if previous == Some(expected) {
            return Ok((true, false));
        }
        let Some(previous) = previous else {
            self.publish(temporary, destination).await?;
            return Ok((false, false));
        };
        if !replace {
            return Err(StagedUploadError::Exists);
        }
        self.copy_backup(destination, backup, previous).await?;
        let result = async {
            if self.probe(destination, maximum).await? != Some(previous)
                || self.probe(backup, maximum).await? != Some(previous)
            {
                return Err(StagedUploadError::Changed);
            }
            let mut payload = Vec::new();
            for path in [temporary, destination] {
                payload.extend_from_slice(&(path.len() as u32).to_be_bytes());
                payload.extend_from_slice(path.as_bytes());
            }
            match self
                .client
                .session
                .extended("posix-rename@openssh.com", payload)
                .await
            {
                Ok(russh_sftp::protocol::Packet::Status(status))
                    if status.status_code == StatusCode::Ok => {}
                _ => return Err(StagedUploadError::Uncertain),
            }
            if self.probe(destination, maximum).await? != Some(expected)
                || self.probe(backup, maximum).await? != Some(previous)
            {
                return Err(StagedUploadError::Changed);
            }
            Ok((false, true))
        }
        .await;
        result.map_err(|_| StagedUploadError::Uncertain)
    }
    pub async fn preflight(
        &self,
        temporary: &str,
        destination: &str,
    ) -> Result<(), StagedUploadError> {
        if !self.client.verified_publish {
            return Err(StagedUploadError::Unsupported);
        }
        if !valid_path(temporary)
            || !valid_path(destination)
            || temporary == destination
            || temporary.rsplit_once('/').map(|p| p.0) != destination.rsplit_once('/').map(|p| p.0)
        {
            return Err(StagedUploadError::Invalid);
        }
        let (parent, _) = destination
            .rsplit_once('/')
            .ok_or(StagedUploadError::Invalid)?;
        let parent = if parent.is_empty() { "/" } else { parent };
        self.client
            .directory_path(parent, false)
            .await
            .map_err(|_| StagedUploadError::Invalid)?;
        let attrs = self
            .client
            .session
            .lstat(parent)
            .await
            .map_err(|_| StagedUploadError::Uncertain)?
            .attrs;
        if attrs.file_type() != FileType::Dir {
            return Err(StagedUploadError::Invalid);
        }
        for path in [temporary, destination] {
            match self.client.session.lstat(path).await {
                Ok(_) => return Err(StagedUploadError::Exists),
                Err(error) if absent(&error) => {}
                Err(_) => return Err(StagedUploadError::Uncertain),
            }
        }
        Ok(())
    }
    pub async fn create(&mut self, temporary: &str) -> Result<(), StagedUploadError> {
        if !self.client.verified_publish || self.handle.is_some() || !valid_path(temporary) {
            return Err(StagedUploadError::Invalid);
        }
        let attrs = FileAttributes {
            permissions: Some(0o600),
            ..Default::default()
        };
        let result = self
            .client
            .session
            .open(
                temporary,
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                attrs,
            )
            .await;
        match result {
            Ok(handle) => {
                self.handle = Some(handle.handle);
                Ok(())
            }
            Err(_) => Err(StagedUploadError::Uncertain),
        }
    }
    pub async fn reopen(&mut self, temporary: &str) -> Result<u64, StagedUploadError> {
        if !self.client.verified_publish || !valid_path(temporary) || self.handle.is_some() {
            return Err(StagedUploadError::Invalid);
        }
        let before = self
            .client
            .session
            .lstat(temporary)
            .await
            .map_err(|_| StagedUploadError::Uncertain)?
            .attrs;
        if before.file_type() != FileType::File {
            return Err(StagedUploadError::Invalid);
        }
        self.handle = Some(
            self.client
                .session
                .open(
                    temporary,
                    OpenFlags::READ | OpenFlags::WRITE,
                    FileAttributes::default(),
                )
                .await
                .map_err(|_| StagedUploadError::Uncertain)?
                .handle,
        );
        let attrs = self
            .client
            .session
            .fstat(
                self.handle
                    .as_ref()
                    .ok_or(StagedUploadError::Invalid)?
                    .clone(),
            )
            .await
            .map_err(|_| StagedUploadError::Uncertain)?
            .attrs;
        if attrs.file_type() != FileType::File || attrs.size != before.size {
            return Err(StagedUploadError::Changed);
        }
        attrs.size.ok_or(StagedUploadError::Invalid)
    }
    pub async fn read(
        &self,
        temporary: &str,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>, StagedUploadError> {
        if !valid_path(temporary) || limit == 0 || limit > SFTP_TRANSFER_CHUNK_BYTES {
            return Err(StagedUploadError::Invalid);
        }
        let (handle, close) = match &self.handle {
            Some(handle) => (handle.clone(), false),
            None => (
                self.client
                    .session
                    .open(temporary, OpenFlags::READ, FileAttributes::default())
                    .await
                    .map_err(|_| StagedUploadError::Uncertain)?
                    .handle,
                true,
            ),
        };
        let size = self
            .client
            .read_chunk_bytes()
            .map_err(|_| StagedUploadError::Invalid)?
            .min(limit as u32);
        let result = self
            .client
            .session
            .read(handle.clone(), offset, size as u32)
            .await;
        if close {
            self.client
                .session
                .close(handle)
                .await
                .map_err(|_| StagedUploadError::Uncertain)?;
        }
        match result {
            Ok(data) if data.data.len() <= size as usize => Ok(data.data.to_vec()),
            Err(error) if is_status(&error, StatusCode::Eof) => Ok(Vec::new()),
            _ => Err(StagedUploadError::Uncertain),
        }
    }
    pub async fn write(&self, offset: u64, bytes: &[u8]) -> Result<(), StagedUploadError> {
        if bytes.is_empty() || bytes.len() > SFTP_TRANSFER_BATCH_BYTES {
            return Err(StagedUploadError::Invalid);
        }
        let handle = self.handle.as_ref().ok_or(StagedUploadError::Invalid)?;
        self.client
            .write_range(handle, offset, bytes)
            .await
            .map_err(|_| StagedUploadError::Uncertain)?;
        Ok(())
    }
    pub async fn verify(
        &mut self,
        temporary: &str,
        limit: u64,
    ) -> Result<StagedUploadContent, StagedUploadError> {
        self.flush().await?;
        self.digest(temporary, limit).await
    }
    pub async fn verify_with_progress(&mut self,temporary:&str,limit:u64,progress:&ReadProgress)->Result<StagedUploadContent,StagedUploadError> {
        self.flush().await?;
        digest_content_with_progress(&self.client,temporary,limit,Some(progress)).await
    }
    pub async fn probe_with_progress(&self,path:&str,limit:u64,progress:&ReadProgress)->Result<Option<StagedUploadContent>,StagedUploadError> {
        if !valid_path(path) {return Err(StagedUploadError::Invalid);}
        match self.client.session.lstat(path).await {
            Ok(_)=>digest_content_with_progress(&self.client,path,limit,Some(progress)).await.map(Some),
            Err(error) if absent(&error)=>{progress(0,0);Ok(None)},
            Err(_)=>Err(StagedUploadError::Uncertain),
        }
    }
    pub async fn flush(&self) -> Result<(), StagedUploadError> {
        if !self.client.verified_publish {
            return Err(StagedUploadError::Unsupported);
        }
        let handle = self.handle.as_ref().ok_or(StagedUploadError::Invalid)?;
        self.client
            .session
            .fsync(handle.clone())
            .await
            .map_err(|_| StagedUploadError::Uncertain)?;
        Ok(())
    }
    async fn digest(
        &self,
        path: &str,
        limit: u64,
    ) -> Result<StagedUploadContent, StagedUploadError> {
        digest_content(&self.client, path, limit).await
    }
    pub async fn set_permissions(&self, permissions: u32) -> Result<(), StagedUploadError> {
        if permissions > 0o777 {
            return Err(StagedUploadError::Invalid);
        }
        let handle = self.handle.as_ref().ok_or(StagedUploadError::Invalid)?;
        self.client
            .session
            .fsetstat(
                handle.clone(),
                FileAttributes {
                    permissions: Some(permissions),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| StagedUploadError::Uncertain)?;
        self.flush().await
    }
    pub async fn set_modified(&self,seconds:u32)->Result<(),StagedUploadError>{
        let handle=self.handle.as_ref().ok_or(StagedUploadError::Invalid)?;
        let attrs=self.client.session.fstat(handle.clone()).await.map_err(|_|StagedUploadError::Uncertain)?.attrs;
        if attrs.file_type()!=FileType::File{return Err(StagedUploadError::Changed);}
        self.client.session.fsetstat(handle.clone(),FileAttributes{atime:Some(attrs.atime.unwrap_or(seconds)),mtime:Some(seconds),..Default::default()}).await.map_err(|_|StagedUploadError::Uncertain)?;
        self.flush().await?;
        let actual=self.client.session.fstat(handle.clone()).await.map_err(|_|StagedUploadError::Uncertain)?.attrs;
        if actual.mtime!=Some(seconds)||actual.size!=attrs.size{return Err(StagedUploadError::Changed);}Ok(())
    }
    pub async fn permissions(&self, path: &str) -> Result<Option<u32>, StagedUploadError> {
        self.client
            .metadata(path, SymlinkPolicy::NoFollow)
            .await
            .map(|m| m.permissions.map(|p| p & 0o7777))
            .map_err(|_| StagedUploadError::Uncertain)
    }
    pub async fn publish(
        &self,
        temporary: &str,
        destination: &str,
    ) -> Result<(), StagedUploadError> {
        if !self.client.verified_publish
            || !valid_path(temporary)
            || !valid_path(destination)
            || temporary.rsplit_once('/').map(|p| p.0) != destination.rsplit_once('/').map(|p| p.0)
        {
            return Err(StagedUploadError::Invalid);
        }
        match self.client.session.hardlink(temporary, destination).await {
            Ok(_) => Ok(()),
            Err(_) => Err(StagedUploadError::Uncertain),
        }
    }
    pub async fn cleanup(&mut self, temporary: &str) -> Result<(), StagedUploadError> {
        if !valid_path(temporary) {
            return Err(StagedUploadError::Invalid);
        }
        if let Some(handle) = self.handle.take() {
            self.client
                .session
                .close(handle)
                .await
                .map_err(|_| StagedUploadError::Uncertain)?;
        }
        match self.client.session.lstat(temporary).await {
            Ok(attrs) if attrs.attrs.file_type() == FileType::File => {}
            Ok(_) => return Err(StagedUploadError::Invalid),
            Err(error) if absent(&error) => return Ok(()),
            Err(_) => return Err(StagedUploadError::Uncertain),
        }
        self.client
            .session
            .remove(temporary)
            .await
            .map_err(|_| StagedUploadError::Uncertain)?;
        Ok(())
    }
    pub async fn probe(
        &self,
        path: &str,
        limit: u64,
    ) -> Result<Option<StagedUploadContent>, StagedUploadError> {
        if !valid_path(path) {
            return Err(StagedUploadError::Invalid);
        }
        match self.client.session.lstat(path).await {
            Ok(_) => self.digest(path, limit).await.map(Some),
            Err(error) if absent(&error) => Ok(None),
            Err(_) => Err(StagedUploadError::Uncertain),
        }
    }
    pub async fn close(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _closed = self.client.session.close(handle).await;
        }
        let _closed = self.client.close().await;
    }
    /// Close this SFTP channel without inferring file ownership or removing files.
    pub fn abort(&mut self) {
        self.handle = None;
        let _closed = self.client.session.close_session();
    }
}
