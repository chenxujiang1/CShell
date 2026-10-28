//! Changing protection publishes a complete, independently verified generation.
use super::*;
use std::path::Path;
const CONNECTION_PREPARATION_TIMEOUT: Duration = Duration::from_secs(120);
impl VaultController {
    pub(crate) fn protect(
        &self,
        revision: u64,
        generation: u64,
        password: &Secret,
        system: bool,
    ) -> Result<(), VaultError> {
        self.protect_with(
            revision,
            generation,
            password,
            system,
            SystemVaultRootKeys::create,
            SystemVaultRootKeys::read,
            rebuild::publish,
            SystemVaultRootKeys::delete,
        )
    }
    fn protect_with(
        &self,
        revision: u64,
        generation: u64,
        password: &Secret,
        system: bool,
        create: impl FnOnce(VaultRootRef) -> Result<Secret, cshell_vault::KeychainError>,
        read: impl FnOnce(VaultRootRef) -> Result<Secret, cshell_vault::KeychainError>,
        publish: impl FnOnce(&Path, &Path) -> Result<(), VaultError>,
        delete: impl Fn(VaultRootRef) -> Result<(), cshell_vault::KeychainError>,
    ) -> Result<(), VaultError> {
        let _transition = self
            .transition_gate
            .try_lock()
            .map_err(|_| VaultError::Busy)?;
        self.ensure_owned()?;
        let (old_path, old_root) = {
            let state = self.guard();
            if root_path(&state.path.with_file_name("retired")).try_exists()? {
                return Err(VaultError::RootCleanupRequired);
            }
            let Backend::Portable(vault) = &state.backend else {
                return Err(VaultError::InvalidInput);
            };
            check_revision(vault, revision)?;
            if !vault.status().unlocked {
                return Err(VaultError::Locked);
            }
            if state.root.is_some() == system {
                return Err(VaultError::InvalidInput);
            }
            if system {
                vault.verify_password(password)?;
            } else {
                if !(8..=1024).contains(&password.expose().len()) {
                    return Err(VaultError::InvalidInput);
                }
                let secret = read(state.root.ok_or(VaultError::InvalidInput)?)
                    .map_err(|_| VaultError::RootKeyUnavailable)?;
                vault.verify_password(&secret)?;
            }
            (state.path.clone(), state.root)
        };
        let epoch = self.begin_unlock(generation)?;
        let parent = self.path.parent().ok_or(VaultError::InvalidInput)?;
        let directory = tempfile::Builder::new()
            .prefix("vault-")
            .tempdir_in(parent)?
            .keep();
        cshell_ipc::RuntimePaths::prepare(directory.clone())
            .map_err(|_| VaultError::Io(std::io::ErrorKind::PermissionDenied))?;
        let path = directory.join("master.vault");
        let root = system.then(VaultRootRef::new);
        let mut created_root = false;
        let result = (|| {
            if let Some(root) = root {
                persist_root(&path, root)?;
            }
            let secret = if let Some(root) = root {
                match create(root) {
                    Ok(secret) => {
                        created_root = true;
                        Some(secret)
                    }
                    Err(cshell_vault::KeychainError::CleanupRequired) => {
                        created_root = true;
                        return Err(VaultError::RootKeyUnavailable);
                    }
                    Err(_) => return Err(VaultError::RootKeyUnavailable),
                }
            } else {
                None
            };
            #[allow(unused_mut)]
            let mut candidate = {
                let state = self.guard();
                let Backend::Portable(old) = &state.backend else {
                    return Err(VaultError::ReloadRequired);
                };
                check_revision(old, revision)?;
                if state.path != old_path {
                    return Err(VaultError::ReloadRequired);
                }
                old.copy_protected(&path, secret.as_ref().unwrap_or(password))?
            };
            activate(&path)?;
            #[cfg(unix)]
            std::fs::File::open(&directory).and_then(|f| f.sync_all())?;
            if let Some(retired) = old_root {
                persist_root(&directory.join("retired"), retired)?;
            }
            self.commit_auth(epoch, || {
                let mut state = self.guard();
                let Backend::Portable(old) = &state.backend else {
                    return Err(VaultError::ReloadRequired);
                };
                check_revision(old, revision)?;
                if state.path != old_path || !old.status().unlocked {
                    return Err(VaultError::AlreadyExists);
                }
                if let Err(error) = publish(&self.path, &directory) {
                    if rebuild::active_path(&self.path)
                        .as_ref()
                        .is_ok_and(|active| active == &old_path)
                    {
                        return Err(error);
                    }
                    state.backend = Backend::Unavailable(VaultError::WriteUncertain);
                    state.unlocked_at = None;
                    return Err(VaultError::WriteUncertain);
                }
                #[cfg(unix)]
                let publication_uncertain = std::fs::File::open(parent)
                    .and_then(|f| f.sync_all())
                    .is_err();
                #[cfg(unix)]
                if publication_uncertain {
                    candidate.lock();
                }
                state.path = path.clone();
                state.root = root;
                state.unlocked_at = candidate.status().unlocked.then(Instant::now);
                state.backend = Backend::Portable(Box::new(candidate));
                self.manually_locked.store(false, Ordering::Release);
                #[cfg(unix)]
                if publication_uncertain {
                    return Err(VaultError::WriteUncertain);
                }
                Ok(())
            })??;
            if !rebuild::active_path(&self.path)
                .as_ref()
                .is_ok_and(|active| active == &path)
            {
                return Err(VaultError::WriteUncertain);
            }
            if let Some(retired) = old_root {
                delete(retired).map_err(|_| VaultError::RootCleanupRequired)?;
                std::fs::remove_file(root_path(&directory.join("retired")))?;
            }
            Ok(())
        })();
        // Never leave a new system key able to decrypt a failed password->system copy.
        // An uncertain pointer is not safe to roll back or delete automatically.
        if result.is_err()
            && created_root
            && let Some(root) = root
            && rebuild::active_path(&self.path)
                .as_ref()
                .is_ok_and(|active| active != &path)
        {
            if delete(root).is_err() {
                persist_root(&old_path.with_file_name("retired"), root)?;
                return Err(VaultError::RootCleanupRequired);
            }
        }
        result
    }
    pub(crate) fn finish_protection_cleanup(&self) -> Result<(), VaultError> {
        let _transition = self
            .transition_gate
            .try_lock()
            .map_err(|_| VaultError::Busy)?;
        self.ensure_owned()?;
        let state = self.guard();
        let marker = state.path.with_file_name("retired");
        if let Some(root) = read_root(&marker)? {
            if !rebuild::active_path(&self.path)
                .as_ref()
                .is_ok_and(|active| active == &state.path)
            {
                return Err(VaultError::ReloadRequired);
            }
            if !matches!(&state.backend,Backend::Portable(vault) if vault.status().unlocked) {
                return Err(VaultError::Locked);
            }
            if state
                .root
                .is_some_and(|current| current.bytes() == root.bytes())
            {
                return Err(VaultError::Corrupt);
            }
            #[cfg(unix)]
            std::fs::File::open(self.path.parent().ok_or(VaultError::InvalidInput)?)
                .and_then(|f| f.sync_all())?;
            SystemVaultRootKeys::delete(root).map_err(|_| VaultError::RootCleanupRequired)?;
            std::fs::remove_file(root_path(&marker))?;
        }
        Ok(())
    }
    pub(crate) fn lock_manually(&self) {
        self.manually_locked.store(true, Ordering::Release);
        self.lock();
    }
    /// Called only by an explicit connection workflow; polling never unlocks.
    pub(crate) async fn prepare_connection(
        self: &Arc<Self>,
        target: String,
    ) -> Result<(), cshell_ssh::SshError> {
        tokio::time::timeout(
            CONNECTION_PREPARATION_TIMEOUT,
            self.prepare_connection_inner(target),
        )
        .await
        .map_err(|_| {
            cshell_ssh::SshError::CredentialUnavailable(
                "凭据库解锁等待超时，请重新连接后再试。".into(),
            )
        })?
    }
    async fn prepare_connection_inner(
        self: &Arc<Self>,
        target: String,
    ) -> Result<(), cshell_ssh::SshError> {
        use cshell_ssh::{KeyboardInteractiveChallenge, KeyboardInteractivePrompt, SshError};
        let _access = self.access_gate.lock().await;
        if self.keyboard_auth.current_owner_cancelled() {
            return Err(cshell_ssh::SshError::AuthenticationCancelled);
        }
        for attempt in 0..3 {
            self.ensure_owned().map_err(|_| {
                SshError::CredentialUnavailable(
                    "凭据库正被另一个后台占用，请关闭重复程序后重试".into(),
                )
            })?;
            let response = self.response(None);
            if !response.exists || response.unlocked {
                return Ok(());
            }
            if response.status != VaultControlStatus::Ok as i32 {
                return Err(SshError::CredentialUnavailable(
                    "凭据库不可用，请在全局设置中检查恢复选项".into(),
                ));
            }
            if response.system && !response.manually_locked {
                let controller = self.clone();
                let generation = response.lock_generation;
                return tokio::task::spawn_blocking(move || controller.unlock_system(generation))
                    .await
                    .map_err(|_| SshError::CredentialUnavailable("系统解锁任务失败".into()))?
                    .map_err(|error| {
                        SshError::CredentialUnavailable(format!(
                            "系统解锁失败：{error}；请在全局设置中检查凭据库"
                        ))
                    });
            }
            let prompts = if response.system {
                vec![]
            } else {
                vec![KeyboardInteractivePrompt {
                    text: "凭据库主密码".into(),
                    echo: false,
                }]
            };
            let answers = self
                .keyboard_auth
                .ask_vault(
                    self,
                    target.clone(),
                    KeyboardInteractiveChallenge {
                        name: if attempt == 0 {
                            "解锁凭据库"
                        } else {
                            "主密码不正确，请重试"
                        }
                        .into(),
                        instructions: if response.system {
                            "凭据库已手动锁定，确认解锁后继续本次连接。"
                        } else {
                            "输入凭据库主密码后自动继续本次连接；此密码不会发送到服务器。"
                        }
                        .into(),
                        prompts,
                    },
                )
                .await?;
            let controller = self.clone();
            let generation = response.lock_generation;
            let system = response.system;
            let secret = Secret::new(
                answers
                    .first()
                    .map_or_else(Vec::new, |s| s.as_bytes().to_vec()),
            );
            let result = tokio::task::spawn_blocking(move || {
                if system {
                    controller.unlock_system(generation)
                } else {
                    controller.unlock(&secret, generation)
                }
            })
            .await
            .map_err(|_| SshError::CredentialUnavailable("凭据库解锁任务失败".into()))?;
            match result {
                Ok(()) => return Ok(()),
                Err(VaultError::AuthenticationFailed) if attempt < 2 => {}
                Err(VaultError::Locked | VaultError::AlreadyExists) => {
                    return Err(SshError::AuthenticationCancelled);
                }
                Err(error) => {
                    return Err(SshError::CredentialUnavailable(format!(
                        "凭据库解锁失败：{error}"
                    )));
                }
            }
        }
        Err(SshError::AuthenticationRejected)
    }
}
#[cfg(test)]
mod tests;
