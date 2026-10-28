//! Bounded, one-shot, in-memory SSH keyboard-interactive challenges.
use crate::vault_control::VaultController;
use cshell_domain::{SessionId, SshConnectionRecord};
use cshell_ipc::{
    KeyboardAuthChallenge, KeyboardAuthPrompt, VaultControlStatus as Status, VaultOperation as Op,
    VaultRequest, VaultResponse,
};
use cshell_ssh::{KeyboardInteractiveChallenge, SshAuthentication, SshError};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use zeroize::Zeroizing;
tokio::task_local! {static OWNER: SessionId;}
pub(crate) async fn for_sftp<F: std::future::Future>(owner: SessionId, future: F) -> F::Output {
    OWNER.scope(owner, future).await
}
type Answers = Result<Zeroizing<Vec<String>>, SshError>;
struct FormAnswer {
    answers: Zeroizing<Vec<String>>,
    username: String,
    save: bool,
}
type FormResult = Result<FormAnswer, SshError>;
struct Pending {
    owner: Option<SessionId>,
    challenge: KeyboardAuthChallenge,
    reply: tokio::sync::oneshot::Sender<FormResult>,
}
#[derive(Default)]
pub(crate) struct Broker {
    pending: Mutex<VecDeque<Pending>>,
    cancelled: Mutex<VecDeque<(SessionId, std::time::Instant)>>,
    profiles: Mutex<std::sync::Weak<crate::ProfileIpcService>>,
    notices: Mutex<VecDeque<String>>,
    catalog_revision: std::sync::atomic::AtomicU64,
    frontend: Mutex<Option<std::time::Instant>>,
}
struct Cleanup {
    broker: Arc<Broker>,
    id: Vec<u8>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.broker
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|p| p.challenge.id != self.id);
    }
}
impl Broker {
    pub(crate) fn bind_profiles(&self, profiles: std::sync::Weak<crate::ProfileIpcService>) {
        *self
            .profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = profiles;
    }
    fn notice(&self, message: String) {
        let mut notices = self
            .notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if notices.len() >= 32 {
            notices.pop_front();
        }
        notices.push_back(message);
    }
    fn frontend_active(&self) -> bool {
        self.frontend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|time| time.elapsed() < Duration::from_secs(5))
    }
    pub(crate) fn cancel_sftp(&self, owner: SessionId) {
        {
            let mut cancelled = self
                .cancelled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cancelled.retain(|(_, time)| time.elapsed() < Duration::from_secs(330));
            if cancelled.len() >= 256 {
                cancelled.pop_front();
            }
            cancelled.push_back((owner, std::time::Instant::now()));
        }
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|p| p.owner != Some(owner));
    }
    pub(crate) fn current_owner_cancelled(&self)->bool {self.is_cancelled(OWNER.try_with(|id|*id).ok())}
    fn is_cancelled(&self, owner: Option<SessionId>) -> bool {
        owner.is_some_and(|owner| {
            self.cancelled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|(id, time)| *id == owner && time.elapsed() < Duration::from_secs(330))
        })
    }
    pub(crate) fn handle(
        &self,
        vault: &VaultController,
        op: Op,
        mut request: VaultRequest,
    ) -> VaultResponse {
        let status = if vault.ensure_owned().is_err() {
            Status::Locked
        } else if !request.secret.is_empty()
            || !request.package.is_empty()
            || !request.profile_id.is_empty()
            || !request.preview_token.is_empty()
            || !request.selected_profiles.is_empty()
        {
            Status::Invalid
        } else {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.retain(|p| !p.reply.is_closed());
            if op == Op::KeyboardAuthPoll {
                *self
                    .frontend
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(std::time::Instant::now());
                if request.auth_answer.is_some() {
                    Status::Invalid
                } else {
                    return VaultResponse {
                        status: Status::Ok as i32,
                        auth_challenge: pending.front().map(|p| p.challenge.clone()),
                        auth_catalog_revision: self
                            .catalog_revision
                            .load(std::sync::atomic::Ordering::Acquire),
                        auth_notice: self
                            .notices
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .pop_front()
                            .unwrap_or_default(),
                        ..Default::default()
                    };
                }
            } else if let Some(mut answer) = request.auth_answer.take() {
                if let Some(index) = pending.iter().position(|p| p.challenge.id == answer.id) {
                    if (!answer.cancel
                        && answer.answers.len() != pending[index].challenge.prompts.len())
                        || answer.answers.iter().any(|s| s.len() > 4096)
                        || (!answer.cancel && pending[index].challenge.vault_unlock && pending[index].challenge.password_required && answer.answers.first().is_none_or(String::is_empty))
                        || (!answer.cancel
                            && pending[index].challenge.login
                            && (answer.username.is_empty()
                                || answer.username.len() > 128
                                || answer.username.chars().any(|c| c.is_control() || c.is_whitespace())
                                || (!pending[index].challenge.username_editable
                                    && answer.username != pending[index].challenge.username)
                                || (pending[index].challenge.password_required
                                    && answer.answers.first().is_none_or(String::is_empty))))
                        || (!pending[index].challenge.login
                            && (answer.save_credentials || !answer.username.is_empty()))
                    {
                        Status::Invalid
                    } else if let Some(prompt) = pending.remove(index) {
                        let result = if answer.cancel {
                            Err(SshError::AuthenticationCancelled)
                        } else {
                            Ok(FormAnswer {
                                answers: Zeroizing::new(std::mem::take(&mut answer.answers)),
                                username: std::mem::take(&mut answer.username),
                                save: answer.save_credentials,
                            })
                        };
                        if prompt.reply.send(result).is_ok() {
                            Status::Ok
                        } else {
                            Status::Conflict
                        }
                    } else {
                        Status::Conflict
                    }
                } else {
                    Status::Conflict
                }
            } else {
                Status::Invalid
            }
        };
        VaultResponse {
            status: status as i32,
            ..Default::default()
        }
    }
    async fn ask(
        self: &Arc<Self>,
        vault: &VaultController,
        target: String,
        challenge: KeyboardInteractiveChallenge,
        owner: Option<SessionId>,
    ) -> Answers {
        self.ask_form(
            vault,
            KeyboardAuthChallenge {
                target,
                name: challenge.name,
                instructions: challenge.instructions,
                prompts: challenge
                    .prompts
                    .into_iter()
                    .map(|p| KeyboardAuthPrompt {
                        text: p.text,
                        echo: p.echo,
                    })
                    .collect(),
                ..Default::default()
            },
            owner,
        )
        .await
        .map(|a| a.answers)
    }
    pub(crate) async fn ask_vault(self:&Arc<Self>,vault:&VaultController,target:String,challenge:KeyboardInteractiveChallenge)->Answers {
        self.ask_form(vault,KeyboardAuthChallenge{target,name:challenge.name,instructions:challenge.instructions,vault_unlock:true,password_required:!challenge.prompts.is_empty(),
            prompts:challenge.prompts.into_iter().map(|p|KeyboardAuthPrompt{text:p.text,echo:false}).collect(),..Default::default()},OWNER.try_with(|id|*id).ok()).await.map(|a|a.answers)
    }
    async fn ask_form(
        self: &Arc<Self>,
        vault: &VaultController,
        mut challenge: KeyboardAuthChallenge,
        owner: Option<SessionId>,
    ) -> FormResult {
        let mut cancellation = vault.cancellation();
        vault
            .ensure_owned()
            .map_err(|_| SshError::AuthenticationCancelled)?;
        let started = std::time::Instant::now();
        while !self.frontend_active() {
            if started.elapsed() > Duration::from_millis(1500) {
                return Err(SshError::InteractiveAuthenticationRequired);
            }
            tokio::select! {_=cancellation.changed()=>return Err(SshError::AuthenticationCancelled),_=tokio::time::sleep(Duration::from_millis(20))=>{}}
        }
        let resume_stage=if challenge.vault_unlock{cshell_domain::ConnectionStage::Credentials}else{cshell_domain::ConnectionStage::Authentication};
        let id = SessionId::new().as_uuid().as_bytes().to_vec();
        let (reply, receiver) = tokio::sync::oneshot::channel();
        cshell_ssh::connection_progress::stage(cshell_domain::ConnectionStage::WaitingForInput);
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.retain(|p| !p.reply.is_closed());
            if pending.len() >= 32 {
                return Err(SshError::KeyboardInteractiveLimitExceeded);
            }
            challenge.id = id.clone();
            pending.push_back(Pending {
                owner,
                challenge,
                reply,
            });
        }
        let _cleanup = Cleanup {
            broker: self.clone(),
            id,
        };
        if self.is_cancelled(owner) {
            return Err(SshError::AuthenticationCancelled);
        }
        let result=tokio::select! {biased;
            _=cancellation.changed()=>Err(SshError::AuthenticationCancelled),
            _=async {loop {tokio::time::sleep(Duration::from_secs(1)).await;if !self.frontend_active(){break;}}}=>Err(SshError::InteractiveAuthenticationRequired),
            result=tokio::time::timeout(Duration::from_secs(120),receiver)=>result.map_err(|_|SshError::KeyboardInteractiveTimeout)?.map_err(|_|SshError::AuthenticationCancelled)?,
        };
        if result.is_ok(){cshell_ssh::connection_progress::stage(resume_stage);}
        result
    }
}
/// A deliberately narrow allowlist: OTP, passcode, PIN and ambiguous prompts stay manual.
fn password_prompt(text: &str, echo: bool) -> bool {
    if echo {
        return false;
    }
    let label = text
        .trim()
        .trim_end_matches([':', '：'])
        .trim()
        .to_lowercase();
    matches!(
        label.as_str(),
        "password" | "login password" | "unix password" | "密码" | "请输入密码"
    ) || (label.ends_with("'s password") && !label.contains(['\n', '\r']))
}
pub(crate) fn authentication(
    vault: Arc<VaultController>,
    target: &SshConnectionRecord,
    mut password: Option<Zeroizing<String>>,
) -> SshAuthentication {
    let owner = OWNER.try_with(|id| *id).ok();
    let target = format!("{}@{}:{}", target.username, target.host, target.port);
    SshAuthentication::KeyboardInteractive {
        responder: Box::new(move |mut challenge| {
            let password_indices: Vec<_> = challenge
                .prompts
                .iter()
                .enumerate()
                .filter(|(_, p)| password_prompt(&p.text, p.echo))
                .map(|(i, _)| i)
                .collect();
            let automatic = if password_indices.len() == 1 {
                password.take().map(|p| (password_indices[0], p))
            } else {
                None
            };
            let vault = vault.clone();
            let target = target.clone();
            Box::pin(async move {
                vault
                    .ensure_owned()
                    .map_err(|_| SshError::AuthenticationCancelled)?;
                if vault.keyboard_auth.is_cancelled(owner) {
                    return Err(SshError::AuthenticationCancelled);
                }
                let count = challenge.prompts.len();
                let mut output = Zeroizing::new(vec![String::new(); count]);
                let auto_index = automatic.as_ref().map(|(i, _)| *i);
                if let Some((index, secret)) = automatic {
                    output[index] = secret.to_string();
                }
                let manual_indices: Vec<_> =
                    (0..count).filter(|i| Some(*i) != auto_index).collect();
                if !manual_indices.is_empty() {
                    challenge.prompts = challenge
                        .prompts
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| Some(*i) != auto_index)
                        .map(|(_, p)| p)
                        .collect();
                    let mut answers = vault
                        .keyboard_auth
                        .ask(&vault, target, challenge, owner)
                        .await?;
                    for (index, answer) in manual_indices.into_iter().zip(answers.iter_mut()) {
                        output[index] = std::mem::take(answer);
                    }
                }
                Ok(std::mem::take(&mut *output))
            })
        }),
    }
}
mod login;
pub(crate) use login::ordered_authentication;
#[cfg(test)]
mod tests;
