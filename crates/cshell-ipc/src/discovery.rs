use crate::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use prost::Message;
use rand::RngExt;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const DISCOVERY_SCHEMA_VERSION: u32 = 1;
const MAX_DISCOVERY_BYTES: u64 = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePaths {
    root: PathBuf,
}

impl RuntimePaths {
    pub fn for_current_user() -> Result<Self, DiscoveryError> {
        if let Some(root) = std::env::var_os("CSHELL_RUNTIME_DIR") {
            return Self::prepare(PathBuf::from(root));
        }

        #[cfg(windows)]
        let root = required_environment_path("LOCALAPPDATA")?
            .join("CShell")
            .join("runtime");
        #[cfg(target_os = "macos")]
        let root = required_environment_path("HOME")?
            .join("Library")
            .join("Application Support")
            .join("CShell")
            .join("runtime");
        #[cfg(all(unix, not(target_os = "macos")))]
        let root = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
            || {
                required_environment_path("HOME").map(|home| {
                    home.join(".local")
                        .join("state")
                        .join("cshell")
                        .join("runtime")
                })
            },
            |runtime| Ok(PathBuf::from(runtime).join("cshell")),
        )?;

        Self::prepare(root)
    }

    pub fn prepare(root: PathBuf) -> Result<Self, DiscoveryError> {
        if root.as_os_str().is_empty() {
            return Err(DiscoveryError::InvalidRuntimeDirectory);
        }
        std::fs::create_dir_all(&root).map_err(DiscoveryError::RuntimeDirectory)?;
        reject_symlink(&root)?;
        secure_runtime_directory(&root)?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn discovery_file(&self) -> PathBuf {
        self.root.join("daemon.discovery")
    }

    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    #[must_use]
    pub fn journal_root(&self) -> PathBuf {
        self.root.join("journals")
    }

    #[cfg(windows)]
    #[must_use]
    pub fn endpoint(&self, instance_id: &[u8; 16]) -> OsString {
        OsString::from(format!(r"\\.\pipe\cshell-{}", encode_hex(instance_id)))
    }

    #[cfg(unix)]
    #[must_use]
    pub fn endpoint(&self, _instance_id: &[u8; 16]) -> OsString {
        self.root.join("daemon.sock").into_os_string()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryRecord {
    pub endpoint: OsString,
    pub daemon_instance_id: [u8; 16],
    pub instance_token: [u8; 32],
    pub daemon_pid: u32,
    pub started_unix_ms: u64,
}

impl DiscoveryRecord {
    #[must_use]
    pub fn generate(paths: &RuntimePaths) -> Self {
        let mut instance_id = [0_u8; 16];
        let mut instance_token = [0_u8; 32];
        let mut rng = rand::rng();
        rng.fill(&mut instance_id);
        rng.fill(&mut instance_token);
        Self {
            endpoint: paths.endpoint(&instance_id),
            daemon_instance_id: instance_id,
            instance_token,
            daemon_pid: std::process::id(),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| {
                    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                }),
        }
    }

    pub fn load(paths: &RuntimePaths) -> Result<Self, DiscoveryError> {
        let path = paths.discovery_file();
        reject_symlink(&path)?;
        validate_private_file(&path)?;
        let metadata = std::fs::metadata(&path).map_err(DiscoveryError::Read)?;
        if metadata.len() > MAX_DISCOVERY_BYTES {
            return Err(DiscoveryError::Oversized(metadata.len()));
        }
        let mut encoded = Vec::with_capacity(metadata.len() as usize);
        File::open(&path)
            .and_then(|mut file| file.read_to_end(&mut encoded))
            .map_err(DiscoveryError::Read)?;
        let stored =
            StoredDiscovery::decode(encoded.as_slice()).map_err(DiscoveryError::MalformedRecord)?;
        let record = Self::try_from(stored)?;
        if record.endpoint != paths.endpoint(&record.daemon_instance_id) {
            return Err(DiscoveryError::InvalidField("endpoint binding"));
        }
        Ok(record)
    }

    fn stored(&self) -> StoredDiscovery {
        StoredDiscovery {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            endpoint: endpoint_bytes(&self.endpoint),
            daemon_instance_id: self.daemon_instance_id.to_vec(),
            instance_token: self.instance_token.to_vec(),
            daemon_pid: self.daemon_pid,
            started_unix_ms: self.started_unix_ms,
        }
    }
}

#[derive(Debug)]
pub struct SingleInstanceGuard {
    _file: File,
}

impl SingleInstanceGuard {
    pub fn acquire(paths: &RuntimePaths) -> Result<Self, DiscoveryError> {
        let path = paths.lock_file();
        reject_symlink_if_present(&path)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        configure_private_create(&mut options);
        let mut file = options.open(&path).map_err(DiscoveryError::Lock)?;
        #[cfg(windows)]
        crate::windows_security::secure_path(&path, false).map_err(DiscoveryError::Lock)?;
        File::try_lock(&file).map_err(|error| {
            let error: std::io::Error = error.into();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                DiscoveryError::AlreadyRunning
            } else {
                DiscoveryError::Lock(error)
            }
        })?;
        file.set_len(0).map_err(DiscoveryError::Lock)?;
        writeln!(file, "{}", std::process::id()).map_err(DiscoveryError::Lock)?;
        file.sync_data().map_err(DiscoveryError::Lock)?;
        Ok(Self { _file: file })
    }
}

