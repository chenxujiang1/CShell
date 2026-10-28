//! Local client runtime helper. Call on a background worker, never the UI thread.
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadSourceFingerprint {
    pub size: u64,
    pub sha256: [u8; 32],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UploadSourceError {
    #[error("upload source must be an absolute regular file")]
    Invalid,
    #[error("upload source is unavailable")]
    Unavailable,
    #[error("upload source changed while hashing")]
    Changed,
    #[error("upload source exceeds the size limit")]
    Limit,
    #[error("upload source hashing cancelled")]
    Cancelled,
}
pub fn fingerprint_upload_source(
    path: &Path,
    maximum: u64,
    cancelled: &AtomicBool,
) -> Result<UploadSourceFingerprint, UploadSourceError> {
    if !path.is_absolute() {
        return Err(UploadSourceError::Invalid);
    }
    let before = std::fs::symlink_metadata(path).map_err(|_| UploadSourceError::Unavailable)?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(UploadSourceError::Invalid);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if before.file_attributes() & 0x400 != 0 {
            return Err(UploadSourceError::Invalid);
        }
    }
    if before.len() > maximum {
        return Err(UploadSourceError::Limit);
    }
    let mut file = std::fs::File::open(path).map_err(|_| UploadSourceError::Unavailable)?;
    let opened = file
        .metadata()
        .map_err(|_| UploadSourceError::Unavailable)?;
    if !opened.is_file()
        || before.len() != opened.len()
        || before.modified().ok() != opened.modified().ok()
    {
        return Err(UploadSourceError::Changed);
    }
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0; 32 * 1024];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(UploadSourceError::Cancelled);
        }
        let read = file
            .read(&mut buffer)
            .map_err(|_| UploadSourceError::Unavailable)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or(UploadSourceError::Limit)?;
        if size > maximum {
            return Err(UploadSourceError::Limit);
        }
        if size > before.len() {
            return Err(UploadSourceError::Changed);
        }
        hash.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|_| UploadSourceError::Unavailable)?;
    let named = std::fs::symlink_metadata(path).map_err(|_| UploadSourceError::Changed)?;
    if size != before.len()
        || after.len() != size
        || after.modified().ok() != before.modified().ok()
        || !named.is_file()
        || named.file_type().is_symlink()
        || named.len() != size
        || named.modified().ok() != after.modified().ok()
    {
        return Err(UploadSourceError::Changed);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(UploadSourceError::Cancelled);
    }
    Ok(UploadSourceFingerprint {
        size,
        sha256: hash.finalize().into(),
    })
}
