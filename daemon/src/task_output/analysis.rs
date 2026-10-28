//! Reads pinned stored-output handles with bounded buffers, cancellation and a deadline.
use super::*;
use cshell_application::{OutputAnalysisError, OutputAnalysisResult, OutputStreamDigest};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::Read,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};
#[derive(Debug, Default)]
pub(crate) struct AnalysisProgress {
    pub left: AtomicU64,
    pub right: AtomicU64,
}
pub(super) struct PinnedOutput {
    pub(super) file: File,
    path: PathBuf,
    pub(super) metadata: Metadata,
}
pub(super) fn storage(error: std::io::Error) -> OutputAnalysisError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        return OutputAnalysisError::Changed;
    }
    if error.kind() == std::io::ErrorKind::NotFound {
        OutputAnalysisError::Missing
    } else {
        OutputAnalysisError::Storage
    }
}
fn valid(metadata: &Metadata, limit: u64) -> bool {
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
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
pub(super) fn same(left: &Metadata, right: &Metadata) -> bool {
    if left.len() != right.len()
        || left
            .modified()
            .ok()
            .zip(right.modified().ok())
            .is_none_or(|(left, right)| left != right)
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.dev() != right.dev()
            || left.ino() != right.ino()
            || left.ctime() != right.ctime()
            || left.ctime_nsec() != right.ctime_nsec()
        {
            return false;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if left.creation_time() != right.creation_time()
            || left.last_write_time() != right.last_write_time()
        {
            return false;
        }
    }
    true
}
impl PinnedOutput {
    pub(super) fn check_after(&self, limit: u64) -> Result<(), OutputAnalysisError> {
        let current = self.file.metadata().map_err(storage)?;
        let path = std::fs::symlink_metadata(&self.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                OutputAnalysisError::Changed
            } else {
                storage(error)
            }
        })?;
        if !valid(&current, limit)
            || !valid(&path, limit)
            || !same(&self.metadata, &current)
            || !same(&self.metadata, &path)
        {
            return Err(OutputAnalysisError::Changed);
        }
        Ok(())
    }
}
impl TaskOutputStore {
    pub(super) fn pin_analysis(
        &self,
        run: TaskRunId,
        host: HostId,
        step: usize,
        stderr: bool,
    ) -> Result<PinnedOutput, OutputAnalysisError> {
        self.pin_named_output(
            run,
            host,
            step,
            if stderr { "stderr.bin" } else { "stdout.bin" },
            self.limit,
        )
    }
    pub(super) fn pin_merge_journal(
        &self,
        run: TaskRunId,
        host: HostId,
        step: usize,
    ) -> Result<PinnedOutput, OutputAnalysisError> {
        self.pin_named_output(
            run,
            host,
            step,
            "events.bin",
            super::events::MAX_JOURNAL_BYTES,
        )
    }
    fn pin_named_output(
        &self,
        run: TaskRunId,
        host: HostId,
        step: usize,
        name: &str,
        limit: u64,
    ) -> Result<PinnedOutput, OutputAnalysisError> {
        let directory = self
            .directory(run, host, step)
            .map_err(|_| OutputAnalysisError::Invalid)?;
        RuntimePaths::prepare(self.root.clone()).map_err(|_| OutputAnalysisError::Invalid)?;
        for relative in [
            run.to_string(),
            format!("{run}/{host}"),
            format!("{run}/{host}/{step}"),
        ] {
            let path = self.root.join(relative);
            let metadata = std::fs::symlink_metadata(&path).map_err(storage)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(OutputAnalysisError::Invalid);
            }
            RuntimePaths::prepare(path).map_err(|_| OutputAnalysisError::Invalid)?;
        }
        let path = directory.join(name);
        let before = std::fs::symlink_metadata(&path).map_err(storage)?;
        if !valid(&before, limit) {
            return Err(OutputAnalysisError::Invalid);
        }
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
        if !valid(&metadata, limit) || !same(&before, &metadata) {
            return Err(OutputAnalysisError::Changed);
        }
        Ok(PinnedOutput {
            file,
            path,
            metadata,
        })
    }
    pub(crate) fn analyze_stored_output(
        &self,
        left: (TaskRunId, HostId, usize),
        right: Option<(TaskRunId, HostId, usize)>,
        stderr: bool,
        cancel: &AtomicBool,
        progress: &AnalysisProgress,
    ) -> Result<OutputAnalysisResult, OutputAnalysisError> {
        let started = Instant::now();
        let check = || {
            if cancel.load(Ordering::Acquire) {
                Err(OutputAnalysisError::Worker)
            } else if started.elapsed() > Duration::from_secs(300) {
                Err(OutputAnalysisError::Timeout)
            } else {
                Ok(())
            }
        };
        check()?;
        let mut left = self.pin_analysis(left.0, left.1, left.2, stderr)?;
        let mut right = right
            .map(|(run, host, step)| self.pin_analysis(run, host, step, stderr))
            .transpose()?;
        let left_length = left.metadata.len();
        let right_length = right.as_ref().map_or(0, |right| right.metadata.len());
        let mut left_hash = Sha256::new();
        let mut right_hash = Sha256::new();
        let mut left_buffer = vec![0_u8; MAX_TASK_OUTPUT_PAGE];
        let mut right_buffer = vec![0_u8; MAX_TASK_OUTPUT_PAGE];
        let mut offset = 0_u64;
        let mut differing = 0_u64;
        let mut first = None;
        while offset < left_length.max(right_length) {
            check()?;
            let left_count = MAX_TASK_OUTPUT_PAGE.min(left_length.saturating_sub(offset) as usize);
            let right_count =
                MAX_TASK_OUTPUT_PAGE.min(right_length.saturating_sub(offset) as usize);
            left.file
                .read_exact(&mut left_buffer[..left_count])
                .map_err(storage)?;
            left_hash.update(&left_buffer[..left_count]);
            if let Some(right) = &mut right {
                right
                    .file
                    .read_exact(&mut right_buffer[..right_count])
                    .map_err(storage)?;
                right_hash.update(&right_buffer[..right_count]);
                for index in 0..left_count.max(right_count) {
                    if left_buffer.get(index).filter(|_| index < left_count)
                        != right_buffer.get(index).filter(|_| index < right_count)
                    {
                        differing += 1;
                        if first.is_none() {
                            first = Some(offset + index as u64);
                        }
                    }
                }
            }
            progress.left.store(
                (offset + left_count as u64).min(left_length),
                Ordering::Release,
            );
            progress.right.store(
                (offset + right_count as u64).min(right_length),
                Ordering::Release,
            );
            offset += MAX_TASK_OUTPUT_PAGE as u64;
        }
        check()?;
        for output in std::iter::once(&left).chain(right.iter()) {
            output.check_after(self.limit)?;
        }
        Ok(OutputAnalysisResult {
            left: OutputStreamDigest {
                bytes: left_length,
                sha256: left_hash.finalize().into(),
            },
            right: right.as_ref().map(|_| OutputStreamDigest {
                bytes: right_length,
                sha256: right_hash.finalize().into(),
            }),
            equal: right.as_ref().map(|_| differing == 0),
            differing_bytes: right.as_ref().map(|_| differing),
            first_difference: first,
        })
    }
}