#[derive(Debug)]
pub struct DiscoveryPublication {
    path: PathBuf,
    daemon_instance_id: [u8; 16],
}

impl DiscoveryPublication {
    pub fn publish(paths: &RuntimePaths, record: &DiscoveryRecord) -> Result<Self, DiscoveryError> {
        let path = paths.discovery_file();
        reject_symlink_if_present(&path)?;
        let temporary = paths.root().join(format!(
            ".daemon.discovery.{}.tmp",
            encode_hex(&record.daemon_instance_id)
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        configure_private_create(&mut options);
        let encoded = record.stored().encode_to_vec();
        let mut file = options.open(&temporary).map_err(DiscoveryError::Publish)?;
        #[cfg(windows)]
        crate::windows_security::secure_path(&temporary, false).map_err(DiscoveryError::Publish)?;
        let write_result = file
            .write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(DiscoveryError::Publish);
        if let Err(error) = write_result {
            let _result = std::fs::remove_file(&temporary);
            return Err(error);
        }
        if path.exists() {
            std::fs::remove_file(&path).map_err(DiscoveryError::Publish)?;
        }
        std::fs::rename(&temporary, &path).map_err(DiscoveryError::Publish)?;
        validate_private_file(&path)?;
        Ok(Self {
            path,
            daemon_instance_id: record.daemon_instance_id,
        })
    }
}

impl Drop for DiscoveryPublication {
    fn drop(&mut self) {
        let should_remove = std::fs::read(&self.path)
            .ok()
            .and_then(|encoded| StoredDiscovery::decode(encoded.as_slice()).ok())
            .is_some_and(|stored| stored.daemon_instance_id == self.daemon_instance_id);
        if should_remove {
            let _result = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("runtime directory environment variable {0} is unavailable")]
    MissingEnvironment(&'static str),
    #[error("runtime directory path is empty")]
    InvalidRuntimeDirectory,
    #[error("cannot prepare runtime directory: {0}")]
    RuntimeDirectory(std::io::Error),
    #[error("runtime or discovery path must not be a symbolic link")]
    SymbolicLink,
    #[error("another cshelld instance already owns this user runtime")]
    AlreadyRunning,
    #[error("cannot acquire daemon instance lock: {0}")]
    Lock(std::io::Error),
    #[error("cannot publish daemon discovery record: {0}")]
    Publish(std::io::Error),
    #[error("cannot read daemon discovery record: {0}")]
    Read(std::io::Error),
    #[error("daemon discovery record is too large: {0} bytes")]
    Oversized(u64),
    #[error("daemon discovery record is malformed: {0}")]
    MalformedRecord(prost::DecodeError),
    #[error("daemon discovery record uses unsupported schema {0}")]
    UnsupportedSchema(u32),
    #[error("daemon discovery record is for unsupported protocol major {0}")]
    UnsupportedProtocol(u32),
    #[error("daemon discovery record contains an invalid {0}")]
    InvalidField(&'static str),
    #[cfg(unix)]
    #[error("runtime metadata permissions are too broad: {0:o}")]
    InsecurePermissions(u32),
}

#[derive(Clone, PartialEq, Message)]
struct StoredDiscovery {
    #[prost(uint32, tag = "1")]
    schema_version: u32,
    #[prost(uint32, tag = "2")]
    protocol_major: u32,
    #[prost(uint32, tag = "3")]
    protocol_minor: u32,
    #[prost(bytes = "vec", tag = "4")]
    endpoint: Vec<u8>,
    #[prost(bytes = "vec", tag = "5")]
    daemon_instance_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "6")]
    instance_token: Vec<u8>,
    #[prost(uint32, tag = "7")]
    daemon_pid: u32,
    #[prost(fixed64, tag = "8")]
    started_unix_ms: u64,
}

impl TryFrom<StoredDiscovery> for DiscoveryRecord {
    type Error = DiscoveryError;

    fn try_from(stored: StoredDiscovery) -> Result<Self, Self::Error> {
        if stored.schema_version != DISCOVERY_SCHEMA_VERSION {
            return Err(DiscoveryError::UnsupportedSchema(stored.schema_version));
        }
        if stored.protocol_major != PROTOCOL_MAJOR {
            return Err(DiscoveryError::UnsupportedProtocol(stored.protocol_major));
        }
        let daemon_instance_id = stored
            .daemon_instance_id
            .try_into()
            .map_err(|_| DiscoveryError::InvalidField("daemon instance ID"))?;
        let instance_token = stored
            .instance_token
            .try_into()
            .map_err(|_| DiscoveryError::InvalidField("instance token"))?;
        if stored.daemon_pid == 0 {
            return Err(DiscoveryError::InvalidField("daemon PID"));
        }
        let endpoint = endpoint_from_bytes(stored.endpoint)?;
        validate_endpoint(&endpoint)?;
        Ok(Self {
            endpoint,
            daemon_instance_id,
            instance_token,
            daemon_pid: stored.daemon_pid,
            started_unix_ms: stored.started_unix_ms,
        })
    }
}

fn required_environment_path(name: &'static str) -> Result<PathBuf, DiscoveryError> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(DiscoveryError::MissingEnvironment(name))
}

fn reject_symlink(path: &Path) -> Result<(), DiscoveryError> {
    if std::fs::symlink_metadata(path)
        .map_err(DiscoveryError::Read)?
        .file_type()
        .is_symlink()
    {
        return Err(DiscoveryError::SymbolicLink);
    }
    Ok(())
}

fn reject_symlink_if_present(path: &Path) -> Result<(), DiscoveryError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(DiscoveryError::SymbolicLink),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DiscoveryError::Read(error)),
    }
}

