//! Client-side local publication. Export paths never cross the IPC boundary.
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExportFileError {
    #[error("export requires an absolute destination file path")]
    Invalid,
    #[error("export destination already exists; choose a new file")]
    Exists,
    #[error("export staging failed")]
    Staging,
    #[error("export publication is uncertain; inspect the destination before retrying")]
    Uncertain,
    #[error("export was published, but sync or cleanup failed; inspect it before retrying")]
    Published,
}
/// Staged streaming publication. The destination remains absent until publish.
pub struct ExportStaging {
    destination: PathBuf,
    file: Option<tempfile::NamedTempFile>,
    staging: tempfile::TempDir,
}
impl ExportStaging {
    pub fn prepare(path: &Path) -> Result<Self, ExportFileError> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(ExportFileError::Invalid);
        }
        let parent = path.parent().ok_or(ExportFileError::Invalid)?;
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(ExportFileError::Exists),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(ExportFileError::Staging),
        }
        let staging = tempfile::Builder::new()
            .prefix(".cshell-result-")
            .tempdir_in(parent)
            .map_err(|_| ExportFileError::Staging)?;
        crate::RuntimePaths::prepare(staging.path().to_owned())
            .map_err(|_| ExportFileError::Staging)?;
        let file = tempfile::NamedTempFile::new_in(staging.path())
            .map_err(|_| ExportFileError::Staging)?;
        Ok(Self {
            destination: path.to_owned(),
            staging,
            file: Some(file),
        })
    }
    pub fn file_mut(&mut self) -> Result<&mut std::fs::File, ExportFileError> {
        self.file
            .as_mut()
            .map(tempfile::NamedTempFile::as_file_mut)
            .ok_or(ExportFileError::Staging)
    }
    pub fn publish(mut self) -> Result<(), ExportFileError> {
        let file = self.file.take().ok_or(ExportFileError::Staging)?;
        file.as_file()
            .sync_all()
            .map_err(|_| ExportFileError::Staging)?;
        let published = file.persist_noclobber(&self.destination).map_err(|error| {
            if error.error.kind() == io::ErrorKind::AlreadyExists {
                ExportFileError::Exists
            } else {
                ExportFileError::Uncertain
            }
        })?;
        drop(published);
        #[cfg(unix)]
        std::fs::File::open(self.destination.parent().ok_or(ExportFileError::Invalid)?)
            .and_then(|file| file.sync_all())
            .map_err(|_| ExportFileError::Published)?;
        self.staging
            .close()
            .map_err(|_| ExportFileError::Published)?;
        Ok(())
    }
}
/// Cancel can win only before the final publication decision.
#[derive(Debug, Default)]
pub struct ExportTransferControl {
    phase: AtomicU8,
    bytes: AtomicU64,
}
impl ExportTransferControl {
    pub fn request_cancel(&self) -> bool {
        self.phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn cancelled(&self) -> bool {
        self.phase.load(Ordering::Acquire) == 1
    }
    pub fn can_cancel(&self) -> bool {
        self.phase.load(Ordering::Acquire) == 0
    }
    pub fn begin_publication(&self) -> bool {
        self.phase
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn finish(&self) {
        self.phase.store(3, Ordering::Release);
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Acquire)
    }
    pub fn record_bytes(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Release);
    }
}
pub fn publish_export_file(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> Result<(), ExportFileError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(ExportFileError::Invalid);
    }
    let parent = path.parent().ok_or(ExportFileError::Invalid)?;
    let staging = tempfile::Builder::new()
        .prefix(".cshell-result-")
        .tempdir_in(parent)
        .map_err(|_| ExportFileError::Staging)?;
    let mut file =
        tempfile::NamedTempFile::new_in(staging.path()).map_err(|_| ExportFileError::Staging)?;
    write(file.as_file_mut())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|_| ExportFileError::Staging)?;
    let published = file.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == io::ErrorKind::AlreadyExists {
            ExportFileError::Exists
        } else {
            ExportFileError::Uncertain
        }
    })?;
    drop(published);
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|_| ExportFileError::Published)?;
    staging.close().map_err(|_| ExportFileError::Published)?;
    Ok(())
}
/// A private flat directory export. Reservation is exclusive; publication can be
/// partially visible on failure, reported as Published, and is never auto-replayed.
pub struct ExportDirectoryStaging {
    destination: PathBuf,
    directory: tempfile::TempDir,
}
impl ExportDirectoryStaging {
    pub fn prepare(path: &Path) -> Result<Self, ExportFileError> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(ExportFileError::Invalid);
        }
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(ExportFileError::Exists),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(ExportFileError::Staging),
        }
        let directory = tempfile::Builder::new()
            .prefix(".cshell-extract-")
            .tempdir_in(path.parent().ok_or(ExportFileError::Invalid)?)
            .map_err(|_| ExportFileError::Staging)?;
        crate::RuntimePaths::prepare(directory.path().to_owned())
            .map_err(|_| ExportFileError::Staging)?;
        Ok(Self {
            destination: path.to_owned(),
            directory,
        })
    }
    pub fn create(&self, name: &str) -> Result<std::fs::File, ExportFileError> {
        if name.is_empty()
            || name.len() > 240
            || name == "."
            || name == ".."
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || name.ends_with('.')
        {
            return Err(ExportFileError::Invalid);
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(self.directory.path().join(name))
            .map_err(|_| ExportFileError::Staging)
    }
    pub fn publish(self) -> Result<(), ExportFileError> {
        std::fs::create_dir(&self.destination).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                ExportFileError::Exists
            } else {
                ExportFileError::Uncertain
            }
        })?;
        crate::RuntimePaths::prepare(self.destination.clone())
            .map_err(|_| ExportFileError::Published)?;
        let mut count = 0;
        for entry in
            std::fs::read_dir(self.directory.path()).map_err(|_| ExportFileError::Published)?
        {
            let entry = entry.map_err(|_| ExportFileError::Published)?;
            count += 1;
            if count > 12_300 {
                return Err(ExportFileError::Published);
            }
            let meta =
                std::fs::symlink_metadata(entry.path()).map_err(|_| ExportFileError::Published)?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(ExportFileError::Published);
            }
            let file = std::fs::File::open(entry.path()).map_err(|_| ExportFileError::Published)?;
            file.sync_all().map_err(|_| ExportFileError::Published)?;
            std::fs::hard_link(entry.path(), self.destination.join(entry.file_name()))
                .map_err(|_| ExportFileError::Published)?;
        }
        #[cfg(unix)]
        {
            std::fs::File::open(&self.destination)
                .and_then(|f| f.sync_all())
                .map_err(|_| ExportFileError::Published)?;
            std::fs::File::open(
                self.destination
                    .parent()
                    .ok_or(ExportFileError::Published)?,
            )
            .and_then(|f| f.sync_all())
            .map_err(|_| ExportFileError::Published)?;
        }
        self.directory
            .close()
            .map_err(|_| ExportFileError::Published)?;
        Ok(())
    }
}
