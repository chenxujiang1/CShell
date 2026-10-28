//! Daemon-side local sink. No recursive cleanup and no destination replacement.
use cshell_application::*;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub(crate) struct LocalDownload {
    root: PathBuf,
    temporary: PathBuf,
    destination: PathBuf,
    file: Option<tokio::fs::File>,
    verified: Option<std::fs::Metadata>,
    policy: Option<FilePublicationPolicy>,
    backup: PathBuf,
}
impl fmt::Debug for LocalDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalDownload")
            .field("open", &self.file.is_some())
            .finish_non_exhaustive()
    }
}
fn regular(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.dev() != right.dev() || left.ino() != right.ino() {
            return false;
        }
    }
    left.is_file()
        && right.is_file()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}
async fn directory(root: &Path) -> Result<(), DownloadReason> {
    // Resolve no file operations through a symlink/reparse directory component.
    for path in root.ancestors() {
        let metadata = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        if !metadata.is_dir() || !regular(&metadata) {
            return Err(DownloadReason::Invalid);
        }
    }
    Ok(())
}
async fn sync_directory(root: &Path) -> Result<(), DownloadReason> {
    #[cfg(unix)]
    {
        tokio::fs::File::open(root)
            .await
            .map_err(|_| DownloadReason::LocalIo)?
            .sync_all()
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}
async fn absent(path: &Path) -> Result<(), DownloadReason> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Err(DownloadReason::DestinationExists),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(DownloadReason::LocalIo),
    }
}
impl LocalDownload {
    pub(crate) async fn set_modified(&mut self,seconds:u32)->Result<(),DownloadReason>{
        directory(&self.root).await?;
        let opened=self.file.as_ref().ok_or(DownloadReason::Invalid)?;
        let before=opened.metadata().await.map_err(|_|DownloadReason::LocalIo)?;
        let named=tokio::fs::symlink_metadata(&self.temporary).await.map_err(|_|DownloadReason::LocalIo)?;
        if !same_file(&before,&named)||self.verified.as_ref().is_some_and(|v|!same_file(v,&before)){return Err(DownloadReason::Checksum);}
        let file=opened.try_clone().await.map_err(|_|DownloadReason::LocalIo)?.into_std().await;
        file.set_modified(std::time::UNIX_EPOCH+std::time::Duration::from_secs(u64::from(seconds))).map_err(|_|DownloadReason::LocalIo)?;
        file.sync_all().map_err(|_|DownloadReason::LocalIo)?;
        let after=opened.metadata().await.map_err(|_|DownloadReason::LocalIo)?;
        let named=tokio::fs::symlink_metadata(&self.temporary).await.map_err(|_|DownloadReason::LocalIo)?;
        if !same_file(&after,&named)||after.len()!=before.len(){return Err(DownloadReason::Checksum);}
        if self.verified.is_some(){self.verified=Some(after);}Ok(())
    }
    pub(crate) async fn verify_size(&mut self,expected:u64)->Result<(),DownloadReason> {
        self.flush().await?;
        directory(&self.root).await?;
        let named=tokio::fs::symlink_metadata(&self.temporary).await.map_err(|_|DownloadReason::LocalIo)?;
        let opened=self.opened()?.metadata().await.map_err(|_|DownloadReason::LocalIo)?;
        if !regular(&named) || !same_file(&named,&opened) || opened.len()!=expected {return Err(DownloadReason::Checksum);}
        self.verified=Some(opened);Ok(())
    }
    pub(crate) async fn probe_sizes(&self)->Result<(Option<u64>,Option<u64>),DownloadReason> {
        directory(&self.root).await?;
        async fn size(path:&Path)->Result<Option<u64>,DownloadReason> {
            match tokio::fs::symlink_metadata(path).await {
                Ok(meta) if meta.is_file() && regular(&meta)=>Ok(Some(meta.len())),
                Ok(_)=>Err(DownloadReason::Invalid),
                Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(None),
                Err(_)=>Err(DownloadReason::LocalIo),
            }
        }
        Ok((size(&self.temporary).await?,size(&self.destination).await?))
    }
    pub(crate) fn for_session(
        destination: PathBuf,
        temporary: PathBuf,
    ) -> Result<Self, DownloadReason> {
        let root = destination
            .parent()
            .ok_or(DownloadReason::Invalid)?
            .to_owned();
        if !destination.is_absolute()
            || !temporary.is_absolute()
            || temporary.parent() != Some(root.as_path())
            || destination == temporary
        {
            return Err(DownloadReason::Invalid);
        }
        Ok(Self {
            root,
            backup: temporary.with_extension("unused-backup"),
            temporary,
            destination,
            file: None,
            verified: None,
            policy: None,
        })
    }
    pub(crate) fn session_policy(
        &mut self,
        policy: Option<FilePublicationPolicy>,
        backup: PathBuf,
    ) -> Result<(), DownloadReason> {
        if backup.parent() != Some(self.root.as_path())
            || backup == self.temporary
            || backup == self.destination
        {
            return Err(DownloadReason::Invalid);
        }
        self.policy = policy;
        self.backup = backup;
        Ok(())
    }
    pub(crate) fn new(
        snapshot: &BatchDownloadSnapshot,
        host: cshell_domain::HostId,
    ) -> Result<Self, DownloadReason> {
        let (temporary, destination) = snapshot
            .local_paths(host)
            .map_err(|_| DownloadReason::Invalid)?;
        Ok(Self {
            root: PathBuf::from(&snapshot.definition.local_directory),
            temporary,
            destination,
            file: None,
            verified: None,
            policy: snapshot.definition.publication_policy,
            backup: Path::new(&snapshot.definition.local_directory)
                .join(format!(".cshell-backup-{}-{host}.bak", snapshot.id)),
        })
    }
    fn opened(&mut self) -> Result<&mut tokio::fs::File, DownloadReason> {
        self.file.as_mut().ok_or(DownloadReason::Invalid)
    }
    async fn fingerprint(path: &Path, maximum: u64) -> Result<DownloadContent, DownloadReason> {
        let before = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        if !before.is_file() || !regular(&before) || before.len() > maximum {
            return Err(DownloadReason::Invalid);
        }
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        let opened = file.metadata().await.map_err(|_| DownloadReason::LocalIo)?;
        if !same_file(&before, &opened) {
            return Err(DownloadReason::Checksum);
        }
        let mut hash = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0; UPLOAD_CHUNK_BYTES];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|_| DownloadReason::LocalIo)?;
            if read == 0 {
                break;
            }
            size = size
                .checked_add(read as u64)
                .ok_or(DownloadReason::Invalid)?;
            if size > before.len() || size > maximum {
                return Err(DownloadReason::Checksum);
            }
            hash.update(&buffer[..read]);
        }
        let after = file.metadata().await.map_err(|_| DownloadReason::LocalIo)?;
        let named = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        if size != before.len()
            || !regular(&named)
            || !same_file(&before, &after)
            || !same_file(&named, &after)
        {
            return Err(DownloadReason::Checksum);
        }
        Ok(DownloadContent {
            size,
            sha256: hash.finalize().into(),
        })
    }
    async fn inspect_optional(
        path: &Path,
        maximum: u64,
    ) -> Result<Option<DownloadContent>, DownloadReason> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(_) => Self::fingerprint(path, maximum).await.map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(DownloadReason::LocalIo),
        }
    }
}
#[async_trait::async_trait]
impl DownloadLocal for LocalDownload {
    async fn plan_publication(
        &mut self,
        policy: Option<FilePublicationPolicy>,
        expected: DownloadContent,
    ) -> Result<Option<FilePublicationReceipt>, DownloadReason> {
        self.policy = policy;
        let Some(policy) = policy else {
            return Ok(None);
        };
        directory(&self.root).await?;
        absent(&self.backup).await?;
        let previous = Self::inspect_optional(&self.destination, MAX_DOWNLOAD_FILE_BYTES)
            .await?
            .map(|p| UploadContent {
                size: p.size,
                sha256: p.sha256,
            });
        let expected = UploadContent {
            size: expected.size,
            sha256: expected.sha256,
        };
        if policy == FilePublicationPolicy::SkipIdentical && previous.is_some_and(|p| p != expected)
        {
            return Err(DownloadReason::DestinationExists);
        }
        Ok(Some(FilePublicationReceipt::planned(previous)))
    }
    async fn publish_planned(
        &mut self,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: DownloadContent,
    ) -> Result<Option<FilePublicationReceipt>, DownloadReason> {
        let Some(policy) = policy else {
            self.publish().await?;
            return Ok(None);
        };
        directory(&self.root).await?;
        let mut receipt = planned.ok_or(DownloadReason::Invalid)?;
        let expected_upload = UploadContent {
            size: expected.size,
            sha256: expected.sha256,
        };
        let to_upload = |p: DownloadContent| UploadContent {
            size: p.size,
            sha256: p.sha256,
        };
        if Self::inspect_optional(&self.destination, MAX_DOWNLOAD_FILE_BYTES)
            .await?
            .map(to_upload)
            != receipt.previous
        {
            return Err(DownloadReason::Checksum);
        }
        if receipt.previous == Some(expected_upload) {
            receipt.skipped_identical = true;
            return Ok(Some(receipt));
        }
        let Some(previous) = receipt.previous else {
            self.publish().await?;
            return Ok(Some(receipt));
        };
        if policy != FilePublicationPolicy::BackupReplace {
            return Err(DownloadReason::DestinationExists);
        }
        absent(&self.backup).await?;
        let result = async {
            let mut source = tokio::fs::File::open(&self.destination)
                .await
                .map_err(|_| DownloadReason::LocalIo)?;
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut backup = options
                .open(&self.backup)
                .await
                .map_err(|_| DownloadReason::LocalIo)?;
            let mut remaining = previous.size;
            let mut buffer = vec![0; UPLOAD_CHUNK_BYTES];
            let mut hash = Sha256::new();
            while remaining > 0 {
                let n = buffer.len().min(remaining as usize);
                source
                    .read_exact(&mut buffer[..n])
                    .await
                    .map_err(|_| DownloadReason::LocalIo)?;
                hash.update(&buffer[..n]);
                backup
                    .write_all(&buffer[..n])
                    .await
                    .map_err(|_| DownloadReason::LocalIo)?;
                remaining -= n as u64;
            }
            backup.flush().await.map_err(|_| DownloadReason::LocalIo)?;
            backup
                .sync_all()
                .await
                .map_err(|_| DownloadReason::LocalIo)?;
            let actual: [u8; 32] = hash.finalize().into();
            if actual != previous.sha256 {
                return Err(DownloadReason::Checksum);
            }
            drop(backup);
            drop(source);
            if Self::fingerprint(&self.backup, MAX_DOWNLOAD_FILE_BYTES)
                .await
                .map(to_upload)?
                != previous
                || Self::fingerprint(&self.destination, MAX_DOWNLOAD_FILE_BYTES)
                    .await
                    .map(to_upload)?
                    != previous
            {
                return Err(DownloadReason::Checksum);
            }
            // Recheck staged bytes and handle identity immediately before replacement.
            if self.verify(MAX_DOWNLOAD_FILE_BYTES).await? != expected {
                return Err(DownloadReason::Checksum);
            }
            sync_directory(&self.root).await?;
            self.close().await;
            tokio::fs::rename(&self.temporary, &self.destination)
                .await
                .map_err(|_| DownloadReason::LocalIo)?;
            sync_directory(&self.root).await?;
            if Self::fingerprint(&self.destination, MAX_DOWNLOAD_FILE_BYTES).await? != expected
                || Self::fingerprint(&self.backup, MAX_DOWNLOAD_FILE_BYTES)
                    .await
                    .map(to_upload)?
                    != previous
            {
                return Err(DownloadReason::Checksum);
            }
            Ok(())
        }
        .await;
        // Backup and rename share a durable Publish intent; any later failure is uncertain.
        result.map_err(|_| DownloadReason::LocalIo)?;
        receipt.backup_verified = true;
        Ok(Some(receipt))
    }
    async fn recover_publication(
        &mut self,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: DownloadContent,
    ) -> Result<Option<FilePublicationReceipt>, DownloadReason> {
        let Some(policy) = policy else {
            return Ok(None);
        };
        let mut receipt = planned.ok_or(DownloadReason::Invalid)?;
        let expected = UploadContent {
            size: expected.size,
            sha256: expected.sha256,
        };
        if receipt.previous == Some(expected) {
            receipt.skipped_identical = true;
        } else if policy == FilePublicationPolicy::BackupReplace && receipt.previous.is_some() {
            let actual = Self::inspect_optional(&self.backup, MAX_DOWNLOAD_FILE_BYTES)
                .await?
                .map(|p| UploadContent {
                    size: p.size,
                    sha256: p.sha256,
                });
            if actual != receipt.previous {
                return Err(DownloadReason::Checksum);
            }
            receipt.backup_verified = true;
        }
        receipt
            .validate_commit(policy, expected)
            .map_err(|_| DownloadReason::Invalid)?;
        Ok(Some(receipt))
    }
    async fn create(&mut self) -> Result<(), DownloadReason> {
        if self.file.is_some() {
            return Err(DownloadReason::Invalid);
        }
        directory(&self.root).await?;
        if self.policy.is_none() {
            absent(&self.destination).await?;
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&self.temporary).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                DownloadReason::DestinationExists
            } else {
                DownloadReason::LocalIo
            }
        })?;
        self.file = Some(file);
        self.flush().await?;
        sync_directory(&self.root).await
    }
    async fn reopen(&mut self) -> Result<u64, DownloadReason> {
        if self.file.is_some() {
            return Err(DownloadReason::Invalid);
        }
        directory(&self.root).await?;
        if self.policy.is_none() {
            absent(&self.destination).await?;
        }
        let before = tokio::fs::symlink_metadata(&self.temporary)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        if !before.is_file() || !regular(&before) {
            return Err(DownloadReason::Invalid);
        }
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.temporary)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        let opened = file.metadata().await.map_err(|_| DownloadReason::LocalIo)?;
        if !same_file(&before, &opened) {
            return Err(DownloadReason::Checksum);
        }
        self.file = Some(file);
        Ok(opened.len())
    }
    async fn read(&mut self, offset: u64, limit: usize) -> Result<Vec<u8>, DownloadReason> {
        if limit == 0 || limit > UPLOAD_CHUNK_BYTES {
            return Err(DownloadReason::Invalid);
        }
        let file = self.opened()?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        let mut bytes = vec![0; limit];
        file.read_exact(&mut bytes)
            .await
            .map_err(|_| DownloadReason::Checksum)?;
        Ok(bytes)
    }
    async fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), DownloadReason> {
        if bytes.is_empty() || bytes.len() > UPLOAD_CHUNK_BYTES {
            return Err(DownloadReason::Invalid);
        }
        self.verified = None;
        let file = self.opened()?;
        if file
            .metadata()
            .await
            .map_err(|_| DownloadReason::LocalIo)?
            .len()
            != offset
        {
            return Err(DownloadReason::Checksum);
        }
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        file.write_all(bytes)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        file.flush().await.map_err(|_| DownloadReason::LocalIo)
    }
    async fn flush(&mut self) -> Result<(), DownloadReason> {
        let file = self.opened()?;
        file.flush().await.map_err(|_| DownloadReason::LocalIo)?;
        file.sync_all().await.map_err(|_| DownloadReason::LocalIo)
    }
    async fn verify(&mut self, maximum: u64) -> Result<DownloadContent, DownloadReason> {
        self.flush().await?;
        directory(&self.root).await?;
        let named = tokio::fs::symlink_metadata(&self.temporary)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        let opened = self
            .opened()?
            .metadata()
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        if !regular(&named) || !same_file(&named, &opened) {
            return Err(DownloadReason::Checksum);
        }
        let content = Self::fingerprint(&self.temporary, maximum).await?;
        let after = self
            .opened()?
            .metadata()
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        if !same_file(&opened, &after) {
            return Err(DownloadReason::Checksum);
        }
        self.verified = Some(after);
        Ok(content)
    }
    async fn publish(&mut self) -> Result<(), DownloadReason> {
        directory(&self.root).await?;
        let named = tokio::fs::symlink_metadata(&self.temporary)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        let opened = self
            .opened()?
            .metadata()
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        if !regular(&named)
            || !same_file(&named, &opened)
            || self
                .verified
                .as_ref()
                .is_none_or(|verified| !same_file(verified, &opened))
        {
            return Err(DownloadReason::Checksum);
        }
        tokio::fs::hard_link(&self.temporary, &self.destination)
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    DownloadReason::DestinationExists
                } else {
                    DownloadReason::LocalIo
                }
            })?;
        sync_directory(&self.root).await
    }
    async fn cleanup(&mut self) -> Result<(), DownloadReason> {
        self.close().await;
        directory(&self.root).await?;
        match tokio::fs::symlink_metadata(&self.temporary).await {
            Ok(metadata) if metadata.is_file() && regular(&metadata) => {}
            Ok(_) => return Err(DownloadReason::Invalid),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(DownloadReason::LocalIo),
        }
        tokio::fs::remove_file(&self.temporary)
            .await
            .map_err(|_| DownloadReason::LocalIo)?;
        sync_directory(&self.root).await
    }
    async fn probe(&mut self, maximum: u64) -> Result<DownloadProbe, DownloadReason> {
        directory(&self.root).await?;
        let temporary = Self::inspect_optional(&self.temporary, maximum).await?;
        let destination = Self::inspect_optional(&self.destination, maximum).await?;
        Ok(DownloadProbe {
            temporary,
            destination,
        })
    }
    async fn close(&mut self) {
        if let Some(mut file) = self.file.take() {
            let _flushed = file.flush().await;
        }
    }
}