#[cfg(unix)]
fn secure_runtime_directory(path: &Path) -> Result<(), DiscoveryError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(DiscoveryError::RuntimeDirectory)?;
    let mode = std::fs::metadata(path)
        .map_err(DiscoveryError::RuntimeDirectory)?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(DiscoveryError::InsecurePermissions(mode));
    }
    Ok(())
}

#[cfg(windows)]
fn secure_runtime_directory(path: &Path) -> Result<(), DiscoveryError> {
    crate::windows_security::secure_path(path, true).map_err(DiscoveryError::RuntimeDirectory)
}

#[cfg(unix)]
fn configure_private_create(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(windows)]
fn configure_private_create(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn validate_private_file(path: &Path) -> Result<(), DiscoveryError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(DiscoveryError::Read)?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(DiscoveryError::InsecurePermissions(mode));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_private_file(path: &Path) -> Result<(), DiscoveryError> {
    crate::windows_security::validate_path(path, false).map_err(DiscoveryError::Read)
}

#[cfg(unix)]
fn endpoint_bytes(endpoint: &OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    endpoint.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn endpoint_bytes(endpoint: &OsString) -> Vec<u8> {
    endpoint.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn endpoint_from_bytes(endpoint: Vec<u8>) -> Result<OsString, DiscoveryError> {
    use std::os::unix::ffi::OsStringExt;
    if endpoint.is_empty() || endpoint.contains(&0) {
        return Err(DiscoveryError::InvalidField("endpoint"));
    }
    Ok(OsString::from_vec(endpoint))
}

#[cfg(windows)]
fn endpoint_from_bytes(endpoint: Vec<u8>) -> Result<OsString, DiscoveryError> {
    let endpoint = String::from_utf8(endpoint)
        .map_err(|_| DiscoveryError::InvalidField("endpoint encoding"))?;
    if endpoint.is_empty() || endpoint.contains('\0') {
        return Err(DiscoveryError::InvalidField("endpoint"));
    }
    Ok(OsString::from(endpoint))
}

#[cfg(windows)]
fn validate_endpoint(endpoint: &OsString) -> Result<(), DiscoveryError> {
    if !endpoint.to_string_lossy().starts_with(r"\\.\pipe\cshell-") {
        return Err(DiscoveryError::InvalidField("named pipe endpoint"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_endpoint(endpoint: &OsString) -> Result<(), DiscoveryError> {
    if !Path::new(endpoint).is_absolute() {
        return Err(DiscoveryError::InvalidField("Unix socket endpoint"));
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        DiscoveryError, DiscoveryPublication, DiscoveryRecord, RuntimePaths, SingleInstanceGuard,
    };

    #[test]
    fn publication_round_trip_and_cleanup_preserve_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::prepare(directory.path().join("runtime")).unwrap();
        let record = DiscoveryRecord::generate(&paths);
        let publication = DiscoveryPublication::publish(&paths, &record).unwrap();
        assert_eq!(DiscoveryRecord::load(&paths).unwrap(), record);
        drop(publication);
        assert!(!paths.discovery_file().exists());
    }

    #[test]
    fn only_one_single_instance_guard_can_hold_the_runtime_lock() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::prepare(directory.path().join("runtime")).unwrap();
        let first = SingleInstanceGuard::acquire(&paths).unwrap();
        assert!(matches!(
            SingleInstanceGuard::acquire(&paths),
            Err(DiscoveryError::AlreadyRunning)
        ));
        drop(first);
        SingleInstanceGuard::acquire(&paths).unwrap();
    }

    #[test]
    fn discovery_endpoint_is_bound_to_its_instance_and_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::prepare(directory.path().join("runtime")).unwrap();
        let mut record = DiscoveryRecord::generate(&paths);
        #[cfg(windows)]
        {
            record.endpoint = std::ffi::OsString::from(r"\\.\pipe\cshell-tampered");
        }
        #[cfg(unix)]
        {
            record.endpoint = directory.path().join("tampered.sock").into_os_string();
        }
        let _publication = DiscoveryPublication::publish(&paths, &record).unwrap();
        assert!(matches!(
            DiscoveryRecord::load(&paths),
            Err(DiscoveryError::InvalidField("endpoint binding"))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_group_readable_records() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::prepare(directory.path().join("runtime")).unwrap();
        let record = DiscoveryRecord::generate(&paths);
        let publication = DiscoveryPublication::publish(&paths, &record).unwrap();
        std::fs::set_permissions(
            paths.discovery_file(),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert!(matches!(
            DiscoveryRecord::load(&paths),
            Err(DiscoveryError::InsecurePermissions(_))
        ));
        drop(publication);
    }
}
