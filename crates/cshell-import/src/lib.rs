//! Explicit, read-only foreign-session intake. Never discovers vendor defaults.
//! Call from a bounded background owner; cancellation does not abort a blocked OS read.
mod source;
use cshell_application::*;
use source::Directory;
use std::{
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignBatchInput {
    SecureCrtDirectory,
    XshellDirectory,
    Manifest,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ForeignIntakeError {
    #[error(
        "migration source is unavailable; close the source application and select a stable local backup"
    )]
    Unavailable,
    #[error(
        "migration accepts regular files and directories only; links, reparse points and ambiguous paths are refused"
    )]
    Invalid,
    #[error(
        "migration source changed during intake; select a stable local backup and preview again"
    )]
    Changed,
    #[error("migration intake cancelled or exceeded its 30 second read budget")]
    Cancelled,
    #[error(transparent)]
    Batch(#[from] ForeignBatchError),
    #[error(
        "migration input #{input_number} in deterministic traversal order was refused: {reason}"
    )]
    Source {
        input_number: usize,
        reason: ForeignBatchError,
    },
}
struct Intake<'a> {
    cancelled: &'a AtomicBool,
    started: Instant,
    counts: ForeignBatchCounts,
    builder: ForeignBatchBuilder,
}
impl Intake<'_> {
    fn check(&self) -> Result<(), ForeignIntakeError> {
        if self.cancelled.load(Ordering::Acquire)
            || self.started.elapsed() > Duration::from_secs(30)
        {
            return Err(ForeignIntakeError::Cancelled);
        }
        Ok(())
    }
    fn read(
        &mut self,
        directory: &Directory,
        name: &str,
    ) -> Result<Zeroizing<Vec<u8>>, ForeignIntakeError> {
        self.check()?;
        let mut file = directory.open_file(name)?;
        let before = file
            .metadata()
            .map_err(|_| ForeignIntakeError::Unavailable)?;
        if before.len() > MAX_PROFILE_IMPORT_BYTES as u64 {
            return Err(ForeignBatchError::Limit.into());
        }
        let mut bytes = Zeroizing::new(Vec::new());
        let mut chunk = Zeroizing::new([0u8; 16384]);
        loop {
            self.check()?;
            let read = file
                .read(&mut chunk[..])
                .map_err(|_| ForeignIntakeError::Unavailable)?;
            if read == 0 {
                break;
            }
            if bytes.len() + read > MAX_PROFILE_IMPORT_BYTES
                || self.counts.input_bytes + read > MAX_FOREIGN_INPUT_BYTES
            {
                return Err(ForeignBatchError::Limit.into());
            }
            bytes.extend_from_slice(&chunk[..read]);
            self.counts.input_bytes += read;
        }
        let after = file
            .metadata()
            .map_err(|_| ForeignIntakeError::Unavailable)?;
        if bytes.len() as u64 != before.len() || !source::unchanged(&before, &after) {
            return Err(ForeignIntakeError::Changed);
        }
        directory.verify_file(name, &file)?;
        self.check()?;
        Ok(bytes)
    }
    fn convert(
        &mut self,
        directory: &Directory,
        name: &str,
        format: ForeignSessionFormat,
        folder: &[String],
        account: &str,
    ) -> Result<(), ForeignIntakeError> {
        self.check()?;
        self.counts.source_files += 1;
        if self.counts.source_files > MAX_FOREIGN_SOURCE_FILES {
            return Err(ForeignBatchError::Limit.into());
        }
        let bytes = self.read(directory, name)?;
        let stem = Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or(ForeignIntakeError::Invalid)?;
        let input_number = self.counts.source_files;
        let converted =
            format
                .convert(&bytes, stem, account)
                .map_err(|reason| ForeignIntakeError::Source {
                    input_number,
                    reason,
                })?;
        if let Some(converted) = converted {
            self.builder
                .push(converted, folder)
                .map_err(|reason| ForeignIntakeError::Source {
                    input_number,
                    reason,
                })?;
        } else {
            self.counts.non_ssh_files_skipped += 1;
        }
        self.check()
    }
    fn directory(
        &mut self,
        directory: &Directory,
        format: ForeignSessionFormat,
        folder: &mut Vec<String>,
        account: &str,
    ) -> Result<(), ForeignIntakeError> {
        self.check()?;
        let names = directory.names(
            MAX_FOREIGN_DIRECTORY_ENTRIES.saturating_sub(self.counts.directory_entries),
            || self.check(),
        )?;
        self.counts.directory_entries += names.len();
        for name in names {
            self.check()?;
            // Inspect every entry, including unrelated extensions, before deciding to skip it.
            if directory.is_directory(&name)? {
                if folder.len() >= MAX_FOREIGN_DEPTH {
                    return Err(ForeignBatchError::Limit.into());
                }
                let child = directory.child(&name)?;
                folder.push(name);
                self.directory(&child, format, folder, account)?;
                folder.pop();
                continue;
            }
            let extension = Path::new(&name)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let expected = if format == ForeignSessionFormat::SecureCrtIni {
                "ini"
            } else {
                "xsh"
            };
            if !extension.eq_ignore_ascii_case(expected) {
                self.counts.ignored_files += 1;
                continue;
            }
            if is_default(&name, format) {
                self.counts.defaults_skipped += 1;
                continue;
            }
            self.convert(directory, &name, format, folder, account)?;
        }
        directory.verify()?;
        Ok(())
    }
}
fn is_default(name: &str, format: ForeignSessionFormat) -> bool {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let defaults: &[&str] = if format == ForeignSessionFormat::SecureCrtIni {
        &["default", "global", "__folderdata__"]
    } else {
        &["default", "default session", "default settings", "global"]
    };
    defaults.iter().any(|v| stem.eq_ignore_ascii_case(v))
}

