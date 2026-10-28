//! Native no-follow opens. Unix data opens are relative to pinned directory FDs;
//! Windows handles deny write/delete sharing, including pinned ancestors.
#![allow(unsafe_code)]
use crate::{ForeignBatchError, ForeignIntakeError as Error, valid_foreign_component};
use std::{
    collections::BTreeSet,
    fs::{File, Metadata},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub(super) struct Directory {
    file: Arc<File>,
    path: PathBuf,
    ancestors: Vec<Arc<File>>,
}
impl Directory {
    pub(super) fn root(path: &Path) -> Result<Self, Error> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| Error::Unavailable)?
                .join(path)
        };
        let mut anchor = PathBuf::new();
        let mut current: Option<Self> = None;
        for part in absolute.components() {
            match part {
                Component::Prefix(prefix) => {
                    #[cfg(windows)]
                    if !matches!(
                        prefix.kind(),
                        std::path::Prefix::Disk(_)
                            | std::path::Prefix::VerbatimDisk(_)
                            | std::path::Prefix::UNC(_, _)
                            | std::path::Prefix::VerbatimUNC(_, _)
                    ) {
                        return Err(Error::Invalid);
                    }
                    anchor.push(prefix.as_os_str());
                }
                Component::RootDir => {
                    anchor.push(part.as_os_str());
                    let file = open_absolute(&anchor, true)?;
                    inspect(&file.metadata().map_err(|_| Error::Unavailable)?, true)?;
                    current = Some(Self {
                        file: Arc::new(file),
                        path: anchor.clone(),
                        ancestors: vec![],
                    });
                }
                Component::Normal(name) => {
                    let name = name
                        .to_str()
                        .filter(|s| valid_foreign_component(s))
                        .ok_or(Error::Invalid)?;
                    current = Some(current.ok_or(Error::Invalid)?.child(name)?);
                }
                Component::CurDir => {}
                Component::ParentDir => return Err(Error::Invalid),
            }
        }
        current.ok_or(Error::Invalid)
    }
    fn open(&self, name: &str, directory: bool) -> Result<File, Error> {
        if !valid_foreign_component(name) {
            return Err(Error::Invalid);
        }
        let file = open_child(&self.file, &self.path.join(name), name, directory)?;
        inspect(&file.metadata().map_err(|_| Error::Unavailable)?, directory)?;
        Ok(file)
    }
    pub(super) fn open_file(&self, name: &str) -> Result<File, Error> {
        self.open(name, false)
    }
    pub(super) fn child(&self, name: &str) -> Result<Self, Error> {
        let file = self.open(name, true)?;
        let mut ancestors = self.ancestors.clone();
        ancestors.push(self.file.clone());
        Ok(Self {
            file: Arc::new(file),
            path: self.path.join(name),
            ancestors,
        })
    }
    pub(super) fn is_directory(&self, name: &str) -> Result<bool, Error> {
        // This inspection does not read file contents. Actual reads re-open relative to the pinned parent.
        let metadata =
            std::fs::symlink_metadata(self.path.join(name)).map_err(|_| Error::Unavailable)?;
        reject_link(&metadata)?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(Error::Invalid);
        }
        Ok(metadata.is_dir())
    }
    pub(super) fn names(
        &self,
        maximum: usize,
        check: impl Fn() -> Result<(), Error>,
    ) -> Result<Vec<String>, Error> {
        self.verify()?;
        let entries = std::fs::read_dir(&self.path).map_err(|_| Error::Unavailable)?;
        let mut names = vec![];
        let mut folded = BTreeSet::new();
        for entry in entries {
            check()?;
            if names.len() >= maximum {
                return Err(ForeignBatchError::Limit.into());
            }
            let name = entry
                .map_err(|_| Error::Unavailable)?
                .file_name()
                .into_string()
                .map_err(|_| Error::Invalid)?;
            if !valid_foreign_component(&name) || !folded.insert(name.to_lowercase()) {
                return Err(Error::Invalid);
            }
            names.push(name);
        }
        self.verify()?;
        names.sort();
        Ok(names)
    }
    pub(super) fn verify(&self) -> Result<(), Error> {
        let pinned = self.file.metadata().map_err(|_| Error::Unavailable)?;
        let named = std::fs::symlink_metadata(&self.path).map_err(|_| Error::Changed)?;
        reject_link(&named)?;
        if !named.is_dir() || !same_object(&pinned, &named) {
            return Err(Error::Changed);
        }
        Ok(())
    }
    pub(super) fn verify_file(&self, name: &str, file: &File) -> Result<(), Error> {
        let named = self.open_file(name)?;
        if !unchanged(
            &file.metadata().map_err(|_| Error::Unavailable)?,
            &named.metadata().map_err(|_| Error::Changed)?,
        ) {
            return Err(Error::Changed);
        }
        self.verify()
    }
}
fn reject_link(metadata: &Metadata) -> Result<(), Error> {
    if metadata.file_type().is_symlink() {
        return Err(Error::Invalid);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}
fn inspect(metadata: &Metadata, directory: bool) -> Result<(), Error> {
    reject_link(metadata)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(Error::Invalid);
    }
    Ok(())
}
pub(super) fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    same_object(a, b) && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}
#[cfg(unix)]
fn same_object(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
fn same_object(a: &Metadata, b: &Metadata) -> bool {
    // Pinned handles prevent write/reparse mutation and rename/replacement.
    use std::os::windows::fs::MetadataExt;
    a.creation_time() == b.creation_time() && a.file_attributes() == b.file_attributes()
}
#[cfg(not(any(unix, windows)))]
fn same_object(_: &Metadata, _: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn open_absolute(path: &Path, directory: bool) -> Result<File, Error> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
        .open(path)
        .map_err(|_| Error::Unavailable)
}
#[cfg(unix)]
fn open_child(parent: &File, _: &Path, name: &str, directory: bool) -> Result<File, Error> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name).map_err(|_| Error::Invalid)?;
    // SAFETY: borrowed parent FD, validated single component, owned returned FD.
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if raw < 0 {
        return Err(Error::Unavailable);
    }
    Ok(unsafe { File::from_raw_fd(raw) })
}
#[cfg(windows)]
fn open_absolute(path: &Path, directory: bool) -> Result<File, Error> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::*;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        )
        .open(path)
        .map_err(|_| Error::Unavailable)
}
#[cfg(windows)]
fn open_child(_: &File, path: &Path, _: &str, directory: bool) -> Result<File, Error> {
    open_absolute(path, directory)
}
#[cfg(not(any(unix, windows)))]
fn open_absolute(_: &Path, _: bool) -> Result<File, Error> {
    Err(Error::Invalid)
}
#[cfg(not(any(unix, windows)))]
fn open_child(_: &File, _: &Path, _: &str, _: bool) -> Result<File, Error> {
    Err(Error::Invalid)
}
