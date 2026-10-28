//! Rebuild by publishing a new active generation. Previous encrypted files and
//! root-key references remain intact, including after a failed preparation.
use super::*;
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

const POINTER_MAGIC: &[u8] = b"CSHVACT1\n";
pub(super) const DEFAULT_MODE_BINDING: &str = "rebuilt-vault-default-mode-v1";
pub(super) fn default_mode_ref() -> VaultCredentialRef {
    VaultCredentialRef {
        profile_id: [0xff; 16],
        kind: Kind::VaultSettings,
    }
}
fn pointer(base: &Path) -> PathBuf {
    let mut name = base.as_os_str().to_owned();
    name.push(".active");
    PathBuf::from(name)
}
pub(super) fn ownership(base: &Path) -> Result<File, VaultError> {
    if !base.is_absolute() {
        return Err(VaultError::InvalidInput);
    }
    let mut name = base.as_os_str().to_owned();
    name.push(".controller.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(name)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(VaultError::Busy),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}
pub(super) fn active_path(base: &Path) -> Result<PathBuf, VaultError> {
    let marker = pointer(base);
    match std::fs::symlink_metadata(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Once generation switching has started, losing the pointer must
            // not silently reactivate the retained, potentially stale library.
            return if activation_path(&marker).try_exists()? {
                Err(VaultError::ReloadRequired)
            } else {
                Ok(base.to_owned())
            };
        }
        Err(error) => return Err(error.into()),
        Ok(metadata)
            if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 128 =>
        {
            return Err(VaultError::Corrupt);
        }
        Ok(_) => {}
    }
    let mut bytes = Vec::new();
    File::open(marker)?.take(129).read_to_end(&mut bytes)?;
    let name = bytes
        .strip_prefix(POINTER_MAGIC)
        .ok_or(VaultError::Corrupt)?;
    if !(6..=80).contains(&name.len())
        || !name.starts_with(b"vault-")
        || !name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
    {
        return Err(VaultError::Corrupt);
    }
    let directory = base
        .parent()
        .ok_or(VaultError::InvalidInput)?
        .join(std::str::from_utf8(name).map_err(|_| VaultError::Corrupt)?);
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(VaultError::Corrupt);
    }
    Ok(directory.join("master.vault"))
}
pub(super) fn publish(base: &Path, directory: &Path) -> Result<(), VaultError> {
    let parent = base.parent().ok_or(VaultError::InvalidInput)?;
    let name = directory
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(VaultError::InvalidInput)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(POINTER_MAGIC)?;
    temporary.write_all(name.as_bytes())?;
    temporary.as_file().sync_all()?;
    activate(&pointer(base))?;
    temporary
        .persist(pointer(base))
        .map_err(|error| VaultError::from(error.error))?;
    Ok(())
}

impl VaultController {
    pub(crate) fn rebuild(
        &self,
        revision: u64,
        generation: u64,
        password: &Secret,
        system: bool,
        confirmed: bool,
    ) -> Result<(), VaultError> {
        self.rebuild_with(revision, generation, password, system, confirmed, publish)
    }
    fn rebuild_with(
        &self,
        revision: u64,
        generation: u64,
        password: &Secret,
        system: bool,
        confirmed: bool,
        publish: impl FnOnce(&Path, &Path) -> Result<(), VaultError>,
    ) -> Result<(), VaultError> {
        let _transition=self.transition_gate.try_lock().map_err(|_|VaultError::Busy)?;
        if !confirmed
            || (system && !password.expose().is_empty())
            || (!system && !(8..=1024).contains(&password.expose().len()))
        {
            return Err(VaultError::InvalidInput);
        }
        self.ensure_owned()?;
        {
            let state = self.guard();
            if matches!(state.backend, Backend::Native) {
                return Err(VaultError::InvalidInput);
            }
            if matches!(state.backend, Backend::Unavailable(VaultError::Busy)) {
                return Err(VaultError::Busy);
            }
            if let Backend::Portable(vault) = &state.backend {
                check_revision(vault, revision)?;
                if vault.status().unlocked {
                    return Err(VaultError::InvalidInput);
                }
            } else if revision != 0 {
                return Err(VaultError::AlreadyExists);
            }
        }
        let epoch = self.begin_unlock(generation)?;
        let parent = self.path.parent().ok_or(VaultError::InvalidInput)?;
        let stage = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(parent)?;
        cshell_ipc::RuntimePaths::prepare(stage.path().to_owned())
            .map_err(|_| VaultError::Io(std::io::ErrorKind::PermissionDenied))?;
        let directory = stage.keep();
        let path = directory.join("master.vault");
        let root = system.then(VaultRootRef::new);
        // Persist the opaque reference before touching the keychain so a partial
        // keychain failure remains identifiable without affecting the old root.
        if let Some(root) = root {
            persist_root(&path, root)?;
        }
        let system_secret = root
            .map(SystemVaultRootKeys::create)
            .transpose()
            .map_err(|_| VaultError::RootKeyUnavailable)?;
        let mut vault = PortableVault::create(&path, system_secret.as_ref().unwrap_or(password))?;
        // Missing credentials after a rebuild must never fall back silently to
        // stale system-keychain passwords or old private-key files.
        vault.write(
            default_mode_ref(),
            DEFAULT_MODE_BINDING,
            &Secret::new(vec![1]),
        )?;
        activate(&path)?;
        // All subsequent failures retain this prepared encrypted generation and
        // its root reference, rather than deleting credentials on an uncertain outcome.
        self.commit_auth(epoch, || {
            let mut state = self.guard();
            if let Backend::Portable(old) = &state.backend {
                check_revision(old, revision)?;
                if old.status().unlocked {
                    return Err(VaultError::AlreadyExists);
                }
            } else if !matches!(state.backend, Backend::Unavailable(_)) || revision != 0 {
                return Err(VaultError::AlreadyExists);
            }
            if let Err(error) = publish(&self.path, &directory) {
                // A publisher may report uncertainty after replacement. Fail closed;
                // an explicit reload resolves the active pointer without replaying reset.
                let active = active_path(&self.path);
                if active.as_ref().map_or(true, |active| active != &state.path) {
                    state.backend = Backend::Unavailable(VaultError::WriteUncertain);
                    if active.ok().as_ref() == Some(&path) {
                        state.path = path.clone();
                        state.root = root;
                    }
                    state.unlocked_at = None;
                    return Err(VaultError::WriteUncertain);
                }
                return Err(error);
            }
            state.path = path;
            state.root = root;
            state.unlocked_at = Some(Instant::now());
            self.manually_locked.store(false,Ordering::Release);
            self.enabled.store(true, Ordering::Release);
            #[cfg(unix)]
            if File::open(parent).and_then(|f| f.sync_all()).is_err() {
                vault.lock();
                state.unlocked_at = None;
                state.backend = Backend::Portable(Box::new(vault));
                return Err(VaultError::WriteUncertain);
            }
            state.backend = Backend::Portable(Box::new(vault));
            Ok(())
        })?
    }
}

#[cfg(test)]
mod tests;
