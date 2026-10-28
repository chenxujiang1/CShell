//! Read-only source handle with bounded full-content inspection and recheck.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PinnedReadError {
    #[error("invalid remote source or size limit")]
    Invalid,
    #[error("remote source content or metadata changed")]
    Changed,
    #[error("remote source read is unavailable")]
    Unavailable,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedReadContent {
    pub size: u64,
    pub sha256: [u8; 32],
}
pub struct PinnedRead {
    client: SftpClient,
    handle: Option<String>,
    size: u64,
    modified: Option<u32>,
}
impl std::fmt::Debug for PinnedRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedRead")
            .field("size", &self.size)
            .field("open", &self.handle.is_some())
            .finish()
    }
}
impl SftpClient {
    #[must_use]
    pub fn pinned_read(self) -> PinnedRead {
        PinnedRead {
            client: self,
            handle: None,
            size: 0,
            modified: None,
        }
    }
}
impl PinnedRead {
    pub async fn open_follow(
        &mut self,
        path: &str,
        maximum: u64,
    ) -> Result<PinnedReadContent, PinnedReadError> {
        let names = self
            .client
            .session
            .realpath(path)
            .await
            .map_err(|_| PinnedReadError::Unavailable)?;
        if names.files.len() != 1 {
            return Err(PinnedReadError::Invalid);
        }
        self.open(&names.files[0].filename, maximum).await
    }
    pub async fn open(
        &mut self,
        path: &str,
        maximum: u64,
    ) -> Result<PinnedReadContent, PinnedReadError> {
        self.open_size(path,maximum,false).await?;
        self.digest().await
    }
    /// Pin and inspect metadata without reading the complete source before transfer.
    pub async fn open_size(&mut self,path:&str,maximum:u64,follow:bool)->Result<u64,PinnedReadError> {
        let resolved;
        let path=if follow {
            let names=self.client.session.realpath(path).await.map_err(|_|PinnedReadError::Unavailable)?;
            if names.files.len()!=1 {return Err(PinnedReadError::Invalid);}
            resolved=names.files[0].filename.clone();&resolved
        } else {path};
        if self.handle.is_some()
            || !path.starts_with('/')
            || path.ends_with('/')
            || path.len() > 4000
            || path.contains('\\')
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(PinnedReadError::Invalid);
        }
        let parent = path.rsplit_once('/').ok_or(PinnedReadError::Invalid)?.0;
        self.client
            .directory_path(if parent.is_empty() { "/" } else { parent }, false)
            .await
            .map_err(|_| PinnedReadError::Invalid)?;
        let before = self
            .client
            .session
            .lstat(path)
            .await
            .map_err(|_| PinnedReadError::Unavailable)?
            .attrs;
        if before.file_type() != FileType::File || before.size.is_none_or(|size| size > maximum) {
            return Err(PinnedReadError::Invalid);
        }
        let handle = self
            .client
            .session
            .open(path, OpenFlags::READ, FileAttributes::default())
            .await
            .map_err(|_| PinnedReadError::Unavailable)?
            .handle;
        self.handle = Some(handle.clone());
        let attrs = self
            .client
            .session
            .fstat(handle)
            .await
            .map_err(|_| PinnedReadError::Unavailable)?
            .attrs;
        if attrs.file_type() != FileType::File
            || attrs.size != before.size
            || attrs.mtime != before.mtime
        {
            return Err(PinnedReadError::Changed);
        }
        self.size = attrs.size.ok_or(PinnedReadError::Invalid)?;
        self.modified = attrs.mtime;
        Ok(self.size)
    }
    pub async fn read(&self, offset: u64, limit: usize) -> Result<Vec<u8>, PinnedReadError> {
        if limit == 0 || limit > SFTP_TRANSFER_BATCH_BYTES || offset > self.size {
            return Err(PinnedReadError::Invalid);
        }
        let handle = self.handle.as_ref().ok_or(PinnedReadError::Invalid)?;
        if offset < self.size {
            let count = (self.size - offset).min(limit as u64) as usize;
            return self
                .client
                .read_range(handle, offset, count)
                .await
                .map_err(|_| PinnedReadError::Changed);
        }
        let limit = limit.min(
            self.client
                .read_chunk_bytes()
                .map_err(|_| PinnedReadError::Invalid)? as usize,
        );
        match self
            .client
            .session
            .read(handle.clone(), offset, limit as u32)
            .await
        {
            Ok(data) if !data.data.is_empty() && data.data.len() <= limit => Ok(data.data.to_vec()),
            Err(error) if is_status(&error, StatusCode::Eof) => Ok(Vec::new()),
            Ok(_) => Err(PinnedReadError::Changed),
            Err(_) => Err(PinnedReadError::Unavailable),
        }
    }
    pub async fn check_metadata(&self) -> Result<(), PinnedReadError> {
        let handle = self.handle.as_ref().ok_or(PinnedReadError::Invalid)?;
        let attrs = self
            .client
            .session
            .fstat(handle.clone())
            .await
            .map_err(|_| PinnedReadError::Unavailable)?
            .attrs;
        if attrs.file_type() != FileType::File
            || attrs.size != Some(self.size)
            || attrs.mtime != self.modified
        {
            return Err(PinnedReadError::Changed);
        }
        Ok(())
    }
    pub async fn digest(&self) -> Result<PinnedReadContent, PinnedReadError> {
        self.check_metadata().await?;
        let mut hash = Sha256::new();
        let mut offset = 0;
        while offset < self.size {
            let limit = SFTP_TRANSFER_BATCH_BYTES.min((self.size - offset) as usize);
            let bytes = self.read(offset, limit).await?;
            if bytes.is_empty() || bytes.len() > limit {
                return Err(PinnedReadError::Changed);
            }
            offset += bytes.len() as u64;
            hash.update(&bytes);
        }
        if !self.read(offset, 1).await?.is_empty() {
            return Err(PinnedReadError::Changed);
        }
        self.check_metadata().await?;
        Ok(PinnedReadContent {
            size: offset,
            sha256: hash.finalize().into(),
        })
    }
    pub async fn close(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _closed = self.client.session.close(handle).await;
        }
        let _closed = self.client.close().await;
    }
    pub fn abort(&mut self) {
        self.handle = None;
        let _closed = self.client.session.close_session();
    }
}
impl Drop for PinnedRead {
    fn drop(&mut self) {
        self.client.abort_channel();
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
