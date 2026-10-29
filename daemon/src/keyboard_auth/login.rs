use super::*;
use cshell_domain::{SshAuthMethod, SshAuthStep};
use cshell_ssh::{AgentBackend, OrderedAuthStep, OrderedAuthentication};
use cshell_vault::{
    KeychainError, ProfilePasswordBinding, ProfilePasswordRef, Secret, SystemProfilePasswordVault,
    VaultCredentialKind as Kind,
};
struct State {
    username: String,
    password: Option<Zeroizing<String>>,
    candidate: Option<Zeroizing<String>>,
    save: bool,
    password_attempts: usize,
}
struct Attempt {
    vault: Arc<VaultController>,
    target: SshConnectionRecord,
    owner: Option<SessionId>,
    epoch: u64,
    state: Mutex<State>,
}
impl Attempt {
    async fn form(&self, edit_username: bool, required: bool) -> Result<(), SshError> {
        let (username, save, rejected) = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                state.username.clone(),
                state.save,
                state.password_attempts > 0,
            )
        };
        let show_password = self.target.uses_password();
        let challenge = KeyboardAuthChallenge {
            target: format!("{}:{}", self.target.host, self.target.port),
            login: true,
            username,
            username_editable: edit_username,
            password_required: required,
            rejected,
            save_credentials: save,
            name: if rejected {
                "认证未通过，请重新输入密码"
            } else {
                "填写连接账号"
            }
            .into(),
            instructions: if !required && show_password {
                "密码可以留空，服务器要求时再填写。"
            } else {
                ""
            }
            .into(),
            prompts: if show_password {
                vec![KeyboardAuthPrompt {
                    text: "密码".into(),
                    echo: false,
                }]
            } else {
                vec![]
            },
            ..Default::default()
        };
        let mut answer = self
            .vault
            .keyboard_auth
            .ask_form(&self.vault, challenge, self.owner)
            .await?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.username = answer.username;
        state.save = answer.save;
        state.password = answer
            .answers
            .first_mut()
            .filter(|p| !p.is_empty())
            .map(|p| Zeroizing::new(std::mem::take(p)));
        Ok(())
    }
    async fn username(&self, preset: String) -> Result<String, SshError> {
        self.vault
            .commit_auth(self.epoch, || ())
            .map_err(|_| SshError::AuthenticationCancelled)?;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.username.is_empty() && !preset.is_empty() {
                state.username = preset;
            }
        }
        if self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .username
            .is_empty()
        {
            self.form(true, false).await?;
        }
        Ok(self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .username
            .clone())
    }
    async fn password(&self, rejected: bool) -> Result<String, SshError> {
        self.vault
            .commit_auth(self.epoch, || ())
            .map_err(|_| SshError::AuthenticationCancelled)?;
        if rejected {
            self.reject();
        }
        if self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .password
            .is_none()
        {
            self.form(false, true).await?;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let password = state
            .password
            .take()
            .ok_or(SshError::AuthenticationCancelled)?;
        state.candidate = Some(Zeroizing::new(password.to_string()));
        state.password_attempts += 1;
        Ok(password.to_string())
    }
    fn reject(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.password = None;
        state.candidate = None;
    }
    async fn keyboard(
        &self,
        mut challenge: KeyboardInteractiveChallenge,
    ) -> Result<Vec<String>, SshError> {
        if self.vault.keyboard_auth.is_cancelled(self.owner) {
            return Err(SshError::AuthenticationCancelled);
        }
        let indices: Vec<_> = challenge
            .prompts
            .iter()
            .enumerate()
            .filter(|(_, p)| password_prompt(&p.text, p.echo))
            .map(|(i, _)| i)
            .collect();
        let mut output = Zeroizing::new(vec![String::new(); challenge.prompts.len()]);
        let automatic = if indices.len() == 1 {
            let index = indices[0];
            output[index] = self.password(false).await?;
            Some(index)
        } else {
            None
        };
        let manual: Vec<_> = (0..output.len())
            .filter(|i| Some(*i) != automatic)
            .collect();
        if !manual.is_empty() {
            challenge.prompts = challenge
                .prompts
                .into_iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != automatic)
                .map(|(_, p)| p)
                .collect();
            let peer = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                format!(
                    "{}@{}:{}",
                    state.username, self.target.host, self.target.port
                )
            };
            let mut answers = self
                .vault
                .keyboard_auth
                .ask(&self.vault, peer, challenge, self.owner)
                .await?;
            for (index, answer) in manual.into_iter().zip(answers.iter_mut()) {
                output[index] = std::mem::take(answer);
            }
        }
        Ok(std::mem::take(&mut *output))
    }
    async fn authenticated(&self) {
        let (save, username, password) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.save, state.username.clone(), state.candidate.take())
        };
        if !save {
            return;
        }
        let profiles = self
            .vault
            .keyboard_auth
            .profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade();
        let result = match profiles {
            Some(profiles) => {
                profiles
                    .save_authenticated_login(&self.target, username, password, self.epoch)
                    .await
            }
            None => Err("配置服务不可用".into()),
        };
        if let Err(error) = result {
            self.vault.keyboard_auth.notice(format!(
                "认证已通过，但账号或密码未能保存：{error}。可在会话配置中重新保存。"
            ));
        }
    }
}
async fn stored_password(
    target: &SshConnectionRecord,
    vault: &Arc<VaultController>,
) -> Result<Option<Zeroizing<String>>, String> {
    let vault = vault.clone();
    let target = target.clone();
    tokio::task::spawn_blocking(move || {
        if target.username.is_empty() || !target.uses_password() {
            return Ok(None);
        }
        let portable = vault.mode(target.profile_id).map_err(|e| e.to_string())?;
        let secret = if portable {
            match vault.read(&target, Kind::Password) {
                Ok(secret) => Some(secret),
                Err(
                    cshell_vault::VaultError::Missing | cshell_vault::VaultError::BindingMismatch,
                ) => None,
                Err(e) => return Err(e.to_string()),
            }
        } else {
            let reference =
                ProfilePasswordRef::from_profile_bytes(*target.profile_id.as_uuid().as_bytes());
            let binding = ProfilePasswordBinding {
                host: target.host,
                port: target.port,
                username: target.username,
            };
            match SystemProfilePasswordVault::new().read(&reference, &binding) {
                Ok(secret) => Some(secret),
                Err(
                    KeychainError::Missing
                    | KeychainError::BindingMismatch
                    | KeychainError::Unavailable,
                ) => None,
                Err(_) => return Err("密码存储不可用".into()),
            }
        };
        secret
            .map(|secret| {
                std::str::from_utf8(secret.expose())
                    .map(|s| Zeroizing::new(s.to_owned()))
                    .map_err(|_| "保存的密码编码无效".into())
            })
            .transpose()
    })
    .await
    .map_err(|_| "凭据读取任务失败".to_owned())?
}
pub(crate) async fn ordered_authentication(
    target: &SshConnectionRecord,
    vault: &Arc<VaultController>,
) -> Result<SshAuthentication, String> {
    let password = stored_password(target, vault).await?;
    let attempt = Arc::new(Attempt {
        vault: vault.clone(),
        target: target.clone(),
        owner: OWNER.try_with(|id| *id).ok(),
        epoch: *vault.cancellation().borrow(),
        state: Mutex::new(State {
            username: target.username.clone(),
            password,
            candidate: None,
            save: false,
            password_attempts: 0,
        }),
    });
    let mut methods = Vec::new();
    for step in target.authentication_steps() {
        match step {
            SshAuthStep::Password => {
                let a = attempt.clone();
                methods.push(OrderedAuthStep::Password(Box::new(move |rejected| {
                    let a = a.clone();
                    Box::pin(async move { a.password(rejected).await })
                })));
            }
            SshAuthStep::KeyboardInteractive => {
                let a = attempt.clone();
                methods.push(OrderedAuthStep::KeyboardInteractive(Box::new(
                    move |challenge| {
                        let a = a.clone();
                        Box::pin(async move { a.keyboard(challenge).await })
                    },
                )));
            }
            SshAuthStep::PrivateKey | SshAuthStep::Certificate => {
                let kind = if step == SshAuthStep::Certificate {
                    SshAuthMethod::Certificate
                } else {
                    SshAuthMethod::PrivateKey
                };
                match crate::ssh_route::load_single_authentication(target, vault, kind).await {
                    Ok(SshAuthentication::PrivateKey(key)) => {
                        methods.push(OrderedAuthStep::PrivateKey(Some(key)))
                    }
                    Ok(SshAuthentication::Certificate {
                        private_key,
                        certificate,
                    }) => methods.push(OrderedAuthStep::Certificate(Some((
                        private_key,
                        certificate,
                    )))),
                    Err(_) => {}
                    _ => {}
                }
            }
            SshAuthStep::Agent => methods.push(OrderedAuthStep::Agent {
                backend: match target.agent_backend {
                    cshell_domain::SshAgentBackend::Auto => AgentBackend::Auto,
                    cshell_domain::SshAgentBackend::OpenSsh => AgentBackend::OpenSsh,
                    cshell_domain::SshAgentBackend::Pageant => AgentBackend::Pageant,
                },
                identity: target.agent_identity.clone(),
            }),
        }
    }
    let login = attempt.clone();
    let rejected = attempt.clone();
    let success = attempt.clone();
    Ok(SshAuthentication::Ordered(Box::new(
        OrderedAuthentication {
            methods,
            identity: Arc::new(Mutex::new(target.username.clone())),
            login: Box::new(move |username| {
                let a = login.clone();
                Box::pin(async move { a.username(username).await })
            }),
            on_rejected: Box::new(move || rejected.reject()),
            on_authenticated: Box::new(move || {
                let a = success.clone();
                Box::pin(async move { a.authenticated().await })
            }),
        },
    )))
}
impl crate::ProfileIpcService {
    async fn save_authenticated_login(
        &self,
        expected: &SshConnectionRecord,
        username: String,
        password: Option<Zeroizing<String>>,
        epoch: u64,
    ) -> Result<(), String> {
        let _gate = self.catalog_mutation.lock().await;
        let snapshot = self
            .service
            .load()
            .await
            .map_err(|_| "无法读取配置")?
            .snapshot();
        let current = snapshot
            .ssh_connections
            .iter()
            .find(|c| c.profile_id == expected.profile_id)
            .ok_or("配置已删除")?;
        let mut target = expected.clone();
        target.username = username;
        if current != expected && current != &target {
            return Err("配置已修改，未覆盖新的账号或连接设置".into());
        }
        self.vault
            .commit_auth(epoch, || ())
            .map_err(|_| "凭据库已锁定")?;
        // Preserve existing portable key material when the explicitly saved account
        // changes its strict credential binding. Never read a mismatched binding.
        let key_credentials = if current.username != target.username && current.uses_private_key() {
            let vault = self.vault.clone();
            let original = current.clone();
            tokio::task::spawn_blocking(move || {
                let mut credentials = Vec::new();
                if vault.mode(original.profile_id)? {
                    for kind in [Kind::PrivateKey, Kind::KeyPassphrase] {
                        match vault.read(&original, kind) {
                            Ok(secret) => credentials.push((kind, secret)),
                            Err(
                                cshell_vault::VaultError::Missing
                                | cshell_vault::VaultError::BindingMismatch,
                            ) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                Ok::<_, cshell_vault::VaultError>(credentials)
            })
            .await
            .map_err(|_| "私钥凭据读取任务失败")?
            .map_err(|_| "私钥凭据不可用，未更改账号")?
        } else {
            Vec::new()
        };
        self.vault
            .commit_auth(epoch, || ())
            .map_err(|_| "凭据库已锁定")?;
        let revision = if current.username != target.username {
            self.service
                .apply_batch(
                    snapshot.revision,
                    &[cshell_application::CatalogChange::UpsertSshConnection(
                        target.clone(),
                    )],
                )
                .await
                .map_err(|_| "账号配置保存失败")?;
            self.service
                .load()
                .await
                .map_err(|_| "无法读取已保存账号")?
                .snapshot()
                .revision
        } else {
            snapshot.revision
        };
        self.vault
            .keyboard_auth
            .catalog_revision
            .fetch_max(revision, std::sync::atomic::Ordering::AcqRel);
        if !key_credentials.is_empty() {
            let vault = self.vault.clone();
            let saved = target.clone();
            tokio::task::spawn_blocking(move || {
                for (kind, secret) in key_credentials {
                    vault.edit_authenticated(&saved, kind, &secret, epoch)?;
                }
                Ok::<_, cshell_vault::VaultError>(())
            })
            .await
            .map_err(|_| "私钥凭据保存任务失败")?
            .map_err(|_| "私钥凭据保存失败或已锁定")?;
        }
        if let Some(password) = password {
            let vault = self.vault.clone();
            tokio::task::spawn_blocking(move || {
                vault.edit_authenticated(
                    &target,
                    Kind::Password,
                    &Secret::new(password.as_bytes().to_vec()),
                    epoch,
                )
            })
            .await
            .map_err(|_| "密码保存任务失败")?
            .map_err(|_| "密码存储不可用或已锁定")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
