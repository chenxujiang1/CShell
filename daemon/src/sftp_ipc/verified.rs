//! Durable session transfers. Every file mutation follows a saved intent.
use super::queue::Queues;
use super::*;
use crate::download_local::LocalDownload;
use cshell_application::*;
use cshell_domain::SftpQueueItemId;
use cshell_sftp::{PinnedRead, StagedUpload, ReadProgress};
use cshell_ipc::SftpTransferPhase;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const CHUNK: usize = cshell_application::MAX_SFTP_WRITE_BATCH_BYTES as usize;
const _: () = assert!(CHUNK <= cshell_sftp::SFTP_TRANSFER_BATCH_BYTES);
type Result<T> = std::result::Result<T, &'static str>;
mod streaming;
#[derive(Clone, Copy)]
pub(super) enum VerifiedAction {
    Transfer { resume: bool },
    Adopt,
    Cleanup,
}
impl VerifiedAction {
    pub(super) fn from_operation(operation: SftpOperation) -> Self {
        match operation {
            SftpOperation::QueueAdoptVerified => Self::Adopt,
            SftpOperation::QueueCleanupVerified => Self::Cleanup,
            SftpOperation::QueueStartOne => Self::Transfer { resume: false },
            _ => Self::Transfer { resume: true },
        }
    }
}
fn regular(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() != b.dev() || a.ino() != b.ino() {
            return false;
        }
    }
    regular(a)
        && regular(b)
        && a.is_file()
        && b.is_file()
        && a.len() == b.len()
        && a.modified().ok() == b.modified().ok()
        && a.created().ok() == b.created().ok()
}
pub(super) struct LocalSource {
    file: tokio::fs::File,
    identity: std::fs::Metadata,
    path: PathBuf,
}
impl LocalSource {
    pub(super) async fn open_bounded(path: &Path, maximum: u64) -> Result<Self> {
        let source = Self::open(path).await?;
        if source.identity.len() > maximum {
            return Err("local source exceeds size limit");
        }
        Ok(source)
    }
    pub(super) async fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("invalid local source");
        }
        for parent in path.parent().ok_or("invalid local source")?.ancestors() {
            let meta = tokio::fs::symlink_metadata(parent)
                .await
                .map_err(|_| "local source unavailable")?;
            if !meta.is_dir() || !regular(&meta) {
                return Err("local source directory changed");
            }
        }
        let before = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|_| "local source unavailable")?;
        if !before.is_file() || !regular(&before) || before.len() > MAX_SFTP_VERIFIED_BYTES {
            return Err("invalid local source");
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        #[cfg(windows)]
        options.custom_flags(0x0020_0000);
        let file = options
            .open(path)
            .await
            .map_err(|_| "local source unavailable")?;
        let identity = file
            .metadata()
            .await
            .map_err(|_| "local source unavailable")?;
        if !same_file(&before, &identity) {
            return Err("local source changed");
        }
        Ok(Self {
            file,
            identity,
            path: path.to_owned(),
        })
    }
    async fn check(&self) -> Result<()> {
        let open = self
            .file
            .metadata()
            .await
            .map_err(|_| "local source changed")?;
        let named = tokio::fs::symlink_metadata(&self.path)
            .await
            .map_err(|_| "local source changed")?;
        if !same_file(&open, &self.identity) || !same_file(&named, &self.identity) {
            return Err("local source changed");
        }
        Ok(())
    }
    pub(super) async fn read(&mut self, at: u64, n: usize) -> Result<Vec<u8>> {
        self.check().await?;
        if n == 0 || n > CHUNK || at > self.identity.len() {
            return Err("invalid source read");
        }
        self.file
            .seek(std::io::SeekFrom::Start(at))
            .await
            .map_err(|_| "local source unavailable")?;
        let mut bytes = vec![0; n];
        let n = self
            .file
            .read(&mut bytes)
            .await
            .map_err(|_| "local source unavailable")?;
        bytes.truncate(n);
        Ok(bytes)
    }
    pub(super) async fn digest(&mut self) -> Result<SftpVerifiedContent> {
        self.check().await?;
        let size = self.identity.len();
        let mut at = 0;
        let mut hash = Sha256::new();
        while at < size {
            let n = (size - at).min(CHUNK as u64) as usize;
            let bytes = self.read(at, n).await?;
            if bytes.is_empty() {
                return Err("local source changed");
            }
            at += bytes.len() as u64;
            hash.update(bytes);
        }
        if !self.read(size, 1).await?.is_empty() {
            return Err("local source changed");
        }
        self.check().await?;
        Ok(SftpVerifiedContent {
            size,
            sha256: hash.finalize().into(),
        })
    }
}
enum Engine {
    Upload {
        checksum_connection: Option<Arc<cshell_ssh::SshConnection>>,
        checksum_ready: bool,
        source: Option<LocalSource>,
        source_path: PathBuf,
        staged: StagedUpload,
        temporary: String,
        destination: String,
        backup: String,
        policy: Option<FilePublicationPolicy>,
        follow_links: bool,
    },
    Download {
        source: PinnedRead,
        opened: bool,
        source_path: String,
        local: LocalDownload,
        follow_links: bool,
        policy: Option<FilePublicationPolicy>,
    },
}
impl Engine {
    async fn set_modified(&mut self,seconds:u32)->Result<()>{
        match self {Self::Upload{staged,..}=>staged.set_modified(seconds).await.map_err(|_|"temporary modification time update unconfirmed"),
            Self::Download{local,..}=>local.set_modified(seconds).await.map_err(|_|"local temporary modification time update unconfirmed")}
    }
    async fn expected_permissions(&self, expected: Option<u32>) -> Result<()> {
        if let Some(expected) = expected
            && let Self::Upload {
                staged,
                destination,
                ..
            } = self
        {
            if staged
                .permissions(destination)
                .await
                .map_err(|_| "destination permissions unavailable")?
                != Some(expected)
            {
                return Err("reviewed destination permissions changed");
            }
        }
        Ok(())
    }
    async fn set_permissions(&self, permissions: u32) -> Result<()> {
        if let Self::Upload { staged, .. } = self {
            staged
                .set_permissions(permissions)
                .await
                .map_err(|_| "temporary permission update unconfirmed")
        } else {
            Err("upload permissions require remote destination")
        }
    }
    async fn open(item: &SftpQueueItem, target: &SshSession) -> Result<Self> {
        let temporary = item.temporary_path()?;
        let client = target
            .open_sftp()
            .await
            .map_err(|_| "session SFTP unavailable")?;
        match item.draft.direction {
            SftpQueueDirection::Upload => Ok(Self::Upload {
                checksum_connection: target.checksum_connection(),
                checksum_ready: false,
                source: None,
                source_path: PathBuf::from(&item.draft.local_path),
                staged: client.staged_upload(),
                temporary,
                destination: item.draft.remote_path.clone(),
                backup: item.backup_path()?,
                policy: item.draft.publication_policy,
                follow_links: item.draft.follow_links,
            }),
            SftpQueueDirection::Download => {
                let mut local = LocalDownload::for_session(
                    PathBuf::from(&item.draft.local_path),
                    PathBuf::from(temporary),
                )
                .map_err(|_| "invalid local destination")?;
                local
                    .session_policy(
                        item.draft.publication_policy,
                        PathBuf::from(item.backup_path()?),
                    )
                    .map_err(|_| "invalid local backup")?;
                Ok(Self::Download {
                    source: client.pinned_read(),
                    opened: false,
                    source_path: item.draft.remote_path.clone(),
                    follow_links: item.draft.follow_links,
                    policy: item.draft.publication_policy,
                    local,
                })
            }
        }
    }
    async fn source_content(&mut self) -> Result<SftpVerifiedContent> {
        match self {
            Self::Upload {
                source,
                source_path,
                follow_links,
                ..
            } => {
                if source.is_none() {
                    let path = if *follow_links {
                        tokio::fs::canonicalize(&*source_path)
                            .await
                            .map_err(|_| "source link unavailable")?
                    } else {
                        source_path.clone()
                    };
                    *source = Some(LocalSource::open(&path).await?);
                }
                source.as_mut().ok_or("source unavailable")?.digest().await
            }
            Self::Download {
                source,
                opened,
                source_path,
                follow_links,
                ..
            } => {
                let content = if *opened {
                    source.digest().await
                } else {
                    *opened = true;
                    if *follow_links {
                        source
                            .open_follow(source_path, MAX_SFTP_VERIFIED_BYTES)
                            .await
                    } else {
                        source.open(source_path, MAX_SFTP_VERIFIED_BYTES).await
                    }
                }
                .map_err(|_| "remote source changed or unavailable")?;
                Ok(SftpVerifiedContent {
                    size: content.size,
                    sha256: content.sha256,
                })
            }
        }
    }
    async fn read(&mut self, source_read: bool, at: u64, n: usize) -> Result<Vec<u8>> {
        let mut result = Vec::with_capacity(n);
        while result.len() < n {
            let offset = at + result.len() as u64;
            let remaining = n - result.len();
            let bytes = match self {
                Self::Upload {
                    source,
                    staged,
                    temporary,
                    ..
                } => {
                    if source_read {
                        source
                            .as_mut()
                            .ok_or("source unavailable")?
                            .read(offset, remaining)
                            .await?
                    } else {
                        staged
                            .read(
                                temporary,
                                offset,
                                remaining.min(cshell_sftp::SFTP_TRANSFER_CHUNK_BYTES),
                            )
                            .await
                            .map_err(|_| "remote temporary unavailable")?
                    }
                }
                Self::Download { source, local, .. } => {
                    if source_read {
                        source
                            .read(offset, remaining)
                            .await
                            .map_err(|_| "remote source unavailable")?
                    } else {
                        local
                            .read(
                                offset,
                                remaining.min(cshell_application::UPLOAD_CHUNK_BYTES),
                            )
                            .await
                            .map_err(|_| "local temporary changed")?
                    }
                }
            };
            if bytes.is_empty() || bytes.len() > remaining {
                return Err("transfer content changed");
            }
            result.extend_from_slice(&bytes);
        }
        Ok(result)
    }
    async fn create(&mut self) -> Result<()> {
        match self {
            Self::Upload {
                staged, temporary, ..
            } => staged
                .create(temporary)
                .await
                .map_err(|_| "remote create unconfirmed"),
            Self::Download { local, .. } => {
                local.create().await.map_err(|_| "local create unconfirmed")
            }
        }
    }
    async fn reopen(&mut self) -> Result<u64> {
        match self {
            Self::Upload {
                staged, temporary, ..
            } => staged
                .reopen(temporary)
                .await
                .map_err(|_| "remote temporary unavailable"),
            Self::Download { local, .. } => local
                .reopen()
                .await
                .map_err(|_| "local temporary unavailable"),
        }
    }
    async fn write(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        self.write_buffered(at,bytes).await?;
        self.flush().await
    }
    async fn flush(&mut self)->Result<()> {
        match self {
            Self::Upload{staged,..}=>staged.flush().await.map_err(|_|"remote flush unconfirmed"),
            Self::Download{local,..}=>local.flush().await.map_err(|_|"local flush unconfirmed"),
        }
    }
    async fn write_buffered(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Upload { staged, .. } => {
                staged
                    .write(at, bytes)
                    .await
                    .map_err(|_| "remote write unconfirmed")?;
                Ok(())
            }
            Self::Download { local, .. } => {
                for (index, bytes) in bytes
                    .chunks(cshell_application::UPLOAD_CHUNK_BYTES)
                    .enumerate()
                {
                    local
                        .write(
                            at + (index * cshell_application::UPLOAD_CHUNK_BYTES) as u64,
                            bytes,
                        )
                        .await
                        .map_err(|_| "local write unconfirmed")?;
                }
                Ok(())
            }
        }
    }
    async fn verify(&mut self, maximum: u64, progress:&ReadProgress) -> Result<SftpVerifiedContent> {
        match self {
            Self::Upload {
                staged, temporary, ..
            } => {
                let c = staged
                    .verify_with_progress(temporary, maximum, progress)
                    .await
                    .map_err(|_| "remote temporary verification failed")?;
                Ok(SftpVerifiedContent {
                    size: c.size,
                    sha256: c.sha256,
                })
            }
            Self::Download { local, .. } => {
                let c = local
                    .verify(maximum)
                    .await
                    .map_err(|_| "local temporary verification failed")?;
                Ok(SftpVerifiedContent {
                    size: c.size,
                    sha256: c.sha256,
                })
            }
        }
    }
    async fn probe(
        &mut self,
        maximum: u64,
        progress:Option<&ReadProgress>,
    ) -> Result<(Option<SftpVerifiedContent>, Option<SftpVerifiedContent>)> {
        match self {
            Self::Upload {
                staged,
                temporary,
                destination,
                ..
            } => {
                // Both checks are retained. Overlap their bounded read pipelines;
                // join also drains both handles before an error is reported.
                let (t,d)=if let Some(progress)=progress {
                    let totals=Arc::new(Mutex::new([(0u64,0u64);2]));
                    let make=|index:usize|->ReadProgress {
                        let totals=totals.clone();let report=Arc::clone(progress);
                        Arc::new(move|done,total| {let mut values=totals.lock().unwrap_or_else(std::sync::PoisonError::into_inner);values[index]=(done,total);report(values[0].0+values[1].0,values[0].1+values[1].1);})
                    };
                    let a=make(0);let b=make(1);
                    tokio::join!(staged.probe_with_progress(temporary,maximum,&a),staged.probe_with_progress(destination,MAX_SFTP_VERIFIED_BYTES,&b))
                } else {
                    tokio::join!(staged.probe(temporary,maximum),staged.probe(destination,MAX_SFTP_VERIFIED_BYTES))
                };
                let t=t.map_err(|_|"remote temporary probe failed")?;
                let d=d.map_err(|_|"remote destination probe failed")?;
                Ok((
                    t.map(|c| SftpVerifiedContent {
                        size: c.size,
                        sha256: c.sha256,
                    }),
                    d.map(|c| SftpVerifiedContent {
                        size: c.size,
                        sha256: c.sha256,
                    }),
                ))
            }
            Self::Download { local, .. } => {
                let p = local
                    .probe(MAX_SFTP_VERIFIED_BYTES)
                    .await
                    .map_err(|_| "local publication probe failed")?;
                Ok((
                    p.temporary.map(|c| SftpVerifiedContent {
                        size: c.size,
                        sha256: c.sha256,
                    }),
                    p.destination.map(|c| SftpVerifiedContent {
                        size: c.size,
                        sha256: c.sha256,
                    }),
                ))
            }
        }
    }
    async fn plan(
        &mut self,
        expected: SftpVerifiedContent,
    ) -> Result<Option<FilePublicationReceipt>> {
        match self {
            Self::Upload {
                staged,
                temporary,
                destination,
                backup,
                policy,
                ..
            } => {
                let Some(policy) = policy else {
                    staged
                        .preflight(temporary, destination)
                        .await
                        .map_err(|_| "destination exists or verified publication unsupported")?;
                    return Ok(None);
                };
                let previous = staged
                    .preflight_existing(
                        temporary,
                        destination,
                        backup,
                        *policy == FilePublicationPolicy::BackupReplace,
                        MAX_SFTP_VERIFIED_BYTES,
                    )
                    .await
                    .map_err(|_| "publication conflict or unsupported server")?
                    .map(|c| UploadContent {
                        size: c.size,
                        sha256: c.sha256,
                    });
                if *policy == FilePublicationPolicy::SkipIdentical
                    && previous.is_some_and(|c| c != upload_content(expected))
                {
                    return Err("destination differs; identical-only policy refused");
                }
                Ok(Some(FilePublicationReceipt::planned(previous)))
            }
            Self::Download { local, policy, .. } => local
                .plan_publication(*policy, download_content(expected))
                .await
                .map_err(|_| "local publication conflict"),
        }
    }
    async fn publish(
        &mut self,
        planned: Option<FilePublicationReceipt>,
        expected: SftpVerifiedContent,
    ) -> Result<Option<FilePublicationReceipt>> {
        match self {
            Self::Upload {
                staged,
                temporary,
                destination,
                backup,
                policy,
                ..
            } => {
                let Some(policy) = policy else {
                    staged
                        .publish(temporary, destination)
                        .await
                        .map_err(|_| "remote publication unconfirmed; probe before recovery")?;
                    return Ok(None);
                };
                let mut receipt = planned.ok_or("missing publication plan")?;
                let previous = receipt.previous.map(|c| cshell_sftp::StagedUploadContent {
                    size: c.size,
                    sha256: c.sha256,
                });
                let (skipped, backed) = staged
                    .publish_existing(
                        temporary,
                        destination,
                        backup,
                        previous,
                        cshell_sftp::StagedUploadContent {
                            size: expected.size,
                            sha256: expected.sha256,
                        },
                        *policy == FilePublicationPolicy::BackupReplace,
                        MAX_SFTP_VERIFIED_BYTES,
                    )
                    .await
                    .map_err(|_| "remote replacement unconfirmed; probe destination and backup")?;
                receipt.skipped_identical = skipped;
                receipt.backup_verified = backed;
                Ok(Some(receipt))
            }
            Self::Download { local, policy, .. } => local
                .publish_planned(*policy, planned, download_content(expected))
                .await
                .map_err(|_| "local publication unconfirmed; probe before recovery"),
        }
    }
    async fn recover_published(
        &mut self,
        planned: Option<FilePublicationReceipt>,
        expected: SftpVerifiedContent,
    ) -> Result<Option<FilePublicationReceipt>> {
        match self {
            Self::Upload {
                staged,
                backup,
                policy,
                ..
            } => {
                let Some(policy) = policy else {
                    return Ok(None);
                };
                let mut receipt = planned.ok_or("missing publication plan")?;
                if receipt.previous == Some(upload_content(expected)) {
                    receipt.skipped_identical = true;
                    receipt.backup_verified = false;
                } else if let Some(previous) = receipt.previous {
                    if *policy != FilePublicationPolicy::BackupReplace
                        || staged
                            .probe(backup, MAX_SFTP_VERIFIED_BYTES)
                            .await
                            .map_err(|_| "backup probe failed")?
                            .map(|c| UploadContent {
                                size: c.size,
                                sha256: c.sha256,
                            })
                            != Some(previous)
                    {
                        return Err("saved backup does not match previous target");
                    }
                    receipt.backup_verified = true;
                }
                Ok(Some(receipt))
            }
            Self::Download { local, policy, .. } => local
                .recover_publication(*policy, planned, download_content(expected))
                .await
                .map_err(|_| "local backup recovery failed"),
        }
    }
    async fn cleanup(&mut self) -> Result<()> {
        match self {
            Self::Upload {
                staged, temporary, ..
            } => staged
                .cleanup(temporary)
                .await
                .map_err(|_| "remote cleanup unconfirmed"),
            Self::Download { local, .. } => local
                .cleanup()
                .await
                .map_err(|_| "local cleanup unconfirmed"),
        }
    }
    async fn close(&mut self) {
        match self {
            Self::Upload { staged, .. } => staged.close().await,
            Self::Download { source, local, .. } => {
                local.close().await;
                source.close().await;
            }
        }
    }
    fn abort_channel(&mut self) {
        match self {
            Self::Upload { staged, .. } => staged.abort(),
            Self::Download { source, .. } => source.abort(),
        }
    }
}

