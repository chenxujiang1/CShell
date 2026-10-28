//! Upload connections are task-owned and use the frozen credential template.
use crate::{
    ProfileIpcService,
    ssh_route::{JumpConnection, SshConnectionPlan, connect_frozen_target},
};
use cshell_application::*;
use cshell_domain::HostId;
use cshell_sftp::{StagedUpload, StagedUploadContent, StagedUploadError};
use cshell_ssh::SshConnection;
use std::{fmt, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Debug)]
pub(crate) struct SshUploadExecutor(pub(crate) Arc<ProfileIpcService>);
struct LocalSource {
    file: tokio::fs::File,
    size: u64,
    modified: Option<std::time::SystemTime>,
}
impl fmt::Debug for LocalSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalSource")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
#[async_trait::async_trait]
impl UploadSource for LocalSource {
    async fn read(&mut self, offset: u64, limit: usize) -> Result<Vec<u8>, UploadReason> {
        if limit == 0 || limit > UPLOAD_CHUNK_BYTES || offset > self.size {
            return Err(UploadReason::Invalid);
        }
        let metadata = self
            .file
            .metadata()
            .await
            .map_err(|_| UploadReason::SourceChanged)?;
        if metadata.len() != self.size || metadata.modified().ok() != self.modified {
            return Err(UploadReason::SourceChanged);
        }
        self.file
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|_| UploadReason::SourceChanged)?;
        let mut bytes = vec![0; limit];
        let read = self
            .file
            .read(&mut bytes)
            .await
            .map_err(|_| UploadReason::SourceChanged)?;
        bytes.truncate(read);
        Ok(bytes)
    }
}
#[async_trait::async_trait]
impl UploadExecutor for SshUploadExecutor {
    async fn source(
        &self,
        snapshot: &BatchUploadSnapshot,
    ) -> Result<Box<dyn UploadSource>, UploadReason> {
        let path = std::path::Path::new(&snapshot.definition.local_source);
        let before = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| UploadReason::SourceChanged)?;
        if !before.is_file()
            || before.file_type().is_symlink()
            || before.len() != snapshot.definition.size
        {
            return Err(UploadReason::SourceChanged);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if before.file_attributes() & 0x400 != 0 {
                return Err(UploadReason::SourceChanged);
            }
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| UploadReason::SourceChanged)?;
        let after = file
            .metadata()
            .await
            .map_err(|_| UploadReason::SourceChanged)?;
        if !after.is_file()
            || after.len() != before.len()
            || after.modified().ok() != before.modified().ok()
        {
            return Err(UploadReason::SourceChanged);
        }
        Ok(Box::new(LocalSource {
            file,
            size: snapshot.definition.size,
            modified: after.modified().ok(),
        }))
    }
    async fn connect(
        &self,
        snapshot: &BatchUploadSnapshot,
        host: HostId,
        cancel: TaskCancellation,
    ) -> Result<Box<dyn UploadRemote>, UploadReason> {
        let target = snapshot
            .targets
            .resolved_connection(host)
            .ok_or(UploadReason::Invalid)?;
        let original = snapshot
            .targets
            .credential_template(host)
            .ok_or(UploadReason::Invalid)?;
        let path = self
            .0
            .known_hosts_path()
            .map_err(|_| UploadReason::Connection)?;
        let plan = SshConnectionPlan::new(String::new(), target, snapshot.targets.connections())
            .map_err(|_| UploadReason::Invalid)?;
        let mut changes = self.0.vault.cancellation();
        let generation = *changes.borrow_and_update();
        let connected = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(UploadReason::Cancelled),
            _ = changes.changed() => return Err(UploadReason::Connection),
            result = connect_frozen_target(&plan, original, &path, &self.0.vault) => result.map_err(|_| UploadReason::Connection)?,
        };
        let client = Arc::new(connected.client);
        let mut remote = RemoteUpload {
            client,
            jump: connected.jump,
            staged: None,
            limit: snapshot.definition.size,
            policy: snapshot.definition.publication_policy,
        };
        let opened = tokio::select! { biased;
            _ = cancel.cancelled() => Err(UploadReason::Cancelled),
            _ = changes.changed() => Err(UploadReason::Connection),
            result = remote.client.open_sftp() => result.map_err(|_| UploadReason::Connection),
        };
        let opened = match opened {
            Ok(opened) => opened,
            Err(error) => {
                remote.close().await;
                return Err(error);
            }
        };
        remote.staged = Some(opened.staged_upload());
        if *changes.borrow() != generation {
            remote.close().await;
            return Err(UploadReason::Connection);
        }
        Ok(Box::new(remote))
    }
}
struct RemoteUpload {
    client: Arc<SshConnection>,
    jump: Option<JumpConnection>,
    staged: Option<StagedUpload>,
    limit: u64,
    policy: Option<FilePublicationPolicy>,
}
impl fmt::Debug for RemoteUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteUpload").finish_non_exhaustive()
    }
}
fn reason(error: StagedUploadError) -> UploadReason {
    match error {
        StagedUploadError::Unsupported => UploadReason::Unsupported,
        StagedUploadError::Exists => UploadReason::DestinationExists,
        StagedUploadError::Invalid => UploadReason::Invalid,
        StagedUploadError::Uncertain => UploadReason::Transport,
        StagedUploadError::Changed => UploadReason::Checksum,
    }
}
fn content(content: StagedUploadContent) -> UploadContent {
    UploadContent {
        size: content.size,
        sha256: content.sha256,
    }
}
impl RemoteUpload {
    fn staged(&mut self) -> Result<&mut StagedUpload, UploadReason> {
        self.staged.as_mut().ok_or(UploadReason::Connection)
    }
}
#[async_trait::async_trait]
impl UploadRemote for RemoteUpload {
    async fn plan_publication(
        &mut self,
        temporary: &str,
        destination: &str,
        backup: &str,
        policy: Option<FilePublicationPolicy>,
        expected: UploadContent,
    ) -> Result<Option<FilePublicationReceipt>, UploadReason> {
        let Some(policy) = policy else {
            self.preflight(temporary, destination).await?;
            return Ok(None);
        };
        let previous = self
            .staged()?
            .preflight_existing(
                temporary,
                destination,
                backup,
                policy == FilePublicationPolicy::BackupReplace,
                MAX_UPLOAD_FILE_BYTES,
            )
            .await
            .map_err(reason)?
            .map(content);
        if policy == FilePublicationPolicy::SkipIdentical && previous.is_some_and(|p| p != expected)
        {
            return Err(UploadReason::DestinationExists);
        }
        Ok(Some(FilePublicationReceipt::planned(previous)))
    }
    async fn publish_planned(
        &mut self,
        temporary: &str,
        destination: &str,
        backup: &str,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: UploadContent,
    ) -> Result<Option<FilePublicationReceipt>, UploadReason> {
        let Some(policy) = policy else {
            self.publish(temporary, destination).await?;
            return Ok(None);
        };
        let mut receipt = planned.ok_or(UploadReason::Invalid)?;
        let to_staged = |p: UploadContent| StagedUploadContent {
            size: p.size,
            sha256: p.sha256,
        };
        let (skipped, backed) = self
            .staged()?
            .publish_existing(
                temporary,
                destination,
                backup,
                receipt.previous.map(to_staged),
                to_staged(expected),
                policy == FilePublicationPolicy::BackupReplace,
                MAX_UPLOAD_FILE_BYTES,
            )
            .await
            .map_err(reason)?;
        receipt.skipped_identical = skipped;
        receipt.backup_verified = backed;
        Ok(Some(receipt))
    }
    async fn recover_publication(
        &mut self,
        backup: &str,
        policy: Option<FilePublicationPolicy>,
        planned: Option<FilePublicationReceipt>,
        expected: UploadContent,
    ) -> Result<Option<FilePublicationReceipt>, UploadReason> {
        let Some(policy) = policy else {
            return Ok(None);
        };
        let mut receipt = planned.ok_or(UploadReason::Invalid)?;
        if receipt.previous == Some(expected) {
            receipt.skipped_identical = true;
        } else if policy == FilePublicationPolicy::BackupReplace && receipt.previous.is_some() {
            let actual = self
                .staged()?
                .probe(backup, MAX_UPLOAD_FILE_BYTES)
                .await
                .map_err(reason)?
                .map(content);
            if actual != receipt.previous {
                return Err(UploadReason::Checksum);
            }
            receipt.backup_verified = true;
        }
        receipt
            .validate_commit(policy, expected)
            .map_err(|_| UploadReason::Invalid)?;
        Ok(Some(receipt))
    }
    async fn preflight(&mut self, temporary: &str, destination: &str) -> Result<(), UploadReason> {
        self.staged()?
            .preflight(temporary, destination)
            .await
            .map_err(reason)
    }
    async fn create(&mut self, temporary: &str) -> Result<(), UploadReason> {
        self.staged()?.create(temporary).await.map_err(reason)
    }
    async fn reopen(&mut self, temporary: &str) -> Result<u64, UploadReason> {
        self.staged()?.reopen(temporary).await.map_err(reason)
    }
    async fn read(
        &mut self,
        temporary: &str,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>, UploadReason> {
        self.staged()?
            .read(temporary, offset, limit)
            .await
            .map_err(reason)
    }
    async fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), UploadReason> {
        self.staged()?.write(offset, bytes).await.map_err(reason)
    }
    async fn verify(&mut self, temporary: &str) -> Result<UploadContent, UploadReason> {
        let limit = self.limit;
        self.staged()?
            .verify(temporary, limit)
            .await
            .map(content)
            .map_err(reason)
    }
    async fn publish(&mut self, temporary: &str, destination: &str) -> Result<(), UploadReason> {
        self.staged()?
            .publish(temporary, destination)
            .await
            .map_err(reason)
    }
    async fn cleanup(&mut self, temporary: &str) -> Result<(), UploadReason> {
        self.staged()?.cleanup(temporary).await.map_err(reason)
    }
    async fn probe(
        &mut self,
        temporary: &str,
        destination: &str,
    ) -> Result<UploadProbe, UploadReason> {
        let limit = self.limit;
        let temporary = self
            .staged()?
            .probe(temporary, limit)
            .await
            .map_err(reason)?
            .map(content);
        let destination_limit = if self.policy.is_some() {
            MAX_UPLOAD_FILE_BYTES
        } else {
            limit
        };
        let destination = self
            .staged()?
            .probe(destination, destination_limit)
            .await
            .map_err(reason)?
            .map(content);
        Ok(UploadProbe {
            temporary,
            destination,
        })
    }
    async fn close(&mut self) {
        if let Some(mut staged) = self.staged.take() {
            staged.close().await;
        }
        let _closed = self.client.disconnect().await;
        if let Some(jump) = self.jump.take() {
            jump.disconnect().await;
        }
    }
}
impl Drop for RemoteUpload {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let mut staged = self.staged.take();
            let jump = self.jump.take();
            runtime.spawn(async move {
                let _closed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    if let Some(staged) = &mut staged {
                        staged.close().await;
                    }
                    let _closed = client.disconnect().await;
                    if let Some(jump) = jump {
                        jump.disconnect().await;
                    }
                })
                .await;
            });
        }
    }
}
