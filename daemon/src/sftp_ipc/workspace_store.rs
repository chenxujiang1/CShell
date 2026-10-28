//! Private checksummed records; write revisions by CAS, never persist file bodies here.
use cshell_application::{MAX_SFTP_WORKSPACE_DOCUMENT, SftpEditDocument, SftpFileJob};
use cshell_domain::SessionId;
use cshell_ipc::RuntimePaths;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, io::Read, path::PathBuf};
pub(super) trait Record: Clone + Serialize + DeserializeOwned + Send + 'static {
    fn revision(&self) -> u64;
    fn id(&self) -> SessionId;
    fn valid(&self) -> Result<(), &'static str>;
}
impl Record for SftpFileJob {
    fn revision(&self) -> u64 {
        self.revision
    }
    fn id(&self) -> SessionId {
        self.id
    }
    fn valid(&self) -> Result<(), &'static str> {
        self.validate()
    }
}
impl Record for SftpEditDocument {
    fn revision(&self) -> u64 {
        self.revision
    }
    fn id(&self) -> SessionId {
        self.id
    }
    fn valid(&self) -> Result<(), &'static str> {
        self.validate()
    }
}
#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored<T> {
    format: String,
    version: u32,
    digest: [u8; 32],
    value: T,
}
pub(super) fn root() -> Result<PathBuf, &'static str> {
    let paths = RuntimePaths::for_current_user().map_err(|_| "workspace storage unavailable")?;
    Ok(RuntimePaths::prepare(paths.root().join("sftp-workspace"))
        .map_err(|_| "workspace storage unavailable")?
        .root()
        .to_owned())
}
pub(super) fn directory(id: SessionId) -> Result<PathBuf, &'static str> {
    if id.as_uuid().is_nil() {
        return Err("invalid workspace identity");
    }
    Ok(RuntimePaths::prepare(root()?.join(id.to_string()))
        .map_err(|_| "workspace directory unavailable")?
        .root()
        .to_owned())
}
pub(super) async fn check_capacity() -> Result<(), &'static str> {
    tokio::task::spawn_blocking(|| {
        let mut count = 0;
        for entry in std::fs::read_dir(root()?).map_err(|_| "workspace index unavailable")? {
            let entry = entry.map_err(|_| "workspace index unavailable")?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| parse_id(n).is_some())
            {
                count += 1;
                if count >= 1024 {
                    return Err("workspace history limit; explicitly forget a terminal record");
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| "workspace index worker unavailable")?
}
pub(super) fn parse_id(name: &str) -> Option<SessionId> {
    if name.len() != 36 {
        return None;
    }
    let value: SessionId =
        serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()?;
    (!value.as_uuid().is_nil() && value.to_string() == name).then_some(value)
}
fn regular(metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
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
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    options
}
fn load<T: Record>(id: SessionId) -> Result<T, &'static str> {
    let candidate = root()?.join(id.to_string());
    let metadata =
        std::fs::symlink_metadata(&candidate).map_err(|_| "workspace record unavailable")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("invalid workspace record directory");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("workspace reparse directory refused");
        }
    }
    let path = directory(id)?.join("record.json");
    let before = std::fs::symlink_metadata(&path).map_err(|_| "workspace record unavailable")?;
    if !regular(&before) || before.len() > (MAX_SFTP_WORKSPACE_DOCUMENT + 1024) as u64 {
        return Err("invalid workspace record");
    }
    let mut file = options()
        .read(true)
        .open(path)
        .map_err(|_| "workspace record unavailable")?;
    if !regular(
        &file
            .metadata()
            .map_err(|_| "workspace record unavailable")?,
    ) {
        return Err("invalid workspace record");
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_SFTP_WORKSPACE_DOCUMENT + 1025) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "workspace record unavailable")?;
    let stored: Stored<T> =
        serde_json::from_slice(&bytes).map_err(|_| "invalid workspace record")?;
    let digest: [u8; 32] = Sha256::digest(
        serde_json::to_vec(&stored.value).map_err(|_| "invalid workspace encoding")?,
    )
    .into();
    stored.value.valid()?;
    if stored.format != "cshell.sftp-workspace-storage"
        || stored.version != 1
        || stored.digest != digest
        || stored.value.id() != id
    {
        return Err("workspace checksum or identity mismatch");
    }
    Ok(stored.value)
}
fn save<T: Record>(value: T, previous: Option<u64>) -> Result<(), &'static str> {
    value.valid()?;
    let directory = directory(value.id())?;
    let lease = options()
        .read(true)
        .write(true)
        .create(true)
        .open(directory.join("record.lock"))
        .map_err(|_| "workspace lease unavailable")?;
    if !regular(
        &lease
            .metadata()
            .map_err(|_| "workspace lease unavailable")?,
    ) {
        return Err("invalid workspace lease");
    }
    lease.try_lock().map_err(|_| "workspace record busy")?;
    if let Some(previous) = previous {
        if load::<T>(value.id())?.revision() != previous
            || previous.checked_add(1) != Some(value.revision())
        {
            return Err("workspace revision conflict");
        }
    } else if value.revision() != 1
        || directory
            .join("record.json")
            .try_exists()
            .map_err(|_| "workspace record unavailable")?
    {
        return Err("workspace record already exists");
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| "invalid workspace encoding")?;
    if bytes.len() > MAX_SFTP_WORKSPACE_DOCUMENT {
        return Err("workspace record limit");
    }
    let stored = Stored {
        format: "cshell.sftp-workspace-storage".into(),
        version: 1,
        digest: Sha256::digest(bytes).into(),
        value,
    };
    let mut file = tempfile::NamedTempFile::new_in(&directory)
        .map_err(|_| "workspace temporary unavailable")?;
    serde_json::to_writer(&mut file, &stored).map_err(|_| "workspace write failed")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "workspace sync failed")?;
    file.persist(directory.join("record.json"))
        .map_err(|_| "workspace publication unconfirmed")?;
    #[cfg(unix)]
    std::fs::File::open(directory)
        .and_then(|f| f.sync_all())
        .map_err(|_| "workspace parent sync failed")?;
    Ok(())
}
pub(super) async fn read<T: Record>(id: SessionId) -> Result<T, &'static str> {
    tokio::task::spawn_blocking(move || load::<T>(id))
        .await
        .map_err(|_| "workspace worker unavailable")?
}
pub(super) async fn history(
    session: SessionId,
    page: cshell_application::SftpWorkspacePage,
) -> Result<cshell_application::SftpWorkspaceReply, &'static str> {
    tokio::task::spawn_blocking(move || {
        let mut items = Vec::new();
        for entry in std::fs::read_dir(root()?).map_err(|_| "workspace index unavailable")? {
            let entry = entry.map_err(|_| "workspace index unavailable")?;
            let Some(id) = entry.file_name().to_str().and_then(parse_id) else {
                continue;
            };
            if !entry
                .path()
                .join("record.json")
                .try_exists()
                .map_err(|_| "workspace record unavailable")?
            {
                continue;
            }
            let summary = if let Ok(job) = load::<SftpFileJob>(id) {
                cshell_application::SftpWorkspaceSummary {
                    id,
                    session: job.session,
                    revision: job.revision,
                    editor: false,
                    state: format!("{:?}", job.state),
                    path: job.draft.path,
                }
            } else {
                let edit = load::<SftpEditDocument>(id)?;
                cshell_application::SftpWorkspaceSummary {
                    id,
                    session: edit.session,
                    revision: edit.revision,
                    editor: true,
                    state: format!("{:?}", edit.state),
                    path: edit.remote_path,
                }
            };
            if page.all_sessions || summary.session == session {
                items.push(summary);
            }
            if items.len() > 1024 {
                return Err("workspace index limit");
            }
        }
        items.sort_by_key(|i| i.id);
        let offset = page.offset as usize;
        let more = items.len() > offset.saturating_add(64);
        Ok(cshell_application::SftpWorkspaceReply::History {
            session,
            offset: page.offset,
            more,
            items: items.into_iter().skip(offset).take(64).collect(),
        })
    })
    .await
    .map_err(|_| "workspace history worker unavailable")?
}
pub(super) async fn forget<T: Record>(value: T, discard: bool) -> Result<bool, &'static str> {
    tokio::task::spawn_blocking(move || {
        let directory = directory(value.id())?;
        let lease = options()
            .read(true)
            .write(true)
            .create(true)
            .open(directory.join("record.lock"))
            .map_err(|_| "workspace lease unavailable")?;
        lease.try_lock().map_err(|_| "workspace record busy")?;
        if load::<T>(value.id())?.revision() != value.revision() {
            return Err("workspace record changed");
        }
        let mut retained = false;
        for entry in std::fs::read_dir(&directory).map_err(|_| "workspace directory unavailable")? {
            let entry = entry.map_err(|_| "workspace directory unavailable")?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("private file name is not UTF-8")?;
            if name == "record.json" || name == "record.lock" {
                continue;
            }
            if !discard {
                retained = true;
                continue;
            }
            if !(name.starts_with("working-")
                || name.starts_with("upload-") && name.ends_with(".bin"))
                || !regular(
                    &std::fs::symlink_metadata(entry.path())
                        .map_err(|_| "private file unavailable")?,
                )
            {
                return Err("unexpected private workspace entry; no recursive cleanup");
            }
        }
        if discard {
            for entry in
                std::fs::read_dir(&directory).map_err(|_| "workspace directory unavailable")?
            {
                let entry = entry.map_err(|_| "workspace directory unavailable")?;
                let name = entry.file_name();
                if name != "record.json" && name != "record.lock" {
                    std::fs::remove_file(entry.path())
                        .map_err(|_| "private file cleanup unconfirmed")?;
                }
            }
        }
        std::fs::remove_file(directory.join("record.json"))
            .map_err(|_| "workspace metadata removal unconfirmed")?;
        drop(lease);
        if !retained {
            std::fs::remove_file(directory.join("record.lock"))
                .map_err(|_| "workspace lease cleanup unconfirmed")?;
            std::fs::remove_dir(&directory)
                .map_err(|_| "workspace directory cleanup unconfirmed")?;
        }
        #[cfg(unix)]
        std::fs::File::open(root()?)
            .and_then(|f| f.sync_all())
            .map_err(|_| "workspace history sync unconfirmed")?;
        Ok(retained)
    })
    .await
    .map_err(|_| "workspace forgetting worker unavailable")?
}
pub(super) async fn write<T: Record>(value: &T, previous: Option<u64>) -> Result<(), &'static str> {
    let value = value.clone();
    tokio::task::spawn_blocking(move || save(value, previous))
        .await
        .map_err(|_| "workspace worker unavailable")?
}