fn upload_content(c: SftpVerifiedContent) -> UploadContent {
    UploadContent {
        size: c.size,
        sha256: c.sha256,
    }
}
fn download_content(c: SftpVerifiedContent) -> DownloadContent {
    DownloadContent {
        size: c.size,
        sha256: c.sha256,
    }
}
struct Journal {
    control: TransferControl,
    service: SftpIpcService,
    session: SessionId,
    item: SftpQueueItemId,
    transfer: SessionId,
    draft: SftpQueueDraft,
    value: SftpVerifiedTransfer,
}
impl Journal {
    fn progress(&self,phase:SftpTransferPhase)->ReadProgress {
        let jobs=self.service.jobs.clone();let transfer=self.transfer;
        let report:ReadProgress=Arc::new(move|done,total| {
            if let Some(job)=jobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(transfer.as_uuid().as_bytes().as_slice()) {
                if job.snapshot.state==SftpTransferState::Running as i32 {
                    job.snapshot.phase=phase as i32;job.snapshot.phase_bytes=done;job.snapshot.phase_total=Some(total);
                }
            }
        });
        report(0,0);report
    }
    async fn update(
        &mut self,
        change: impl FnOnce(&mut SftpVerifiedTransfer) + Send,
    ) -> Result<()> {
        let _critical = self.control.enter_critical();
        let mut value = self.value.clone();
        change(&mut value);
        if let Some(previous) = self.value.publication {
            previous
                .validate_next(&value.publication.ok_or("publication receipt removed")?)
                .map_err(|_| "publication receipt changed")?;
        }
        let lock = self
            .service
            .queues
            .lock(self.session)
            .map_err(|_| "verified queue storage unavailable")?;
        let guard = lock.lock_owned().await;
        let mut saved = Queues::load_async(self.session)
            .await
            .map_err(|_| "verified queue storage unavailable")?;
        let row = saved
            .items
            .iter_mut()
            .find(|i| {
                i.id == self.item
                    && i.transfer_id == Some(self.transfer)
                    && i.state == SftpQueueState::Running
                    && i.draft == self.draft
            })
            .ok_or("verified queue ownership changed")?;
        if row.verified.as_ref() != Some(&self.value) {
            return Err("verified queue journal changed");
        }
        row.verified = Some(value.clone());
        saved.changed().map_err(|_| "invalid verified journal")?;
        Queues::save_owned(&saved, guard)
            .await
            .map_err(|_| "verified queue storage unavailable")?;
        self.value = value;
        let mut jobs = self
            .service
            .jobs
            .lock()
            .map_err(|_| "transfer status unavailable")?;
        if let Some(job) = jobs.get_mut(self.transfer.as_uuid().as_bytes().as_slice()) {
            job.snapshot.bytes_transferred = self.value.bytes;
            job.snapshot.total_bytes = self.value.content.map(|c| c.size);
            job.snapshot.destination_committed = self.value.committed;
        }
        Ok(())
    }
}

