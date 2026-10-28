//! Raw bounded task output. Only typed identifiers form filesystem paths.
use cshell_domain::{HostId, TaskRunId};
use cshell_ipc::RuntimePaths;
use std::{fmt, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
mod analysis;
mod archive;
mod events;
mod merge;
mod search;
mod spool;
mod workspace;
pub(crate) use analysis::AnalysisProgress;
pub(crate) use archive::ArchiveCapture;
pub(crate) use workspace::{WorkspaceBundle, WorkspaceCapture, WorkspaceProgress};

pub const MAX_TASK_OUTPUT_PAGE: usize = 64 * 1024;
pub const MAX_TASK_STEP_OUTPUT: u64 = 1024 * 1024 * 1024;

#[derive(Clone)]
pub struct TaskOutputStore {
    root: PathBuf,
    limit: u64,
    indexes: std::sync::Arc<std::sync::Mutex<()>>,
}
impl fmt::Debug for TaskOutputStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskOutputStore")
            .field("limit", &self.limit)
            .finish()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TaskOutputError {
    #[error("task output storage unavailable")]
    Unavailable,
    #[error("task output exceeds its limit")]
    Limit,
    #[error("task output request is invalid")]
    Invalid,
}
impl TaskOutputStore {
    #[cfg(test)]
    pub(crate) fn with_test_limit(mut self, limit: u64) -> Self {
        self.limit = limit;
        self
    }
    pub fn new(root: PathBuf) -> Result<Self, TaskOutputError> {
        let root = RuntimePaths::prepare(root)
            .map_err(|_| TaskOutputError::Unavailable)?
            .root()
            .to_owned();
        let store = Self {
            root,
            limit: MAX_TASK_STEP_OUTPUT,
            indexes: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        if store.clean_old_spools().is_err() {
            tracing::warn!("Generated output spool cleanup did not finish");
        }
        Ok(store)
    }
    fn directory(
        &self,
        run: TaskRunId,
        host: HostId,
        step: usize,
    ) -> Result<PathBuf, TaskOutputError> {
        if run.as_uuid().is_nil()
            || host.as_uuid().is_nil()
            || step >= cshell_application::MAX_TASK_STEPS
        {
            return Err(TaskOutputError::Invalid);
        }
        Ok(self
            .root
            .join(run.to_string())
            .join(host.to_string())
            .join(step.to_string()))
    }
    pub(crate) async fn create(
        &self,
        run: TaskRunId,
        host: HostId,
        step: usize,
    ) -> Result<TaskOutputWriter, TaskOutputError> {
        let directory = self.directory(run, host, step)?;
        // Secure every newly created directory, including the run/host parents.
        let root = self.root.clone();
        let path = directory.clone();
        tokio::task::spawn_blocking(move || {
            for relative in [
                run.to_string(),
                format!("{run}/{host}"),
                format!("{run}/{host}/{step}"),
            ] {
                RuntimePaths::prepare(root.join(relative))
                    .map_err(|_| TaskOutputError::Unavailable)?;
            }
            Ok::<_, TaskOutputError>(path)
        })
        .await
        .map_err(|_| TaskOutputError::Unavailable)??;
        let stdout = create_file(directory.join("stdout.bin")).await?;
        let stderr = create_file(directory.join("stderr.bin")).await?;
        let events = events::TaskEventWriter::new(
            create_file(directory.join("events.bin")).await?,
            run,
            host,
            step,
        )
        .await?;
        #[cfg(unix)]
        {
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || {
                for path in [
                    &directory,
                    &root.join(run.to_string()).join(host.to_string()),
                    &root.join(run.to_string()),
                    &root,
                ] {
                    std::fs::File::open(path)
                        .and_then(|file| file.sync_all())
                        .map_err(|_| TaskOutputError::Unavailable)?;
                }
                Ok::<_, TaskOutputError>(())
            })
            .await
            .map_err(|_| TaskOutputError::Unavailable)??;
        }
        Ok(TaskOutputWriter {
            stdout,
            stderr,
            events,
            stream_bytes: [0, 0],
            bytes: 0,
            limit: self.limit,
        })
    }
    /// Read a bounded page, including partial output from an interrupted step.
    pub(crate) async fn comparison_page(
        &self,
        run: TaskRunId,
        host: HostId,
        step: usize,
        stderr: bool,
        offset: u64,
        limit: usize,
    ) -> Result<(Vec<u8>, u64), TaskOutputError> {
        if limit == 0 || limit > MAX_TASK_OUTPUT_PAGE || offset > self.limit {
            return Err(TaskOutputError::Invalid);
        }
        // Read validates existence and protection first. Missing is not empty.
        // Comparison is called only for terminal runs without a writer owner.
        let (_, length) = self.read(run, host, step, stderr, 0, 1).await?;
        if offset >= length {
            return Ok((Vec::new(), length));
        }
        let (bytes, after) = self.read(run, host, step, stderr, offset, limit).await?;
        if after != length {
            return Err(TaskOutputError::Unavailable);
        }
        Ok((bytes, length))
    }

    /// Read a bounded page, including partial output from an interrupted step.
    pub async fn read(
        &self,
        run: TaskRunId,
        host: HostId,
        step: usize,
        stderr: bool,
        offset: u64,
        limit: usize,
    ) -> Result<(Vec<u8>, u64), TaskOutputError> {
        if limit == 0 || limit > MAX_TASK_OUTPUT_PAGE || offset > self.limit {
            return Err(TaskOutputError::Invalid);
        }
        let directory = self.directory(run, host, step)?;
        // Reject symlink/reparse directories before accessing output.
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            for relative in [
                run.to_string(),
                format!("{run}/{host}"),
                format!("{run}/{host}/{step}"),
            ] {
                let path = root.join(relative);
                let metadata =
                    std::fs::symlink_metadata(&path).map_err(|_| TaskOutputError::Unavailable)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(TaskOutputError::Invalid);
                }
                RuntimePaths::prepare(path).map_err(|_| TaskOutputError::Unavailable)?;
            }
            Ok::<_, TaskOutputError>(())
        })
        .await
        .map_err(|_| TaskOutputError::Unavailable)??;
        let path = directory.join(if stderr { "stderr.bin" } else { "stdout.bin" });
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > self.limit {
            return Err(TaskOutputError::Invalid);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(TaskOutputError::Invalid);
            }
        }
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        let length = file
            .metadata()
            .await
            .map_err(|_| TaskOutputError::Unavailable)?
            .len();
        if offset > length {
            return Err(TaskOutputError::Invalid);
        }
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        let mut bytes = vec![0; limit.min((length - offset) as usize)];
        file.read_exact(&mut bytes)
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        Ok((bytes, length))
    }
}
async fn create_file(path: PathBuf) -> Result<tokio::fs::File, TaskOutputError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .await
        .map_err(|_| TaskOutputError::Unavailable)
}
pub(crate) struct TaskOutputWriter {
    stdout: tokio::fs::File,
    stderr: tokio::fs::File,
    events: events::TaskEventWriter,
    stream_bytes: [u64; 2],
    bytes: u64,
    limit: u64,
}
impl fmt::Debug for TaskOutputWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskOutputWriter")
            .field("bytes", &self.bytes)
            .finish()
    }
}
impl TaskOutputWriter {
    pub(crate) async fn write(&mut self, stderr: bool, data: &[u8]) -> Result<(), TaskOutputError> {
        if self.events.is_sealed() {
            return Err(TaskOutputError::Invalid);
        }
        if data.len() > MAX_TASK_OUTPUT_PAGE
            || self.bytes.saturating_add(data.len() as u64) > self.limit
        {
            return Err(TaskOutputError::Limit);
        }
        if data.is_empty() {
            return Ok(());
        }
        let time = self.events.now()?;
        let stream = usize::from(stderr);
        let offset = self.stream_bytes[stream];
        self.bytes += data.len() as u64;
        let file = if stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        file.write_all(data)
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.stream_bytes[stream] += data.len() as u64;
        self.events.append(stderr, offset, data, time).await
    }
    pub(crate) async fn finish(&mut self) -> Result<(), TaskOutputError> {
        self.stdout
            .flush()
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.stderr
            .flush()
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.stdout
            .sync_all()
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.stderr
            .sync_all()
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.events.finish(self.stream_bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn output_limits_preserve_partial_bytes_and_never_overwrite()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let mut store = TaskOutputStore::new(directory.path().join("output"))?;
        store.limit = 8;
        let (run, host) = (TaskRunId::new(), HostId::new());
        let mut output = store.create(run, host, 0).await?;
        output.write(false, b"12345").await?;
        output.write(true, b"678").await?;
        assert_eq!(output.write(false, b"9").await, Err(TaskOutputError::Limit));
        output.finish().await?;
        assert_eq!(store.read(run, host, 0, false, 0, 8).await?.0, b"12345");
        assert_eq!(store.read(run, host, 0, true, 1, 8).await?.0, b"78");
        assert!(store.create(run, host, 0).await.is_err());
        assert!(store.read(run, host, 0, false, 6, 8).await.is_err());
        assert!(
            store
                .read(run, host, 0, false, 0, MAX_TASK_OUTPUT_PAGE + 1)
                .await
                .is_err()
        );
        assert!(
            store
                .read(run, host, cshell_application::MAX_TASK_STEPS, false, 0, 8)
                .await
                .is_err()
        );
        assert_eq!(store.read(run, host, 0, false, 0, 8).await?.0, b"12345");
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn output_reads_reject_symlinks_and_files_are_private()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir()?;
        let store = TaskOutputStore::new(directory.path().join("output"))?;
        let (run, host) = (TaskRunId::new(), HostId::new());
        let mut writer = store.create(run, host, 0).await?;
        writer.write(false, b"private").await?;
        writer.finish().await?;
        drop(writer);
        let path = store.directory(run, host, 0)?.join("stdout.bin");
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(&path)?;
        std::os::unix::fs::symlink(directory.path().join("outside"), &path)?;
        assert_eq!(
            store.read(run, host, 0, false, 0, 8).await,
            Err(TaskOutputError::Invalid)
        );
        Ok(())
    }
}
