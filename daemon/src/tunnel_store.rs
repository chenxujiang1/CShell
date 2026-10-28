//! Private checksummed tunnel catalog with a file lease and revision CAS.
use cshell_application::{MAX_TUNNEL_DOCUMENT, TunnelSnapshot, decode_tunnels};
use cshell_ipc::RuntimePaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::PathBuf,
};
type Result<T> = std::result::Result<T, &'static str>;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    format: String,
    version: u32,
    digest: [u8; 32],
    value: TunnelSnapshot,
}
fn directory() -> Result<PathBuf> {
    let paths = RuntimePaths::for_current_user().map_err(|_| "tunnel storage unavailable")?;
    Ok(RuntimePaths::prepare(paths.root().join("tunnels"))
        .map_err(|_| "tunnel storage unavailable")?
        .root()
        .to_owned())
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
fn options() -> OpenOptions {
    let mut value = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        value
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        value.custom_flags(0x0020_0000);
    }
    value
}
fn load() -> Result<TunnelSnapshot> {
    let path = directory()?.join("catalog.json");
    let before = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TunnelSnapshot::default()),
        Err(_) => return Err("tunnel catalog unavailable"),
    };
    if !regular(&before) || before.len() > (MAX_TUNNEL_DOCUMENT + 1024) as u64 {
        return Err("invalid tunnel catalog file");
    }
    let mut file = options()
        .read(true)
        .open(&path)
        .map_err(|_| "tunnel catalog unavailable")?;
    let after = file.metadata().map_err(|_| "tunnel catalog unavailable")?;
    if !regular(&after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err("tunnel catalog changed");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err("tunnel catalog changed");
        }
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_TUNNEL_DOCUMENT + 1025) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "tunnel catalog read failed")?;
    let stored: Stored =
        serde_json::from_slice(&bytes).map_err(|_| "invalid tunnel catalog storage")?;
    let canonical = serde_json::to_vec(&stored.value).map_err(|_| "tunnel encoding failed")?;
    let digest: [u8; 32] = Sha256::digest(&canonical).into();
    if stored.format != "cshell.tunnel-storage" || stored.version != 1 || stored.digest != digest {
        return Err("tunnel checksum mismatch");
    }
    decode_tunnels(&canonical)
}
fn save(snapshot: TunnelSnapshot, previous: u64) -> Result<()> {
    snapshot.validate()?;
    let directory = directory()?;
    let lease = options()
        .read(true)
        .write(true)
        .create(true)
        .open(directory.join("catalog.lock"))
        .map_err(|_| "tunnel lease unavailable")?;
    if !regular(&lease.metadata().map_err(|_| "tunnel lease unavailable")?) {
        return Err("invalid tunnel lease");
    }
    lease.try_lock().map_err(|_| "tunnel catalog busy")?;
    if load()?.revision != previous || previous.checked_add(1) != Some(snapshot.revision) {
        return Err("tunnel revision conflict");
    }
    let value = serde_json::to_vec(&snapshot).map_err(|_| "tunnel encoding failed")?;
    if value.len() > MAX_TUNNEL_DOCUMENT {
        return Err("tunnel catalog limit");
    }
    let stored = Stored {
        format: "cshell.tunnel-storage".into(),
        version: 1,
        digest: Sha256::digest(value).into(),
        value: snapshot,
    };
    let mut temporary =
        tempfile::NamedTempFile::new_in(&directory).map_err(|_| "tunnel temporary unavailable")?;
    temporary
        .write_all(&serde_json::to_vec(&stored).map_err(|_| "tunnel encoding failed")?)
        .map_err(|_| "tunnel write failed")?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| "tunnel sync failed")?;
    temporary
        .persist(directory.join("catalog.json"))
        .map_err(|_| "tunnel publication unconfirmed")?;
    #[cfg(unix)]
    std::fs::File::open(&directory)
        .and_then(|f| f.sync_all())
        .map_err(|_| "tunnel parent sync unconfirmed")?;
    Ok(())
}
pub(super) async fn read() -> Result<TunnelSnapshot> {
    tokio::task::spawn_blocking(load)
        .await
        .map_err(|_| "tunnel storage worker unavailable")?
}
pub(super) async fn write(value: &TunnelSnapshot, previous: u64) -> Result<()> {
    let value = value.clone();
    tokio::task::spawn_blocking(move || save(value, previous))
        .await
        .map_err(|_| "tunnel storage worker unavailable")?
}
