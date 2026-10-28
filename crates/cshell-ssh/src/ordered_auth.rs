//! Server-advertised authentication order, with bounded interactive password retries.
use super::*;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
};
pub type LoginResponder =
    Box<dyn FnMut(String) -> Pin<Box<dyn Future<Output = Result<String, SshError>> + Send>> + Send>;
pub type PasswordResponder =
    Box<dyn FnMut(bool) -> Pin<Box<dyn Future<Output = Result<String, SshError>> + Send>> + Send>;
pub struct OrderedAuthentication {
    pub methods: Vec<OrderedAuthStep>,
    pub login: LoginResponder,
    pub identity: Arc<Mutex<String>>,
    pub on_rejected: Box<dyn FnMut() + Send>,
    pub on_authenticated: Box<dyn FnMut() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>,
}
pub enum OrderedAuthStep {
    Password(PasswordResponder),
    KeyboardInteractive(KeyboardInteractiveResponder),
    PrivateKey(Option<SshPrivateKey>),
    Certificate(Option<(SshPrivateKey, Box<SshCertificate>)>),
    Agent {
        backend: AgentBackend,
        identity: Option<String>,
    },
}
impl OrderedAuthStep {
    pub(crate) fn capability(&self) -> cshell_domain::SshCapability {
        use cshell_domain::SshCapability as C;
        match self {
            Self::Password(_) => C::Password,
            Self::KeyboardInteractive(_) => C::KeyboardInteractive,
            Self::PrivateKey(_) => C::PublicKey,
            Self::Certificate(_) => C::UserCertificate,
            Self::Agent { .. } => C::Agent,
        }
    }
    fn kind(&self) -> russh::MethodKind {
        match self {
            Self::Password(_) => russh::MethodKind::Password,
            Self::KeyboardInteractive(_) => russh::MethodKind::KeyboardInteractive,
            _ => russh::MethodKind::PublicKey,
        }
    }
}
pub(super) async fn authenticate(
    session: &mut russh::client::Handle<VerifiedClient>,
    username: String,
    ordered: &mut OrderedAuthentication,
) -> Result<(), SshError> {
    authenticate_inner(session, username, ordered).await?;
    (ordered.on_authenticated)().await;
    Ok(())
}
async fn authenticate_inner(
    session: &mut russh::client::Handle<VerifiedClient>,
    username: String,
    ordered: &mut OrderedAuthentication,
) -> Result<(), SshError> {
    let username = (ordered.login)(username).await?;
    if username.is_empty() || username.len() > 128 || username.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(SshError::AuthenticationCancelled);
    }
    *ordered
        .identity
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = username.clone();
    let probe = tokio::time::timeout(
        SSH_AUTH_ROUND_TIMEOUT,
        session.authenticate_none(username.clone()),
    )
    .await
    .map_err(|_| SshError::AuthenticationTimeout)??;
    let mut allowed = match probe {
        russh::client::AuthResult::Success => return Ok(()),
        russh::client::AuthResult::Failure {
            remaining_methods, ..
        } => remaining_methods.iter().copied().collect::<Vec<_>>(),
    };
    // A second factor can become available only after a later preference
    // partially succeeds. Reconsider skipped preferences without retrying a
    // completed/exhausted method or allowing an unbounded authentication loop.
    let mut attempted = vec![false; ordered.methods.len()];
    loop {
        let next = ordered
            .methods
            .iter()
            .enumerate()
            .find_map(|(index, method)| {
                (!attempted[index] && allowed.contains(&method.kind())).then_some(index)
            });
        let Some(index) = next else {
            break;
        };
        attempted[index] = true;
        let method = &mut ordered.methods[index];
        let result = match method {
            OrderedAuthStep::Password(responder) => {
                let mut result = Err(SshError::AuthenticationRejected);
                for attempt in 0..3 {
                    let password = responder(attempt > 0).await?;
                    let auth = tokio::time::timeout(
                        SSH_AUTH_ROUND_TIMEOUT,
                        session.authenticate_password(username.clone(), password),
                    )
                    .await
                    .map_err(|_| SshError::AuthenticationTimeout)??;
                    match auth {
                        russh::client::AuthResult::Success => return Ok(()),
                        russh::client::AuthResult::Failure {
                            remaining_methods,
                            partial_success,
                        } => {
                            allowed = remaining_methods.iter().copied().collect();
                            if partial_success {
                                result = Err(SshError::AuthenticationPartial(
                                    remaining_methods.iter().map(String::from).collect(),
                                ));
                                break;
                            }
                            (ordered.on_rejected)();
                            if !allowed.contains(&russh::MethodKind::Password) {
                                break;
                            }
                        }
                    }
                }
                result
            }
            OrderedAuthStep::KeyboardInteractive(responder) => {
                let mut result = Err(SshError::AuthenticationRejected);
                for _ in 0..3 {
                    let mut prompted = false;
                    let mut wrapped = |challenge| {
                        prompted = true;
                        responder(challenge)
                    };
                    result = authenticate_keyboard_interactive_step(
                        session,
                        username.clone(),
                        &mut wrapped,
                    )
                    .await;
                    if !prompted {
                        break;
                    }
                    if !matches!(result, Err(SshError::AuthenticationRejected)) {
                        break;
                    }
                    (ordered.on_rejected)();
                }
                result
            }
            OrderedAuthStep::PrivateKey(key) => match key.take() {
                Some(key) => authenticate_with_private_key(session, username.clone(), key).await,
                None => continue,
            },
            OrderedAuthStep::Certificate(credential) => match credential.take() {
                Some((key, certificate)) => {
                    certificate.validate_private_key(&key)?;
                    authenticate_with_certificate(session, username.clone(), key, *certificate)
                        .await
                }
                None => continue,
            },
            OrderedAuthStep::Agent { backend, identity } => {
                match tokio::time::timeout(SSH_AUTH_ROUND_TIMEOUT, connect_agent_backend(*backend))
                    .await
                {
                    Ok(Ok(mut agent)) => {
                        authenticate_with_agent_selected(
                            session,
                            username.clone(),
                            &mut agent,
                            identity.as_deref(),
                        )
                        .await
                    }
                    Ok(Err(_)) => continue,
                    Err(_) => continue,
                }
            }
        };
        match result {
            Ok(()) => return Ok(()),
            Err(
                SshError::AuthenticationRejected
                | SshError::AgentIdentityUnavailable
                | SshError::AgentAuthentication(_)
                | SshError::Agent(_),
            ) => {}
            Err(SshError::AuthenticationPartial(methods)) => {
                allowed = methods.iter().filter_map(|m| m.parse().ok()).collect();
            }
            Err(error) => return Err(error),
        }
    }
    Err(SshError::AuthenticationRejected)
}

impl std::fmt::Debug for OrderedAuthentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderedAuthentication")
            .field("methods", &self.methods)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for OrderedAuthStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Password(_) => "Password([REDACTED])",
            Self::KeyboardInteractive(_) => "KeyboardInteractive([REDACTED])",
            Self::PrivateKey(_) => "PrivateKey([REDACTED])",
            Self::Certificate(_) => "Certificate([REDACTED])",
            Self::Agent { .. } => "Agent([REDACTED])",
        })
    }
}
