//! Private checksummed host CA catalog with a file lease and revision CAS.
use cshell_application::{HostCaCatalog, MAX_HOST_CA_DOCUMENT, decode_host_ca_catalog};
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
    value: HostCaCatalog,
}
fn directory() -> Result<PathBuf> {
    let paths = RuntimePaths::for_current_user().map_err(|_| "host CA storage unavailable")?;
    Ok(RuntimePaths::prepare(paths.root().join("host-ca"))
        .map_err(|_| "host CA storage unavailable")?
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
fn load() -> Result<HostCaCatalog> {
    let path = directory()?.join("catalog.json");
    let before = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HostCaCatalog::default()),
        Err(_) => return Err("host CA catalog unavailable"),
    };
    if !regular(&before) || before.len() > (MAX_HOST_CA_DOCUMENT + 1024) as u64 {
        return Err("invalid host CA catalog file");
    }
    let mut file = options()
        .read(true)
        .open(&path)
        .map_err(|_| "host CA catalog unavailable")?;
    let after = file.metadata().map_err(|_| "host CA catalog unavailable")?;
    if !regular(&after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err("host CA catalog changed");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err("host CA catalog changed");
        }
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_HOST_CA_DOCUMENT + 1025) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "host CA catalog read failed")?;
    let stored: Stored =
        serde_json::from_slice(&bytes).map_err(|_| "invalid host CA catalog storage")?;
    let canonical = serde_json::to_vec(&stored.value).map_err(|_| "host CA encoding failed")?;
    let digest: [u8; 32] = Sha256::digest(&canonical).into();
    if stored.format != "cshell.host-ca-storage" || stored.version != 1 || stored.digest != digest {
        return Err("host CA checksum mismatch");
    }
    let value = decode_host_ca_catalog(&canonical)?;
    for row in &value.items {
        let (key, fingerprint) = cshell_ssh::normalize_host_authority(
            &row.definition.patterns,
            &row.definition.public_key,
        )?;
        if key != row.definition.public_key || fingerprint != row.fingerprint {
            return Err("host CA key/fingerprint storage mismatch");
        }
    }
    Ok(value)
}
fn save(snapshot: HostCaCatalog, previous: u64) -> Result<()> {
    snapshot.validate()?;
    let directory = directory()?;
    let lease = options()
        .read(true)
        .write(true)
        .create(true)
        .open(directory.join("catalog.lock"))
        .map_err(|_| "host CA lease unavailable")?;
    if !regular(&lease.metadata().map_err(|_| "host CA lease unavailable")?) {
        return Err("invalid host CA lease");
    }
    lease.try_lock().map_err(|_| "host CA catalog busy")?;
    if load()?.revision != previous || previous.checked_add(1) != Some(snapshot.revision) {
        return Err("host CA revision conflict");
    }
    let value = serde_json::to_vec(&snapshot).map_err(|_| "host CA encoding failed")?;
    if value.len() > MAX_HOST_CA_DOCUMENT {
        return Err("host CA catalog limit");
    }
    let stored = Stored {
        format: "cshell.host-ca-storage".into(),
        version: 1,
        digest: Sha256::digest(value).into(),
        value: snapshot,
    };
    let mut temporary =
        tempfile::NamedTempFile::new_in(&directory).map_err(|_| "host CA temporary unavailable")?;
    temporary
        .write_all(&serde_json::to_vec(&stored).map_err(|_| "host CA encoding failed")?)
        .map_err(|_| "host CA write failed")?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| "host CA sync failed")?;
    temporary
        .persist(directory.join("catalog.json"))
        .map_err(|_| "host CA publication unconfirmed")?;
    #[cfg(unix)]
    std::fs::File::open(&directory)
        .and_then(|f| f.sync_all())
        .map_err(|_| "host CA parent sync unconfirmed")?;
    Ok(())
}
pub(super) async fn read() -> Result<HostCaCatalog> {
    tokio::task::spawn_blocking(load)
        .await
        .map_err(|_| "host CA storage worker unavailable")?
}
pub(super) async fn write(value: &HostCaCatalog, previous: u64) -> Result<()> {
    let value = value.clone();
    tokio::task::spawn_blocking(move || save(value, previous))
        .await
        .map_err(|_| "host CA storage worker unavailable")?
}

pub(super) fn coordination() -> std::sync::Arc<tokio::sync::RwLock<()>> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::RwLock<()>>> =
        std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Arc::new(tokio::sync::RwLock::new(())))
        .clone()
}

pub(super) async fn verifier(
    path: &std::path::Path,
    host: &str,
    port: u16,
) -> Result<cshell_ssh::KnownHostsVerifier> {
    let policy = read().await?;
    let path = path.to_owned();
    let host = host.to_owned();
    tokio::task::spawn_blocking(move || {
        let rules: Vec<_> = policy
            .items
            .into_iter()
            .filter(|r| r.definition.state != cshell_application::HostCaState::Disabled)
            .map(|r| cshell_ssh::HostAuthorityRule {
                patterns: r.definition.patterns,
                public_key: r.definition.public_key,
                revoked: r.definition.state == cshell_application::HostCaState::Revoked,
            })
            .collect();
        let verifier = cshell_ssh::KnownHostsVerifier::load_or_empty(path, &host, port)
            .map_err(|_| "known_hosts unavailable or corrupt")?;
        verifier.with_managed_authorities(&rules)
    })
    .await
    .map_err(|_| "effective trust worker unavailable")?
}
