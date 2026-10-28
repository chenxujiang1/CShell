//! New no-replace transfers: one source pass, bounded durable checkpoints.
//! Recovery still reads and compares the saved prefix before any further writes.
use super::*;
use cshell_domain::SftpVerification;

impl Engine {
    async fn source_size(&mut self) -> Result<u64> {
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
                let source = source.as_ref().ok_or("source unavailable")?;
                source.check().await?;
                Ok(source.identity.len())
            }
            Self::Download {
                source,
                source_path,
                opened,
                follow_links,
                ..
            } => {
                let size = source
                    .open_size(source_path, MAX_SFTP_VERIFIED_BYTES, *follow_links)
                    .await
                    .map_err(|_| "remote source unavailable")?;
                *opened = true;
                Ok(size)
            }
        }
    }
    async fn source_unchanged(&mut self, size: u64) -> Result<()> {
        match self {
            Self::Upload { source, .. } => {
                let source = source.as_mut().ok_or("source unavailable")?;
                source.check().await?;
                if source.identity.len() != size || !source.read(size, 1).await?.is_empty() {
                    return Err("source changed during transfer");
                }
                Ok(())
            }
            Self::Download { source, .. } => {
                source
                    .check_metadata()
                    .await
                    .map_err(|_| "remote source changed")?;
                if !source
                    .read(size, 1)
                    .await
                    .map_err(|_| "remote source changed")?
                    .is_empty()
                {
                    return Err("remote source changed");
                }
                Ok(())
            }
        }
    }
    async fn prepare_checksum(&mut self) -> Result<()> {
        if let Self::Upload {
            staged,
            temporary,
            checksum_connection,
            checksum_ready,
            ..
        } = self
        {
            if staged.supports_server_checksum() {
                return Ok(());
            }
            if let Some(connection) = checksum_connection {
                // Servers that cannot truncate the held file can still use the
                // single-read fallback without ever receiving challenge bytes.
                if staged.reset_empty().await.is_err() {
                    staged
                        .check_size(temporary, 0)
                        .await
                        .map_err(|_| "new staging file is not empty")?;
                    return Ok(());
                }
                // This happens inside the durable Create intent, before ownership is
                // acknowledged. No extra probe file or user data is left behind.
                let nonce = SessionId::new().to_string();
                staged
                    .write(0, nonce.as_bytes())
                    .await
                    .map_err(|_| "checksum namespace probe write unconfirmed")?;
                *checksum_ready = connection
                    .file_checksum_probe(temporary, nonce.as_bytes())
                    .await;
                staged
                    .reset_empty()
                    .await
                    .map_err(|_| "checksum namespace probe reset unconfirmed")?;
                staged
                    .check_size(temporary, 0)
                    .await
                    .map_err(|_| "checksum namespace probe reset failed")?;
            }
        }
        Ok(())
    }
    async fn check_sizes(&mut self, expected: u64, published: bool) -> Result<()> {
        match self {
            Self::Upload {
                staged,
                temporary,
                destination,
                ..
            } => {
                staged
                    .check_size(temporary, expected)
                    .await
                    .map_err(|_| "remote temporary size changed")?;
                if published
                    && staged
                        .size(destination)
                        .await
                        .map_err(|_| "remote destination unavailable")?
                        != Some(expected)
                {
                    return Err("published size mismatch");
                }
                Ok(())
            }
            Self::Download { local, .. } => {
                if !published {
                    return local
                        .verify_size(expected)
                        .await
                        .map_err(|_| "local temporary size changed");
                }
                let (temporary, destination) = local
                    .probe_sizes()
                    .await
                    .map_err(|_| "local publication unavailable")?;
                if temporary != Some(expected) || destination != Some(expected) {
                    return Err("published size mismatch");
                }
                Ok(())
            }
        }
    }
    async fn verify_once(
        &mut self,
        expected: SftpVerifiedContent,
        journal: &Journal,
    ) -> Result<()> {
        let digest = match self {
            Self::Upload {
                staged,
                temporary,
                checksum_connection,
                checksum_ready,
                ..
            } => {
                let extension = if staged.supports_server_checksum() {
                    let _ = journal.progress(SftpTransferPhase::ServerChecksum);
                    staged.server_checksum().await
                } else {
                    None
                };
                if extension.is_some() {
                    extension
                } else if *checksum_ready {
                    let _ = journal.progress(SftpTransferPhase::ServerChecksum);
                    match checksum_connection {
                        Some(connection) => connection.file_sha256(temporary).await,
                        None => None,
                    }
                } else {
                    None
                }
            }
            Self::Download { .. } => None,
        };
        if let Some(digest) = digest {
            if digest != expected.sha256 {
                return Err("server SHA-256 mismatch");
            }
            return Ok(());
        }
        // Complete verification remains mandatory if a command/extension is unavailable.
        let progress = journal.progress(SftpTransferPhase::Verifying);
        progress(0, expected.size);
        if self.verify(expected.size, &progress).await? != expected {
            return Err("full transfer SHA-256 mismatch");
        }
        Ok(())
    }
}
impl Journal {
    fn transferred(&self, bytes: u64, total: u64) {
        if let Some(job) = self
            .service
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(self.transfer.as_uuid().as_bytes().as_slice())
        {
            job.snapshot.bytes_transferred = bytes;
            job.snapshot.total_bytes = Some(total);
        }
    }
}
pub(super) async fn execute(
    engine: &mut Engine,
    journal: &mut Journal,
    resume: bool,
) -> Result<()> {
    let mode = journal
        .draft
        .verification
        .ok_or("missing transfer policy")?;
    let _ = journal.progress(SftpTransferPhase::Preparing);
    let size = engine.source_size().await?;
    engine
        .expected_permissions(journal.draft.expected_permissions)
        .await?;
    let mut hash = Sha256::new();
    if resume {
        // A crashed streaming transfer may not have a final source digest yet.
        // Freeze the current source and compare every byte already written.
        let content = engine.source_content().await?;
        let saved = journal.value.content.ok_or("missing saved source")?;
        if saved.size != content.size || !journal.value.digest_pending && saved != content {
            return Err("source differs from saved content");
        }
        let (temporary, destination) = engine.probe(content.size, None).await?;
        if destination.is_some() {
            return Err("destination exists; resume refused");
        }
        let temporary = temporary.ok_or("owned temporary is missing")?;
        let end = journal.value.write_end.unwrap_or(journal.value.bytes);
        if !journal.value.temporary_owned
            || journal.value.cleaned
            || journal.value.committed
            || matches!(
                journal.value.intent,
                SftpVerifiedIntent::Publish | SftpVerifiedIntent::Cleanup
            )
            || temporary.size < journal.value.bytes
            || temporary.size > end
        {
            return Err("temporary does not match saved write boundary");
        }
        if engine.reopen().await? != temporary.size {
            return Err("temporary changed during reopen");
        }
        hash = compare_prefix(engine, journal.value.bytes).await?;
        if Some(hash.clone().finalize().into()) != journal.value.prefix_sha256 {
            return Err("source prefix differs from saved checkpoint");
        }
        let mut checked = journal.value.bytes;
        while checked < temporary.size {
            let n = (temporary.size - checked).min(CHUNK as u64) as usize;
            let bytes = engine.read(true, checked, n).await?;
            if engine.read(false, checked, n).await? != bytes {
                return Err("unconfirmed write differs from source");
            }
            hash.update(&bytes);
            checked += n as u64;
        }
        let digest: [u8; 32] = hash.clone().finalize().into();
        if digest != temporary.sha256 {
            return Err("temporary changed during prefix check");
        }
        engine.flush().await?;
        journal
            .update(|v| {
                v.content = Some(content);
                v.digest_pending = false;
                v.bytes = temporary.size;
                v.prefix_sha256 = Some(digest);
                v.intent = SftpVerifiedIntent::None;
                v.write_end = None;
                v.verified = false;
            })
            .await?;
    } else {
        // The placeholder digest is explicitly marked pending and cannot be published/adopted.
        let content = SftpVerifiedContent {
            size,
            sha256: [0; 32],
        };
        engine.plan(content).await?;
        journal
            .update(|v| {
                v.content = Some(content);
                v.digest_pending = true;
                v.intent = SftpVerifiedIntent::Create;
            })
            .await?;
        engine.create().await?;
        if mode == SftpVerification::Full {
            engine.prepare_checksum().await?;
        }
        let empty: [u8; 32] = hash.clone().finalize().into();
        journal
            .update(|v| {
                v.temporary_owned = true;
                v.prefix_sha256 = Some(empty);
                v.intent = SftpVerifiedIntent::None;
            })
            .await?;
    }
    let _ = journal.progress(SftpTransferPhase::Transferring);
    let start = tokio::time::Instant::now();
    let offset = journal.value.bytes;
    let mut at = offset;
    while at < size {
        let end = (at + MAX_SFTP_CHECKPOINT_BYTES).min(size);
        journal
            .update(|v| {
                v.intent = SftpVerifiedIntent::Write;
                v.write_end = Some(end);
            })
            .await?;
        let checkpoint = tokio::time::Instant::now();
        while at < end {
            let batch = journal
                .draft
                .rate_limit
                .map(|r| r.max(cshell_sftp::SFTP_TRANSFER_CHUNK_BYTES as u64))
                .unwrap_or(CHUNK as u64)
                .min(CHUNK as u64);
            let n = (end - at).min(batch) as usize;
            let bytes = engine.read(true, at, n).await?;
            if let Some(rate) = journal.draft.rate_limit {
                tokio::time::sleep_until(
                    start
                        + std::time::Duration::from_secs_f64(
                            (at - offset + n as u64) as f64 / rate as f64,
                        ),
                )
                .await;
            }
            engine.write_buffered(at, &bytes).await?;
            hash.update(&bytes);
            at += bytes.len() as u64;
            journal.transferred(at, size);
            if checkpoint.elapsed() >= std::time::Duration::from_secs(2) {
                break;
            }
        }
        engine.flush().await?;
        let digest: [u8; 32] = hash.clone().finalize().into();
        journal
            .update(|v| {
                v.bytes = at;
                v.prefix_sha256 = Some(digest);
                v.intent = SftpVerifiedIntent::None;
                v.write_end = None;
            })
            .await?;
    }
    engine.source_unchanged(size).await?;
    engine.flush().await?;
    let content = SftpVerifiedContent {
        size,
        sha256: hash.finalize().into(),
    };
    if !journal.value.digest_pending && journal.value.content != Some(content) {
        return Err("source changed during resumed transfer");
    }
    journal
        .update(|v| {
            v.content = Some(content);
            v.digest_pending = false;
        })
        .await?;
    engine.check_sizes(size, false).await?;
    if mode == SftpVerification::Full {
        engine.verify_once(content, journal).await?;
        engine.check_sizes(size, false).await?;
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
    let _ = journal.progress(SftpTransferPhase::Publishing);
    engine.publish(None, content).await?;
    journal
        .update(|v| {
            v.committed = true;
            v.intent = SftpVerifiedIntent::None;
        })
        .await?;
    let _ = journal.progress(SftpTransferPhase::CheckingPublished);
    engine.check_sizes(size, true).await?;
    journal
        .update(|v| {
            v.intent = SftpVerifiedIntent::Cleanup;
        })
        .await?;
    let _ = journal.progress(SftpTransferPhase::Cleaning);
    engine.cleanup().await?;
    journal
        .update(|v| {
            v.cleaned = true;
            v.intent = SftpVerifiedIntent::None;
        })
        .await?;
    Ok(())
}
