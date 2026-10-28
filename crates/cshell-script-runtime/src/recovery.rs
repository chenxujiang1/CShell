//! Two per-user durable slots. Unproved launch windows remain occupied.
use crate::{ScriptRunId, ScriptWorkerError};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
mod process;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessIdentity {
    pid: u32,
    birth: [u64; 2],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    run: ScriptRunId,
    directory: Option<String>,
    launching: bool,
    process: Option<ProcessIdentity>,
    exit_confirmed: bool,
}
pub(super) struct Slot {
    file: File,
    root: PathBuf,
    record: Record,
}
fn root() -> Result<PathBuf, ScriptWorkerError> {
    let paths =
        cshell_ipc::RuntimePaths::for_current_user().map_err(|_| ScriptWorkerError::Unavailable)?;
    let paths = cshell_ipc::RuntimePaths::prepare(paths.root().join("script-workers"))
        .map_err(|_| ScriptWorkerError::Unavailable)?;
    paths
        .root()
        .canonicalize()
        .map_err(|_| ScriptWorkerError::Unavailable)
}
fn open(root: &Path, index: u8) -> Result<File, ScriptWorkerError> {
    let path = root.join(format!("slot-{index}.json"));
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink() || !m.is_file())
    {
        return Err(ScriptWorkerError::Isolation);
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|_| ScriptWorkerError::Unavailable)?;
    if !file
        .metadata()
        .map_err(|_| ScriptWorkerError::Unavailable)?
        .is_file()
    {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(file)
}
fn read(file: &mut File) -> Result<Option<Record>, ScriptWorkerError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ScriptWorkerError::Isolation)?;
    let mut bytes = Vec::new();
    (&mut *file)
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| ScriptWorkerError::Isolation)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > 4096 {
        return Err(ScriptWorkerError::Isolation);
    }
    let record: Record =
        serde_json::from_slice(&bytes).map_err(|_| ScriptWorkerError::Isolation)?;
    if record.version != 1 || record.run.as_uuid().is_nil() {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(Some(record))
}
impl Slot {
    pub(super) fn acquire(run: ScriptRunId) -> Result<Self, ScriptWorkerError> {
        let root = root()?;
        for index in 0..2 {
            let file = open(&root, index)?;
            if File::try_lock(&file).is_err() {
                continue;
            }
            // An unlocked nonempty file still represents an unproved run.
            // Only the explicit recovery operation may inspect/clear it.
            if file
                .metadata()
                .map_err(|_| ScriptWorkerError::Isolation)?
                .len()
                != 0
            {
                continue;
            }
            let mut slot = Self {
                file,
                root,
                record: Record {
                    version: 1,
                    run,
                    directory: None,
                    launching: false,
                    process: None,
                    exit_confirmed: false,
                },
            };
            slot.save()?;
            return Ok(slot);
        }
        Err(ScriptWorkerError::Busy)
    }
    pub(super) fn root(&self) -> &Path {
        &self.root
    }
    fn save(&mut self) -> Result<(), ScriptWorkerError> {
        let bytes = serde_json::to_vec(&self.record).map_err(|_| ScriptWorkerError::Isolation)?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| ScriptWorkerError::Isolation)?;
        self.file
            .write_all(&bytes)
            .map_err(|_| ScriptWorkerError::Isolation)?;
        self.file
            .set_len(bytes.len() as u64)
            .map_err(|_| ScriptWorkerError::Isolation)?;
        self.file
            .sync_all()
            .map_err(|_| ScriptWorkerError::Isolation)
    }
    pub(super) fn directory(&mut self, path: &Path) -> Result<(), ScriptWorkerError> {
        if path.parent() != Some(self.root.as_path()) {
            return Err(ScriptWorkerError::Isolation);
        }
        self.record.directory = Some(
            path.file_name()
                .and_then(|p| p.to_str())
                .ok_or(ScriptWorkerError::Isolation)?
                .to_owned(),
        );
        self.save()
    }
    pub(super) fn launching(&mut self) -> Result<(), ScriptWorkerError> {
        self.record.launching = true;
        self.save()
    }
    pub(super) fn started(&mut self, pid: u32) -> Result<(), ScriptWorkerError> {
        self.record.process = Some(process::identity(pid)?.ok_or(ScriptWorkerError::Isolation)?);
        self.save()
    }
    pub(super) fn exited(&mut self) -> Result<(), ScriptWorkerError> {
        self.record.exit_confirmed = true;
        self.save()
    }
    pub(super) fn clear(&mut self) -> bool {
        self.file
            .set_len(0)
            .and_then(|()| self.file.sync_all())
            .is_ok()
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptRecoveryReport {
    pub version: u32,
    pub recovered: u8,
    pub busy: u8,
    pub unresolved: u8,
}
pub async fn recover_script_resources_bounded() -> Result<ScriptRecoveryReport, ScriptWorkerError> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let permit = GATE
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ScriptWorkerError::Busy)?;
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("cshell-script-recovery".into())
        .spawn(move || {
            let _permit = permit;
            let _ = send.send(recover_script_resources());
        })
        .map_err(|_| ScriptWorkerError::Unavailable)?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| ScriptWorkerError::Unavailable)?
        .map_err(|_| ScriptWorkerError::Unavailable)?
}
/// Explicit and payload-free. Never sends signals, trusts a bare PID, or
/// clears an unknown launch window. Errors leave the durable record intact.
pub fn recover_script_resources() -> Result<ScriptRecoveryReport, ScriptWorkerError> {
    let root = root()?;
    let mut report = ScriptRecoveryReport {
        version: 1,
        ..Default::default()
    };
    for index in 0..2 {
        let mut file = open(&root, index)?;
        if File::try_lock(&file).is_err() {
            report.busy += 1;
            continue;
        }
        let record = match read(&mut file) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(_) => {
                report.unresolved += 1;
                continue;
            }
        };
        let absent = record.exit_confirmed
            || (!record.launching && record.process.is_none())
            || record.process.is_some_and(|old| {
                process::identity(old.pid).is_ok_and(|current| current != Some(old))
            });
        if !absent {
            report.unresolved += 1;
            continue;
        }
        #[cfg(windows)]
        if !crate::containment::cleanup_orphan_profile(record.run) {
            report.unresolved += 1;
            continue;
        }
        if !remove_directory(&root, &record) {
            report.unresolved += 1;
            continue;
        }
        if file.set_len(0).and_then(|()| file.sync_all()).is_ok() {
            report.recovered += 1;
        } else {
            report.unresolved += 1;
        }
    }
    Ok(report)
}
fn remove_directory(root: &Path, record: &Record) -> bool {
    let Some(name) = &record.directory else {
        return true;
    };
    let prefix = format!("run-{}-", record.run);
    if !name.starts_with(&prefix)
        || name.len() > 100
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return false;
    }
    let path = root.join(name);
    match path.symlink_metadata() {
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
            // Fixed direct child of the secured runtime root, never a path
            // supplied by a script. Canonical containment must still agree.
            path.canonicalize().is_ok_and(|p| p.parent() == Some(root))
                && std::fs::remove_dir_all(path).is_ok()
        }
        _ => false,
    }
}
