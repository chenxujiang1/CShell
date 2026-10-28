use cshell_domain::{ProfileId, SshConnectionRecord};
use cshell_ipc::{VaultControlStatus, VaultResponse, VaultSessionState};
use cshell_vault::{
    PortableVault, ProfileKeyPassphraseRef, ProfilePasswordBinding, ProfilePasswordRef, Secret,
    SystemProfileKeyPassphraseVault, SystemProfilePasswordVault, SystemVaultRootKeys,
    VaultCredentialKind as Kind, VaultCredentialRef, VaultError, VaultRootRef,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

mod package;
mod managed_keys;
mod rebuild;
mod protection;

const MODE_BINDING: &str = "profile-credential-mode-v1";
#[cfg(test)]
mod tests;
enum Backend {
    Native,
    Portable(Box<PortableVault>),
    Unavailable(VaultError),
}
struct State {
    path: PathBuf,
    backend: Backend,
    // Diagnostic unlock timestamp only; no deadline or idle expiry.
    unlocked_at: Option<Instant>,
    root: Option<VaultRootRef>,
}
pub(crate) struct VaultController {
    pub(crate) keyboard_auth: Arc<crate::keyboard_auth::Broker>,
    path: PathBuf,
    _ownership: Option<std::fs::File>,
    state: Mutex<State>,
    epoch: AtomicU64,
    changes: tokio::sync::watch::Sender<u64>,
    enabled: AtomicBool,
    commit_gate: Mutex<()>,
    session_state: AtomicU64,
    manually_locked: AtomicBool,
    access_gate: tokio::sync::Mutex<()>,
    transition_gate: Mutex<()>,
}
impl std::fmt::Debug for VaultController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultController").finish_non_exhaustive()
    }
}
pub(crate) fn reference(id: ProfileId, kind: Kind) -> VaultCredentialRef {
    VaultCredentialRef {
        profile_id: *id.as_uuid().as_bytes(),
        kind,
    }
}
pub(crate) fn binding(target: &SshConnectionRecord, kind: Kind) -> Result<String, VaultError> {
    // A versioned tuple prevents delimiter collisions and binds imported keys to their target.
    serde_json::to_string(&(
        1u8,
        kind as u8,
        &target.host,
        target.port,
        &target.username,
        target.auth_method,
        &target.private_key_path,
        &target.certificate_path,
        &target.route,
    ))
    .map_err(|_| VaultError::InvalidInput)
}
impl VaultController {
    pub(crate) fn new(path: PathBuf) -> Arc<Self> {
        let prepared = path
            .parent()
            .ok_or(VaultError::InvalidInput)
            .and_then(|parent| {
                cshell_ipc::RuntimePaths::prepare(parent.to_owned())
                    .map_err(|_| VaultError::Io(std::io::ErrorKind::PermissionDenied))
            });
        let ownership = prepared.and_then(|_| rebuild::ownership(&path));
        let active = rebuild::active_path(&path);
        let active_path = active.as_ref().cloned().unwrap_or_else(|_| path.clone());
        let root = read_root(&active_path);
        let backend = if let Err(error) = &ownership {
            Backend::Unavailable(*error)
        } else if let Err(error) = active {
            Backend::Unavailable(error)
        } else if let Err(error) = &root {
            Backend::Unavailable(*error)
        } else {
            match active_path.try_exists().and_then(|exists| {
                activation_path(&active_path)
                    .try_exists()
                    .map(|active| exists || active || root.as_ref().is_ok_and(Option::is_some))
            }) {
                Ok(false) if active_path != path => Backend::Unavailable(VaultError::ReloadRequired),
                Ok(false) => Backend::Native,
                Ok(true) => match PortableVault::open(&active_path) {
                    Ok(v) => Backend::Portable(Box::new(v)),
                    Err(e) => Backend::Unavailable(e),
                },
                Err(e) => Backend::Unavailable(e.into()),
            }
        };
        let (changes, _) = tokio::sync::watch::channel(0);
        let enabled = AtomicBool::new(!matches!(backend, Backend::Native));
        Arc::new(Self {
            keyboard_auth: Arc::new(crate::keyboard_auth::Broker::default()),
            path,
            _ownership: ownership.ok(),
            state: Mutex::new(State {
                path: active_path,
                backend,
                unlocked_at: None,
                root: root.unwrap_or(None),
            }),
            epoch: AtomicU64::new(0),
            changes,
            enabled,
            commit_gate: Mutex::new(()),
            session_state: AtomicU64::new(VaultSessionState::Unsupported as u64),
            manually_locked: AtomicBool::new(false),
            access_gate: tokio::sync::Mutex::new(()),
            transition_gate: Mutex::new(()),
        })
    }
    fn guard(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(crate) fn session_state(&self) -> VaultSessionState {
        VaultSessionState::try_from(self.session_state.load(Ordering::Acquire) as i32)
            .unwrap_or(VaultSessionState::Unavailable)
    }
    pub(crate) fn ensure_owned(&self) -> Result<(), VaultError> {
        if self._ownership.is_none() { return Err(VaultError::Busy); }
        Ok(())
    }
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    pub(crate) fn observe_desktop_session(&self, observed: VaultSessionState) {
        // Desktop monitoring is diagnostic only. Unlock lasts for this program run;
        // screen lock, suspend and unavailable monitoring must not revoke credentials.
        self.session_state.store(observed as u64, Ordering::Release);
    }
    pub(crate) fn cancellation(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }
    pub(crate) fn invalidate(&self) {
        let _gate = self
            .commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        self.changes.send_replace(epoch);
    }
    pub(crate) fn commit_auth<T>(
        &self,
        epoch: u64,
        commit: impl FnOnce() -> T,
    ) -> Result<T, VaultError> {
        let _gate = self
            .commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.epoch.load(Ordering::Acquire) != epoch {
            return Err(VaultError::Locked);
        }
        Ok(commit())
    }
    fn begin_unlock(&self, expected: u64) -> Result<u64, VaultError> {
        if self._ownership.is_none() { return Err(VaultError::Busy); }
        let _gate = self
            .commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.epoch.load(Ordering::Acquire) != expected {
            return Err(VaultError::AlreadyExists);
        }
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        self.changes.send_replace(epoch);
        Ok(epoch)
    }
    pub(crate) fn lock(&self) {
        self.invalidate();
        let mut state = self.guard();
        if let Backend::Portable(vault) = &mut state.backend {
            vault.lock();
        }
        state.unlocked_at = None;
    }
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
    pub(crate) fn create(&self, password: &Secret, expected: u64) -> Result<(), VaultError> {
        let epoch = self.begin_unlock(expected)?;
        let mut state = self.guard();
        if !matches!(state.backend, Backend::Native) {
            return Err(VaultError::AlreadyExists);
        }
        let mut vault = match PortableVault::create(&state.path, password) {
            Ok(vault) => vault,
            Err(error) => {
                if self.path.try_exists().unwrap_or(true) {
                    self.enabled.store(true, Ordering::Release);
                    state.backend = Backend::Unavailable(error);
                }
                return Err(error);
            }
        };
        self.enabled.store(true, Ordering::Release);
        if let Err(error) = activate(&state.path) {
            state.backend = Backend::Unavailable(error);
            return Err(error);
        }
        if self.epoch.load(Ordering::Acquire) != epoch {
            vault.lock();
        }
        state.unlocked_at = vault.status().unlocked.then(Instant::now);
        state.backend = Backend::Portable(Box::new(vault));
        if state.unlocked_at.is_none() {
            return Err(VaultError::Locked);
        }
        self.manually_locked.store(false,Ordering::Release);
        Ok(())
    }
    pub(crate) fn create_system(&self, expected: u64) -> Result<(), VaultError> {
        let epoch = self.begin_unlock(expected)?;
        let mut state = self.guard();
        if !matches!(state.backend, Backend::Native) {
            return Err(VaultError::AlreadyExists);
        }
        let id = VaultRootRef::new();
        let secret = match SystemVaultRootKeys::create(id) {
            Ok(secret) => secret,
            Err(error) => {
                if error == cshell_vault::KeychainError::CleanupRequired {
                    state.root = Some(id);
                    let _ = persist_root(&state.path, id);
                    state.backend = Backend::Unavailable(VaultError::Io(std::io::ErrorKind::Other));
                    self.enabled.store(true, Ordering::Release);
                }
                return Err(VaultError::Io(std::io::ErrorKind::Other));
            }
        };
        if let Err(error) = persist_root(&state.path, id) {
            if read_root(&state.path)
                .ok()
                .flatten()
                .is_some_and(|saved| saved.bytes() == id.bytes())
            {
                state.root = Some(id);
            } else if SystemVaultRootKeys::delete(id).is_ok()
                && !root_path(&state.path).try_exists().unwrap_or(true)
            {
                return Err(error);
            } else {
                state.root = Some(id);
            }
            state.backend = Backend::Unavailable(error);
            self.enabled.store(true, Ordering::Release);
            return Err(error);
        }
        state.root = Some(id);
        self.enabled.store(true, Ordering::Release);
        let mut vault = match PortableVault::create(&state.path, &secret) {
            Ok(vault) => vault,
            Err(error) => {
                state.backend = Backend::Unavailable(error);
                return Err(error);
            }
        };
        if let Err(error) = activate(&state.path) {
            state.backend = Backend::Unavailable(error);
            return Err(error);
        }
        if self.epoch.load(Ordering::Acquire) != epoch {
            vault.lock();
        }
        state.unlocked_at = vault.status().unlocked.then(Instant::now);
        state.backend = Backend::Portable(Box::new(vault));
        if state.unlocked_at.is_none() {
            return Err(VaultError::Locked);
        }
        self.manually_locked.store(false,Ordering::Release);
        Ok(())
    }
    pub(crate) fn unlock_system(&self, expected: u64) -> Result<(), VaultError> {
        self.unlock_system_with(expected, SystemVaultRootKeys::read)
    }
    fn unlock_system_with(&self, expected: u64, read: impl FnOnce(VaultRootRef) -> Result<Secret,cshell_vault::KeychainError>) -> Result<(), VaultError> {
        let epoch = self.begin_unlock(expected)?;
        let mut state = self.guard();
        state.unlocked_at = None;
        if let Backend::Portable(vault) = &mut state.backend {
            vault.lock();
        }
        let id = state.root.ok_or(VaultError::InvalidInput)?;
        let vault = portable(&mut state)?;
        vault.lock();
        let secret = read(id).map_err(|error| match error {
            cshell_vault::KeychainError::Missing => VaultError::RootKeyMissing,
            _ => VaultError::RootKeyUnavailable,
        })?;
        vault.unlock(&secret)?;
        if self.epoch.load(Ordering::Acquire) != epoch {
            vault.lock();
            return Err(VaultError::Locked);
        }
        state.unlocked_at = Some(Instant::now());
        self.manually_locked.store(false,Ordering::Release);
        Ok(())
    }
    pub(crate) fn unlock(&self, password: &Secret, expected: u64) -> Result<(), VaultError> {
        let epoch = self.begin_unlock(expected)?;
        let mut state = self.guard();
        state.unlocked_at = None;
        if let Backend::Portable(vault) = &mut state.backend {
            vault.lock();
        }
        if state.root.is_some() {
            return Err(VaultError::InvalidInput);
        }
        let Backend::Portable(vault) = &mut state.backend else {
            return Err(VaultError::ReloadRequired);
        };
        vault.unlock(password)?;
        if self.epoch.load(Ordering::Acquire) != epoch {
            vault.lock();
            return Err(VaultError::Locked);
        }
        state.unlocked_at = Some(Instant::now());
        self.manually_locked.store(false,Ordering::Release);
        Ok(())
    }
    pub(crate) fn reload(&self) -> Result<(), VaultError> {
        if self._ownership.is_none() { return Err(VaultError::Busy); }
        if !self.enabled() { return Err(VaultError::InvalidInput); }
        self.lock();
        let mut state = self.guard();
        let path = match rebuild::active_path(&self.path) {
            Ok(path) => path,
            Err(error) => { state.backend = Backend::Unavailable(error); return Err(error); }
        };
        cshell_ipc::RuntimePaths::prepare(path.parent().ok_or(VaultError::InvalidInput)?.to_owned())
            .map_err(|_| VaultError::Io(std::io::ErrorKind::PermissionDenied))?;
        let switched = state.path != path;
        if !root_path(&path).try_exists()? && !path.try_exists()? && !switched && let Some(id) = state.root {
            persist_root(&path, id)?;
        }
        state.path = path;
        state.root = match read_root(&state.path) {
            Ok(root) => root,
            Err(error) => { state.backend = Backend::Unavailable(error); return Err(error); }
        };
        if !state.path.try_exists()? && !activation_path(&state.path).try_exists()? && let Some(id) = state.root {
            let secret = SystemVaultRootKeys::read(id).map_err(|_| VaultError::RootKeyUnavailable)?;
            let mut vault = PortableVault::create(&state.path, &secret)?;
            activate(&state.path)?; vault.lock(); state.backend = Backend::Portable(Box::new(vault));
            return Ok(());
        }
        if !switched && let Backend::Portable(vault) = &mut state.backend { return vault.reload(); }
        match PortableVault::open(&state.path) {
            Ok(vault) => { state.backend = Backend::Portable(Box::new(vault)); Ok(()) }
            Err(error) => { state.backend = Backend::Unavailable(error); Err(error) }
        }
    }
    pub(crate) fn rotate(
        &self,
        revision: u64,
        password: &Secret,
        expected: u64,
    ) -> Result<(), VaultError> {
        let epoch = self.begin_unlock(expected)?;
        let mut state = self.guard();
        if state.root.is_some() {
            return Err(VaultError::InvalidInput);
        }
        let vault = portable(&mut state)?;
        check_revision(vault, revision)?;
        let result = vault.change_master_password(password);
        if self.epoch.load(Ordering::Acquire) != epoch {
            vault.lock();
            return Err(VaultError::Locked);
        }
        result
    }
    pub(crate) fn mode(&self, id: ProfileId) -> Result<bool, VaultError> {
        self.ensure_owned()?;
        let mut state = self.guard();
        let result=mode(&mut state,id);
        result
    }
    pub(crate) fn set_mode(
        &self,
        id: ProfileId,
        revision: u64,
        selected: bool,
    ) -> Result<(), VaultError> {
        self.ensure_owned()?;
        self.invalidate();
        let mut state = self.guard();
        let vault = portable(&mut state)?;
        check_revision(vault, revision)?;
        vault.write(
            reference(id, Kind::ProfileMode),
            MODE_BINDING,
            &Secret::new(vec![u8::from(selected)]),
        )
    }
    pub(crate) fn read(
        &self,
        target: &SshConnectionRecord,
        kind: Kind,
    ) -> Result<Secret, VaultError> {
        self.ensure_owned()?;
        let mut state = self.guard();
        if !mode(&mut state, target.profile_id)? {
            return Err(VaultError::InvalidInput);
        }
        let result=portable(&mut state)?.read(reference(target.profile_id,kind),&binding(target,kind)?);
        result
    }
    pub(crate) fn edit(
        &self,
        target: &SshConnectionRecord,
        kind: Kind,
        secret: Option<&Secret>,
    ) -> Result<(), VaultError> {
        self.ensure_owned()?;
        self.invalidate();
        self.edit_current(target,kind,secret)
    }
    pub(crate) fn edit_authenticated(&self,target:&SshConnectionRecord,kind:Kind,secret:&Secret,epoch:u64)->Result<(),VaultError> {
        self.ensure_owned()?;
        self.commit_auth(epoch,||self.edit_current(target,kind,Some(secret)))?
    }
    fn edit_current(&self,target:&SshConnectionRecord,kind:Kind,secret:Option<&Secret>)->Result<(),VaultError> {
        let mut state = self.guard();
        if mode(&mut state, target.profile_id)? {
            let vault = portable(&mut state)?;
            let reference = reference(target.profile_id, kind);
            return match secret {
                Some(s) => vault.write(reference, &binding(target, kind)?, s),
                None => vault.delete(reference),
            };
        }
        let result = match kind {
            Kind::Password => {
                let id =
                    ProfilePasswordRef::from_profile_bytes(*target.profile_id.as_uuid().as_bytes());
                let vault = SystemProfilePasswordVault::new();
                let binding = ProfilePasswordBinding {
                    host: target.host.clone(),
                    port: target.port,
                    username: target.username.clone(),
                };
                match secret {
                    Some(s) => vault.write(&id, &binding, s),
                    None => vault.delete(&id),
                }
            }
            Kind::KeyPassphrase => {
                let id = ProfileKeyPassphraseRef::from_profile_bytes(
                    *target.profile_id.as_uuid().as_bytes(),
                );
                let path = target
                    .private_key_path
                    .as_deref()
                    .ok_or(VaultError::InvalidInput)?;
                let vault = SystemProfileKeyPassphraseVault::new();
                match secret {
                    Some(s) => vault.write(&id, path, s),
                    None => vault.delete(&id),
                }
            }
            _ => return Err(VaultError::InvalidInput),
        };
        result.map_err(|_| VaultError::Io(std::io::ErrorKind::Other))
    }
    pub(crate) fn import_key(
        &self,
        target: &SshConnectionRecord,
        revision: u64,
        bytes: &Secret,
        passphrase: Option<&Secret>,
    ) -> Result<(), VaultError> {
        self.ensure_owned()?;
        self.invalidate();
        let mut state = self.guard();
        if !mode(&mut state, target.profile_id)? {
            return Err(VaultError::InvalidInput);
        }
        let vault = portable(&mut state)?;
        check_revision(vault, revision)?;
        let key_binding = binding(target, Kind::PrivateKey)?;
        let pass_binding = binding(target, Kind::KeyPassphrase)?;
        let mut entries = vec![(
            reference(target.profile_id, Kind::PrivateKey),
            key_binding.as_str(),
            bytes,
        )];
        if let Some(secret) = passphrase {
            entries.push((
                reference(target.profile_id, Kind::KeyPassphrase),
                pass_binding.as_str(),
                secret,
            ));
        }
        let removals = if passphrase.is_none() {
            vec![reference(target.profile_id, Kind::KeyPassphrase)]
        } else {
            Vec::new()
        };
        vault.update_batch(&entries, &removals)
    }
    pub(crate) fn delete_key(&self, id: ProfileId, revision: u64) -> Result<(), VaultError> {
        self.ensure_owned()?;
        self.invalidate();
        let mut state = self.guard();
        let vault = portable(&mut state)?;
        check_revision(vault, revision)?;
        vault.update_batch(
            &[],
            &[
                reference(id, Kind::PrivateKey),
                reference(id, Kind::KeyPassphrase),
            ],
        )
    }
    pub(crate) fn response(&self, id: Option<ProfileId>) -> VaultResponse {
        let mut state = self.guard();
        let mut response = VaultResponse {
            status: VaultControlStatus::Ok as i32,
            lock_generation: self.epoch.load(Ordering::Acquire),
            system: state.root.is_some(),
            manually_locked: self.manually_locked.load(Ordering::Acquire),
            protection_cleanup_pending: root_path(&state.path.with_file_name("retired")).try_exists().unwrap_or(true),
            storage_path: state.path.to_string_lossy().into_owned(),
            session_state: self.session_state() as i32,
            profile_id: id.map_or_else(Vec::new, |id| id.as_uuid().as_bytes().to_vec()),
            ..Default::default()
        };
        match &mut state.backend {
            Backend::Native => {}
            Backend::Unavailable(error) => {
                response.exists = true;
                response.status = status(*error) as i32;
                response.key_notice = match error {
                    VaultError::Corrupt => "凭据库或保护信息损坏，请先检查数据文件；无法恢复时可保留旧库并重建。",
                    VaultError::Busy => "凭据库正被另一个后台占用，请先关闭重复程序；此时不能重建。",
                    VaultError::ReloadRequired | VaultError::WriteUncertain => "凭据库需要重新加载，或活动库信息缺失。请先重新加载；无法恢复时可保留旧库并重建。",
                    VaultError::Io(std::io::ErrorKind::NotFound) => "凭据库文件或保护信息不存在，请先检查存储位置；无法恢复时可保留旧库并重建。",
                    _ => "凭据库暂时不可用，请检查文件权限与存储位置后重试。",
                }.into();
            }
            Backend::Portable(vault) => {
                let current = vault.status();
                response.exists = true;
                response.unlocked = current.unlocked;
                response.revision = current.revision;
                if response.unlocked {
                    match managed_keys::list(vault) {
                        Ok(keys) => response.managed_keys = keys,
                        Err(error) => response.status = status(error) as i32,
                    }
                }
                if current.reload_required {
                    response.status = VaultControlStatus::ReloadRequired as i32;
                }
                if response.unlocked
                    && let Some(id) = id
                {
                    match mode_value(vault, id) {
                        Ok(value) => response.portable = value,
                        Err(e) => response.status = status(e) as i32,
                    }
                    response.has_password = vault
                        .contains(reference(id, Kind::Password))
                        .unwrap_or(false);
                    response.has_key_passphrase = vault
                        .contains(reference(id, Kind::KeyPassphrase))
                        .unwrap_or(false);
                    response.has_private_key = vault
                        .contains(reference(id, Kind::PrivateKey))
                        .unwrap_or(false);
                }
            }
        }
        response
    }
}
fn activation_path(path: &std::path::Path) -> PathBuf {
    let mut marker = path.as_os_str().to_owned();
    marker.push(".enabled");
    PathBuf::from(marker)
}
fn root_path(path: &std::path::Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".root");
    PathBuf::from(value)
}
fn read_root(path: &std::path::Path) -> Result<Option<VaultRootRef>, VaultError> {
    use std::io::Read;
    let file = match std::fs::File::open(root_path(path)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() != 24 {
        return Err(VaultError::Corrupt);
    }
    let mut bytes = [0; 24];
    file.take(25).read_exact(&mut bytes)?;
    if &bytes[..8] != b"CSHROOT1" || bytes[8..] == [0; 16] {
        return Err(VaultError::Corrupt);
    }
    let mut id = [0; 16];
    id.copy_from_slice(&bytes[8..]);
    Ok(Some(VaultRootRef::from_bytes(id)))
}
fn persist_root(path: &std::path::Path, id: VaultRootRef) -> Result<(), VaultError> {
    use std::io::Write;
    let parent = path.parent().ok_or(VaultError::InvalidInput)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(b"CSHROOT1")?;
    temporary.write_all(&id.bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(root_path(path))
        .map_err(|error| VaultError::from(error.error))?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn activate(path: &std::path::Path) -> Result<(), VaultError> {
    use std::io::Write;
    let marker = activation_path(path);
    if marker.try_exists()? {
        return Ok(());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(marker)?;
    file.write_all(b"CSHELL-VAULT-ENABLED-v1")?;
    file.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(path.parent().ok_or(VaultError::InvalidInput)?)?.sync_all()?;
    Ok(())
}
fn portable(state: &mut State) -> Result<&mut PortableVault, VaultError> {
    match &mut state.backend {
        Backend::Portable(v) => Ok(v),
        Backend::Native => Err(VaultError::InvalidInput),
        Backend::Unavailable(e) => Err(*e),
    }
}
fn mode(state: &mut State, id: ProfileId) -> Result<bool, VaultError> {
    match &mut state.backend {
        Backend::Native => Ok(false),
        Backend::Portable(v) => mode_value(v, id),
        Backend::Unavailable(e) => Err(*e),
    }
}
fn mode_value(vault: &PortableVault, id: ProfileId) -> Result<bool, VaultError> {
    match vault.read(reference(id, Kind::ProfileMode), MODE_BINDING) {
        Ok(secret) => match secret.expose() {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(VaultError::Corrupt),
        },
        Err(VaultError::Missing) => match vault.read(rebuild::default_mode_ref(), rebuild::DEFAULT_MODE_BINDING) {
            Ok(secret) if secret.expose() == [1] => Ok(true),
            Err(VaultError::Missing) => Ok(false),
            Ok(_) => Err(VaultError::Corrupt),
            Err(error) => Err(error),
        },
        Err(e) => Err(e),
    }
}
fn check_revision(vault: &PortableVault, revision: u64) -> Result<(), VaultError> {
    if vault.status().revision != revision {
        Err(VaultError::AlreadyExists)
    } else {
        Ok(())
    }
}
pub(crate) fn status(error: VaultError) -> VaultControlStatus {
    match error {
        VaultError::RootKeyMissing => VaultControlStatus::RootKeyMissing,
        VaultError::RootKeyUnavailable => VaultControlStatus::KeychainUnavailable,
        VaultError::RootCleanupRequired => VaultControlStatus::KeychainUnavailable,
        VaultError::Locked => VaultControlStatus::Locked,
        VaultError::AlreadyExists => VaultControlStatus::Conflict,
        VaultError::AuthenticationFailed => VaultControlStatus::AuthenticationFailed,
        VaultError::WriteUncertain | VaultError::ReloadRequired => {
            VaultControlStatus::ReloadRequired
        }
        VaultError::Busy => VaultControlStatus::Busy,
        VaultError::InvalidInput | VaultError::Capacity => VaultControlStatus::Invalid,
        _ => VaultControlStatus::Unavailable,
    }
}