pub fn load_foreign_batch(
    path: &Path,
    input: ForeignBatchInput,
    default_username: &str,
    cancelled: &AtomicBool,
) -> Result<ForeignProfileImportResult, ForeignIntakeError> {
    let mut intake = Intake {
        cancelled,
        started: Instant::now(),
        counts: ForeignBatchCounts::default(),
        builder: ForeignBatchBuilder::default(),
    };
    intake.check()?;
    match input {
        ForeignBatchInput::SecureCrtDirectory | ForeignBatchInput::XshellDirectory => {
            let root = Directory::root(path)?;
            let format = if input == ForeignBatchInput::SecureCrtDirectory {
                ForeignSessionFormat::SecureCrtIni
            } else {
                ForeignSessionFormat::XshellSession
            };
            intake.directory(&root, format, &mut Vec::new(), default_username.trim())?;
        }
        ForeignBatchInput::Manifest => {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| valid_foreign_component(s))
                .ok_or(ForeignIntakeError::Invalid)?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let root = Directory::root(parent)?;
            let bytes = intake.read(&root, name)?;
            let mut manifest = ForeignSourceManifest::parse(&bytes)?;
            drop(bytes);
            manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
            for entry in manifest.entries {
                intake.check()?;
                let mut parts: Vec<_> = entry.path.split('/').collect();
                let name = parts.pop().ok_or(ForeignIntakeError::Invalid)?;
                let mut directories = vec![root.clone()];
                for component in parts {
                    let child = directories
                        .last()
                        .ok_or(ForeignIntakeError::Invalid)?
                        .child(component)?;
                    directories.push(child);
                }
                if is_default(name, entry.format)
                    && matches!(
                        entry.format,
                        ForeignSessionFormat::SecureCrtIni | ForeignSessionFormat::XshellSession
                    )
                {
                    // Explicit manifest defaults are errors rather than silently missing entries.
                    return Err(ForeignBatchError::Source.into());
                }
                intake.convert(
                    directories.last().ok_or(ForeignIntakeError::Invalid)?,
                    name,
                    entry.format,
                    &entry.folder,
                    default_username.trim(),
                )?;
                for directory in &directories {
                    directory.verify()?;
                }
            }
            root.verify()?;
        }
    }
    intake.check()?;
    let result = intake.builder.finish(intake.counts)?;
    if cancelled.load(Ordering::Acquire) || intake.started.elapsed() > Duration::from_secs(30) {
        return Err(ForeignIntakeError::Cancelled);
    }
    Ok(result)
}
