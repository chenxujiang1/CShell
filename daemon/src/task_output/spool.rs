//! Only this version's leased, inactive, old generated spools are removed.
use super::*;
use cshell_application::{SystemTaskClock, TaskClock};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
};
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Lease {
    format: String,
    version: u32,
    run: TaskRunId,
    created_at_ms: u64,
}
pub(super) fn lease(directory: &std::path::Path, run: TaskRunId) -> Result<File, TaskOutputError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(directory.join("lease.json"))
        .map_err(|_| TaskOutputError::Unavailable)?;
    file.try_lock().map_err(|_| TaskOutputError::Unavailable)?;
    let record = Lease {
        format: "cshell.output-spool-lease".into(),
        version: 1,
        run,
        created_at_ms: SystemTaskClock::new()
            .and_then(|c| c.now_ms())
            .map_err(|_| TaskOutputError::Unavailable)?,
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| TaskOutputError::Unavailable)?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| TaskOutputError::Unavailable)?;
    Ok(file)
}
fn regular(meta: &std::fs::Metadata) -> bool {
    if !meta.is_file() || meta.file_type().is_symlink() {
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
impl TaskOutputStore {
    pub(super) fn clean_old_spools(&self) -> Result<usize, TaskOutputError> {
        let now = SystemTaskClock::new()
            .and_then(|c| c.now_ms())
            .map_err(|_| TaskOutputError::Unavailable)?;
        let mut removed = 0;
        for (bucket, prefix, body_name) in [
            ("archives", "capture-", "archive.bin"),
            ("bundles", "bundle-", "bundle.bin"),
        ] {
            let root = self.root.join(bucket);
            let entries = match std::fs::read_dir(&root) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(TaskOutputError::Unavailable),
            };
            RuntimePaths::prepare(root.clone()).map_err(|_| TaskOutputError::Unavailable)?;
            for entry in entries.take(128) {
                let entry = entry.map_err(|_| TaskOutputError::Unavailable)?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if !name.starts_with(prefix)
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    continue;
                }
                let directory = entry.path();
                let meta = std::fs::symlink_metadata(&directory)
                    .map_err(|_| TaskOutputError::Unavailable)?;
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    continue;
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        continue;
                    }
                }
                let lease_path = directory.join("lease.json");
                let meta = match std::fs::symlink_metadata(&lease_path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !regular(&meta) || meta.len() > 512 {
                    continue;
                }
                let mut options = OpenOptions::new();
                options.read(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    options.custom_flags(0x0020_0000);
                }
                let mut file = match options.open(&lease_path) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                if file.try_lock().is_err() {
                    continue;
                }
                if !regular(&file.metadata().map_err(|_| TaskOutputError::Unavailable)?) {
                    continue;
                }
                let mut bytes = Vec::new();
                (&mut file)
                    .take(513)
                    .read_to_end(&mut bytes)
                    .map_err(|_| TaskOutputError::Unavailable)?;
                let Ok(record) = serde_json::from_slice::<Lease>(&bytes) else {
                    continue;
                };
                if record.format != "cshell.output-spool-lease"
                    || record.version != 1
                    || record.run.as_uuid().is_nil()
                    || now < record.created_at_ms
                    || now - record.created_at_ms < 24 * 60 * 60 * 1000
                {
                    continue;
                }
                RuntimePaths::prepare(directory.clone())
                    .map_err(|_| TaskOutputError::Unavailable)?;
                let names = std::fs::read_dir(&directory)
                    .map_err(|_| TaskOutputError::Unavailable)?
                    .map(|e| e.map(|e| e.file_name()))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| TaskOutputError::Unavailable)?;
                if names
                    .iter()
                    .any(|n| n.to_str() != Some("lease.json") && n.to_str() != Some(body_name))
                {
                    continue;
                }
                let body = directory.join(body_name);
                match std::fs::symlink_metadata(&body) {
                    Ok(meta) if regular(&meta) => {
                        let age = meta.modified().ok().and_then(|time| {
                            std::time::SystemTime::now().duration_since(time).ok()
                        });
                        if age.is_none_or(|age| age.as_secs() < 24 * 60 * 60) {
                            continue;
                        }
                        std::fs::remove_file(&body).map_err(|_| TaskOutputError::Unavailable)?
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    _ => continue,
                }
                // Keep the lease through data deletion, then close before deleting its name.
                drop(file);
                std::fs::remove_file(&lease_path).map_err(|_| TaskOutputError::Unavailable)?;
                std::fs::remove_dir(&directory).map_err(|_| TaskOutputError::Unavailable)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}