async fn compare_prefix(engine: &mut Engine, size: u64) -> Result<Sha256> {
    let mut hash = Sha256::new();
    let mut at = 0;
    while at < size {
        let n = (size - at).min(CHUNK as u64) as usize;
        let source = engine.read(true, at, n).await?;
        if engine.read(false, at, n).await? != source {
            return Err("temporary prefix does not match frozen source");
        }
        hash.update(source);
        at += n as u64;
    }
    Ok(hash)
}
async fn execute(engine: &mut Engine, journal: &mut Journal, action: VerifiedAction) -> Result<()> {
    if let VerifiedAction::Transfer{resume}=action {
        if journal.draft.verification.is_some() && journal.draft.publication_policy.is_none()
            && journal.draft.expected_source.is_none() && journal.draft.expected_destination.is_none() {
            return streaming::execute(engine,journal,resume).await;
        }
    }
    match action {
        VerifiedAction::Transfer { resume } => {
            let _=journal.progress(SftpTransferPhase::Preparing);
            let content = engine.source_content().await?;
            engine
                .expected_permissions(journal.draft.expected_permissions)
                .await?;
            if journal.draft.expected_source.is_some_and(|c| c != content) {
                return Err("source differs from reviewed content");
            }
            let mut hash = Sha256::new();
            if resume {
                if journal.value.content != Some(content) {
                    return Err("source differs from saved SHA-256");
                }
                let (temporary, destination) = engine.probe(content.size,None).await?;
                let previous = journal.value.publication.and_then(|p| p.previous).map(|c| {
                    SftpVerifiedContent {
                        size: c.size,
                        sha256: c.sha256,
                    }
                });
                if destination != previous {
                    return Err("destination differs from saved publication plan; resume refused");
                }
                let temporary = temporary.ok_or("owned temporary is missing")?;
                let end = journal.value.write_end.unwrap_or(journal.value.bytes);
                if temporary.size < journal.value.bytes
                    || temporary.size > end
                    || !journal.value.temporary_owned
                    || journal.value.cleaned
                    || journal.value.committed
                    || matches!(
                        journal.value.intent,
                        SftpVerifiedIntent::Publish | SftpVerifiedIntent::Cleanup
                    )
                {
                    return Err("temporary does not match saved write boundary");
                }
                if engine.reopen().await? != temporary.size {
                    return Err("temporary changed during reopen");
                }
                hash = compare_prefix(engine, temporary.size).await?;
                let digest: [u8; 32] = hash.clone().finalize().into();
                if digest != temporary.sha256 {
                    return Err("temporary changed during prefix check");
                }
                journal
                    .update(|v| {
                        v.bytes = temporary.size;
                        v.prefix_sha256 = Some(digest);
                        v.intent = SftpVerifiedIntent::None;
                        v.write_end = None;
                        v.verified = false;
                    })
                    .await?;
            } else {
                let publication = engine.plan(content).await?;
                if journal.draft.expected_destination.is_some_and(|c| {
                    publication.and_then(|p| p.previous) != Some(upload_content(c))
                }) {
                    return Err("remote edit or reviewed destination changed");
                }
                journal
                    .update(|v| {
                        v.content = Some(content);
                        v.publication = publication;
                        v.intent = SftpVerifiedIntent::Create;
                    })
                    .await?;
                engine.create().await?;
                let digest: [u8; 32] = hash.clone().finalize().into();
                journal
                    .update(|v| {
                        v.temporary_owned = true;
                        v.prefix_sha256 = Some(digest);
                        v.intent = SftpVerifiedIntent::None;
                    })
                    .await?;
            }
            let _=journal.progress(SftpTransferPhase::Transferring);
            let rate_start = tokio::time::Instant::now();
            let rate_offset = journal.value.bytes;
            while journal.value.bytes < content.size {
                let at = journal.value.bytes;
                // Keep throttled transfers responsive even at a small rate limit.
                let batch = journal
                    .draft
                    .rate_limit
                    .map(|rate| rate.max(cshell_sftp::SFTP_TRANSFER_CHUNK_BYTES as u64))
                    .unwrap_or(CHUNK as u64)
                    .min(CHUNK as u64);
                let n = (content.size - at).min(batch) as usize;
                let bytes = engine.read(true, at, n).await?;
                journal
                    .update(|v| {
                        v.intent = SftpVerifiedIntent::Write;
                        v.write_end = Some(at + n as u64);
                    })
                    .await?;
                if let Some(rate) = journal.draft.rate_limit {
                    let seconds =
                        (journal.value.bytes - rate_offset + n as u64) as f64 / rate as f64;
                    tokio::time::sleep_until(
                        rate_start + std::time::Duration::from_secs_f64(seconds),
                    )
                    .await;
                }
                engine.write(at, &bytes).await?;
                hash.update(bytes);
                let digest: [u8; 32] = hash.clone().finalize().into();
                journal
                    .update(|v| {
                        v.bytes = at + n as u64;
                        v.prefix_sha256 = Some(digest);
                        v.intent = SftpVerifiedIntent::None;
                        v.write_end = None;
                    })
                    .await?;
            }
            let progress=journal.progress(SftpTransferPhase::Verifying);
            if engine.source_content().await? != content
                || engine.verify(content.size,&progress).await? != content
            {
                return Err("full transfer SHA-256 mismatch");
            }
            journal
                .update(|v| {
                    v.verified = true;
                })
                .await?;
            if let Some(permissions) = journal.draft.destination_permissions {
                journal
                    .update(|v| {
                        v.intent = SftpVerifiedIntent::Permissions;
                    })
                    .await?;
                engine.set_permissions(permissions).await?;
                journal
                    .update(|v| {
                        v.intent = SftpVerifiedIntent::None;
                    })
                    .await?;
            }
            if let Some(seconds)=journal.draft.modified_unix_seconds {
                journal.update(|v|v.intent=SftpVerifiedIntent::Permissions).await?;
                engine.set_modified(seconds).await?;
                journal.update(|v|v.intent=SftpVerifiedIntent::None).await?;
            }
            engine
                .expected_permissions(journal.draft.expected_permissions)
                .await?;
            journal
                .update(|v| {
                    v.intent = SftpVerifiedIntent::Publish;
                })
                .await?;
            let _=journal.progress(SftpTransferPhase::Publishing);
            let publication = engine.publish(journal.value.publication, content).await?;
            journal
                .update(|v| {
                    v.publication = publication;
                    v.committed = true;
                    v.intent = SftpVerifiedIntent::None;
                })
                .await?;
            let progress=journal.progress(SftpTransferPhase::CheckingPublished);
            let (temporary, destination) = engine.probe(content.size,Some(&progress)).await?;
            if temporary.is_some_and(|c| c != content) || destination != Some(content) {
                return Err("published content changed; cleanup refused");
            }
            journal
                .update(|v| {
                    v.intent = SftpVerifiedIntent::Cleanup;
                })
                .await?;
            let _=journal.progress(SftpTransferPhase::Cleaning);
            if temporary.is_some() {
                engine.cleanup().await?;
            }
            journal
                .update(|v| {
                    v.cleaned = true;
                    v.intent = SftpVerifiedIntent::None;
                })
                .await?;
        }
        VerifiedAction::Adopt => {
            let content = journal.value.content.ok_or("missing saved SHA-256")?;
            if !journal.value.verified
                || !(journal.value.intent == SftpVerifiedIntent::Publish || journal.value.committed)
            {
                return Err("publication adoption is not eligible");
            }
            let (temporary, destination) = engine.probe(content.size,None).await?;
            if destination != Some(content) || temporary.is_some_and(|t| t != content) {
                return Err("publication does not match saved SHA-256");
            }
            let publication = engine
                .recover_published(journal.value.publication, content)
                .await?;
            journal
                .update(|v| {
                    v.publication = publication;
                    v.committed = true;
                    v.cleaned = temporary.is_none();
                    v.intent = SftpVerifiedIntent::None;
                    v.write_end = None;
                })
                .await?;
            // Adoption records matching content; remaining temporary cleanup is a separate action.
        }
        VerifiedAction::Cleanup => {
            let content = journal.value.content.ok_or("missing saved SHA-256")?;
            if !journal.value.temporary_owned
                || journal.value.cleaned
                || journal.value.intent == SftpVerifiedIntent::Publish && !journal.value.committed
            {
                return Err("cleanup is not eligible");
            }
            let (temporary, destination) = engine.probe(content.size,None).await?;
            if journal.value.committed {
                if destination != Some(content) {
                    return Err("committed destination changed; cleanup refused");
                }
            } else if destination
                != journal
                    .value
                    .publication
                    .and_then(|p| p.previous)
                    .map(|c| SftpVerifiedContent {
                        size: c.size,
                        sha256: c.sha256,
                    })
            {
                return Err("unexpected destination; cleanup refused");
            }
            if let Some(temporary) = temporary {
                let known = temporary.size == journal.value.bytes
                    && Some(temporary.sha256) == journal.value.prefix_sha256;
                if !known {
                    let end = journal.value.write_end.unwrap_or(journal.value.bytes);
                    if temporary.size < journal.value.bytes
                        || temporary.size > end
                        || engine.source_content().await? != content
                    {
                        return Err("temporary changed; cleanup refused");
                    }
                    if engine.reopen().await? != temporary.size {
                        return Err("temporary changed during reopen");
                    }
                    let hash: [u8; 32] = compare_prefix(engine, temporary.size)
                        .await?
                        .finalize()
                        .into();
                    if hash != temporary.sha256 {
                        return Err("temporary changed during cleanup check");
                    }
                }
            }
            journal
                .update(|v| {
                    v.intent = SftpVerifiedIntent::Cleanup;
                    v.write_end = None;
                    if let Some(temporary) = temporary {
                        v.bytes = temporary.size;
                        v.prefix_sha256 = Some(temporary.sha256);
                    }
                })
                .await?;
            if temporary.is_some() {
                engine.cleanup().await?;
            }
            journal
                .update(|v| {
                    v.cleaned = true;
                    v.intent = SftpVerifiedIntent::None;
                })
                .await?;
        }
    }
    Ok(())
}
impl SftpIpcService {
    pub(super) fn start_verified(
        &self,
        session: SessionId,
        target: Arc<SshSession>,
        item: SftpQueueItemId,
        transfer: SessionId,
        action: VerifiedAction,
        concurrency: usize,
    ) -> SftpResponse {
        if !target.running() {
            return SftpResponse::with_status(SftpStatus::Unavailable, "SSH session unavailable");
        }
        let control = TransferControl::default();
        let snapshot = match self.register_transfer(
            session,
            transfer.as_uuid().as_bytes().to_vec(),
            control.clone(),
            concurrency,
        ) {
            Ok(job) => job,
            Err(status) => {
                return SftpResponse::with_status(status, "SFTP capacity reached before dispatch");
            }
        };
        let service = self.clone();
        tokio::spawn(async move {
            service
                .run_verified(session, target, item, transfer, action, control)
                .await;
        });
        SftpResponse {
            transfer: Some(snapshot),
            ..Default::default()
        }
    }
    async fn run_verified(
        &self,
        session: SessionId,
        target: Arc<SshSession>,
        item: SftpQueueItemId,
        transfer: SessionId,
        action: VerifiedAction,
        control: TransferControl,
    ) {
        let result = async {
            let saved = Queues::load_async(session).await.map_err(|_| "verified queue storage unavailable")?;
            let item = saved.items.iter().find(|i| i.id == item && i.state == SftpQueueState::Running && i.transfer_id == Some(transfer)).ok_or("verified queue ownership changed")?;
            let value = item.verified.clone().ok_or("missing verified journal")?;
            let mut journal = Journal { control: control.clone(), service: self.clone(), session, item: item.id, transfer, draft: item.draft.clone(), value };
            if control.is_cancelled() { return Err("verified transfer cancelled before SFTP open"); }
            let peer=target.clipboard_trust().cloned();
            let lock_control=control.clone();
            let protection=async { if let Some(peer)=&peer { let local=self.namespaces.scope(true,peer)?.read_owned().await;let remote=self.namespaces.scope(false,peer)?.read_owned().await;let destination=self.namespaces.destination(&item.draft,peer).await?;Ok(Some((local,remote,destination))) }else{Ok::<_,&'static str>(None)} };
            let _protection=tokio::select!{_ = async{loop{if lock_control.is_cancelled(){break;}tokio::time::sleep(std::time::Duration::from_millis(100)).await;}}=>return Err("transfer cancelled while waiting for destination"),result=protection=>result?};
            let mut engine = tokio::time::timeout(std::time::Duration::from_secs(30), Engine::open(item, &target)).await.map_err(|_| "session SFTP open timed out")??;
            let outcome = tokio::select! { biased;
                _ = async { loop { if control.is_cancelled() { break; } tokio::time::sleep(std::time::Duration::from_millis(100)).await; } } => Err("verified transfer cancelled; inspect before recovery"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)) => Err("verified transfer deadline exceeded; inspect before recovery"),
                result = control.run(execute(&mut engine, &mut journal, action)) => result,
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), engine.close()).await;
            engine.abort_channel();
            outcome
        }.await;
        // Report only the last durable facts, including when a post-mutation save failed.
        let stored = Queues::load_async(session).await.ok().and_then(|q| {
            q.items
                .into_iter()
                .find(|i| i.id == item && i.transfer_id == Some(transfer))
        });
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(job) = jobs.get_mut(transfer.as_uuid().as_bytes().as_slice()) {
            if let Some(row) = stored
                && let Some(v) = &row.verified
            {
                job.snapshot.bytes_transferred = v.bytes;
                job.snapshot.total_bytes = v.content.map(|c| c.size);
                job.snapshot.destination_committed = v.committed;
                job.snapshot.cleanup_path = if v.temporary_owned && !v.cleaned {
                    job.snapshot.cleanup_location =
                        if row.draft.direction == SftpQueueDirection::Upload {
                            SftpCleanupLocation::Remote as i32
                        } else {
                            SftpCleanupLocation::DaemonLocal as i32
                        };
                    row.temporary_path().ok()
                } else {
                    None
                };
                job.snapshot.state = if v.committed && v.cleaned {
                    SftpTransferState::Succeeded as i32
                } else if control.is_cancelled() {
                    SftpTransferState::Cancelled as i32
                } else {
                    SftpTransferState::Failed as i32
                };
            } else {
                job.snapshot.state = SftpTransferState::Failed as i32;
            }
            job.snapshot.detail = if job.snapshot.state == SftpTransferState::Succeeded as i32 {
                String::new()
            } else {
                result
                    .err()
                    .unwrap_or("explicit recovery completed; reload queue for durable state")
                    .into()
            };
        }
    }
}
