//! Immutable private spool; original command files are never moved or replaced.
use super::analysis::storage;
use super::*;
use cshell_application::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
pub(crate) struct ArchiveCapture {
    pub artifact: OutputArchiveArtifact,
    file: File,
    _lease: File,
    _directory: tempfile::TempDir,
}
impl fmt::Debug for ArchiveCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveCapture")
            .field("artifact", &self.artifact)
            .finish_non_exhaustive()
    }
}
impl ArchiveCapture {
    pub(crate) fn read(&self, offset: u64, limit: usize) -> Result<Vec<u8>, OutputAnalysisError> {
        if offset > self.artifact.bytes || limit == 0 || limit > MAX_OUTPUT_ARCHIVE_CHUNK {
            return Err(OutputAnalysisError::Invalid);
        }
        if self.file.metadata().map_err(storage)?.len() != self.artifact.bytes {
            return Err(OutputAnalysisError::Changed);
        }
        let mut body = vec![0; limit.min((self.artifact.bytes - offset) as usize)];
        let mut done = 0;
        while done < body.len() {
            #[cfg(unix)]
            let count = {
                use std::os::unix::fs::FileExt;
                self.file.read_at(&mut body[done..], offset + done as u64)
            };
            #[cfg(windows)]
            let count = {
                use std::os::windows::fs::FileExt;
                self.file.seek_read(&mut body[done..], offset + done as u64)
            };
            let count = count.map_err(storage)?;
            if count == 0 {
                return Err(OutputAnalysisError::Changed);
            }
            done += count;
        }
        Ok(body)
    }
}
fn check(cancel: &AtomicBool, started: Instant) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::other("archive cancelled"))
    } else if started.elapsed() > Duration::from_secs(600) {
        Err(io::Error::new(io::ErrorKind::TimedOut, "archive deadline"))
    } else {
        Ok(())
    }
}
struct CheckedReader<'a> {
    file: &'a mut File,
    cancel: &'a AtomicBool,
    started: Instant,
}
impl Read for CheckedReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        check(self.cancel, self.started)?;
        self.file.read(bytes)
    }
}
struct CaptureWriter<'a> {
    file: &'a mut File,
    cancel: &'a AtomicBool,
    started: Instant,
    progress: &'a AnalysisProgress,
}
impl Write for CaptureWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check(self.cancel, self.started)?;
        let count = self.file.write(bytes)?;
        self.progress
            .left
            .fetch_add(count as u64, Ordering::Release);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
fn failure(error: io::Error) -> OutputAnalysisError {
    match error.kind() {
        io::ErrorKind::TimedOut => OutputAnalysisError::Timeout,
        io::ErrorKind::InvalidData => OutputAnalysisError::Changed,
        _ => storage(error),
    }
}
impl TaskOutputStore {
    pub(crate) fn capture_archive(
        &self,
        scope: OutputAnalysisRequest,
        binding: OutputAnalysisBinding,
        cancel: &AtomicBool,
        progress: &AnalysisProgress,
    ) -> Result<ArchiveCapture, OutputAnalysisError> {
        let started = Instant::now();
        let _cleaned = self.clean_old_spools();
        check(cancel, started).map_err(failure)?;
        let source = (binding.child_id, scope.left, binding.child_step as usize);
        let mut inputs = vec![
            self.pin_analysis(source.0, source.1, source.2, false)?,
            self.pin_analysis(source.0, source.1, source.2, true)?,
        ];
        match self.pin_merge_journal(source.0, source.1, source.2) {
            Ok(journal) => inputs.push(journal),
            Err(OutputAnalysisError::Missing) => {}
            Err(error) => return Err(error),
        }
        if inputs[0]
            .metadata
            .len()
            .checked_add(inputs[1].metadata.len())
            .is_none_or(|bytes| bytes > MAX_ANALYSIS_STREAM_BYTES)
        {
            return Err(OutputAnalysisError::Invalid);
        }
        let mut entries = Vec::new();
        let mut buffer = vec![0_u8; MAX_TASK_OUTPUT_PAGE];
        for (index, input) in inputs.iter_mut().enumerate() {
            let size = input.metadata.len();
            let mut remaining = size;
            let mut hash = Sha256::new();
            while remaining > 0 {
                check(cancel, started).map_err(failure)?;
                let count = buffer.len().min(remaining as usize);
                input
                    .file
                    .read_exact(&mut buffer[..count])
                    .map_err(storage)?;
                hash.update(&buffer[..count]);
                progress.right.fetch_add(count as u64, Ordering::Release);
                remaining -= count as u64;
            }
            input.check_after(if index == 2 {
                MAX_ARCHIVE_JOURNAL_BYTES
            } else {
                self.limit
            })?;
            input.file.seek(SeekFrom::Start(0)).map_err(storage)?;
            entries.push(OutputArchiveEntry {
                kind: match index {
                    0 => OutputArchiveEntryKind::Stdout,
                    1 => OutputArchiveEntryKind::Stderr,
                    _ => OutputArchiveEntryKind::Journal,
                },
                bytes: size,
                sha256: hash.finalize().into(),
            });
        }
        let captured_at_ms = SystemTaskClock::new()
            .and_then(|clock| clock.now_ms())
            .map_err(|_| OutputAnalysisError::Storage)?;
        let manifest = OutputArchiveManifest {
            format: "cshell.output-archive".into(),
            version: 1,
            archive_id: TaskRunId::new(),
            scope,
            binding,
            captured_at_ms,
            entries,
        };
        manifest
            .validate()
            .map_err(|_| OutputAnalysisError::Invalid)?;
        let parent = RuntimePaths::prepare(self.root.join("archives"))
            .map_err(|_| OutputAnalysisError::Storage)?;
        let directory = tempfile::Builder::new()
            .prefix("capture-")
            .tempdir_in(parent.root())
            .map_err(storage)?;
        RuntimePaths::prepare(directory.path().to_owned())
            .map_err(|_| OutputAnalysisError::Storage)?;
        let lease = super::spool::lease(directory.path(), manifest.scope.run_id)
            .map_err(|_| OutputAnalysisError::Storage)?;
        let path = directory.path().join("archive.bin");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(storage)?;
        let mut readers: Vec<_> = inputs
            .iter_mut()
            .map(|input| CheckedReader {
                file: &mut input.file,
                cancel,
                started,
            })
            .collect();
        let mut sources: Vec<&mut dyn Read> = readers
            .iter_mut()
            .map(|reader| reader as &mut dyn Read)
            .collect();
        let artifact = write_output_archive(
            &manifest,
            &mut sources,
            &mut CaptureWriter {
                file: &mut file,
                cancel,
                started,
                progress,
            },
        )
        .map_err(failure)?;
        drop(sources);
        drop(readers);
        file.sync_all().map_err(storage)?;
        check(cancel, started).map_err(failure)?;
        for (index, input) in inputs.iter().enumerate() {
            input.check_after(if index == 2 {
                MAX_ARCHIVE_JOURNAL_BYTES
            } else {
                self.limit
            })?;
        }
        drop(file);
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000).share_mode(1);
        }
        let file = options.open(&path).map_err(storage)?;
        let metadata = file.metadata().map_err(storage)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != artifact.bytes
        {
            return Err(OutputAnalysisError::Changed);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(OutputAnalysisError::Invalid);
            }
        }
        Ok(ArchiveCapture {
            _lease: lease,
            artifact,
            file,
            _directory: directory,
        })
    }
}
