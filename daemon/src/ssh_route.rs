use crate::session_ipc::{SshLaunchFailure, read_ssh_auth_file, ssh_launch_failure};
use cshell_domain::{SshAgentBackend, SshAuthMethod, SshConnectionRecord, SshRoute};
use cshell_ipc::SessionFailureCode;
use cshell_ssh::{
    AgentBackend, KnownHostsError, KnownHostsVerifier, ProxyProtocol, RusshClient, ScannedHostKey,
    SshAuthentication, SshCertificate, SshPrivateKey, SshTransport, open_proxy_stream,
    open_tcp_stream, scan_host_key_stream,
};
use cshell_vault::{
    KeychainError, ProfileKeyPassphraseRef, ProfilePasswordBinding, ProfilePasswordRef,
    SystemProfileKeyPassphraseVault, SystemProfilePasswordVault,
};
use std::{path::Path, sync::Arc};
use zeroize::Zeroizing;

#[derive(Clone, Debug)]
pub(crate) struct SshConnectionPlan {
    pub title: String,
    pub target: SshConnectionRecord,
    pub jump: Option<SshConnectionRecord>,
}

#[derive(Clone, Debug)]
pub(crate) struct JumpConnection(Arc<JumpClient>);
#[derive(Debug)]
struct JumpClient {
    client: Arc<RusshClient>,
}
impl JumpConnection {
    pub async fn disconnect(&self) {
        let _ = self.0.client.disconnect().await;
    }
}
impl Drop for JumpClient {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let client = Arc::clone(&self.client);
            runtime.spawn(async move {
                let _ = client.disconnect().await;
            });
        }
    }
}

pub(crate) struct RoutedSshConnection {
    pub client: RusshClient,
    pub jump: Option<JumpConnection>,
}

async fn verifier(
    target: &SshConnectionRecord,
    path: &Path,
) -> Result<KnownHostsVerifier, SshLaunchFailure> {
    let path = path.to_owned();
    let host = target.host.clone();
    let port = target.port;
    tokio::task::spawn_blocking(move || KnownHostsVerifier::load(path, &host, port)).await
        .map_err(|_| SshLaunchFailure::new(SessionFailureCode::HostKeyDataInvalid, "known_hosts loading failed"))?
        .map_err(|error| {
            let code = if matches!(&error, KnownHostsError::Io(error) if error.kind() == std::io::ErrorKind::NotFound) {
                SessionFailureCode::HostKeyUnknown
            } else { SessionFailureCode::HostKeyDataInvalid };
            SshLaunchFailure::new(code, format!("strict known_hosts check cannot start: {error}"))
        })
}
fn ssh_failure(error: cshell_ssh::SshError) -> SshLaunchFailure {
    ssh_launch_failure(crate::SshSessionError::Ssh(error))
}
async fn leaf_stream(target: &SshConnectionRecord) -> Result<SshTransport, SshLaunchFailure> {
    match &target.route {
        SshRoute::Direct => open_tcp_stream(&target.host, target.port).await,
        SshRoute::Socks5 { host, port } => {
            open_proxy_stream(
                ProxyProtocol::Socks5,
                host,
                *port,
                &target.host,
                target.port,
            )
            .await
        }
        SshRoute::HttpConnect { host, port } => {
            open_proxy_stream(
                ProxyProtocol::HttpConnect,
                host,
                *port,
                &target.host,
                target.port,
            )
            .await
        }
        SshRoute::Jump { .. } => {
            return Err(SshLaunchFailure::new(
                SessionFailureCode::InvalidConfiguration,
                "nested jump routes are unsupported",
            ));
        }
    }
    .map_err(ssh_failure)
}

async fn route_stream(
    plan: &SshConnectionPlan,
    path: &Path,
) -> Result<(SshTransport, Option<JumpConnection>), SshLaunchFailure> {
    match &plan.target.route {
        SshRoute::Jump { profile_id } => {
            let jump = plan
                .jump
                .as_ref()
                .filter(|jump| jump.profile_id == *profile_id)
                .ok_or_else(|| {
                    SshLaunchFailure::new(
                        SessionFailureCode::InvalidConfiguration,
                        "jump Profile is missing",
                    )
                })?;
            let connected = async {
                let verifier = verifier(jump, path).await?;
                let auth = load_authentication(jump).await.map_err(|detail| {
                    SshLaunchFailure::new(SessionFailureCode::CredentialUnavailable, detail)
                })?;
                let stream = leaf_stream(jump).await?;
                RusshClient::connect_known_hosts_stream(
                    stream,
                    jump.username.clone(),
                    auth,
                    verifier,
                )
                .await
                .map_err(ssh_failure)
            }
            .await
            .map_err(|mut error| {
                error.detail = format!("Jump Profile: {}", error.detail);
                error
            })?;
            let guard = JumpConnection(Arc::new(JumpClient {
                client: Arc::new(connected),
            }));
            let stream = guard
                .0
                .client
                .open_direct_stream(plan.target.host.clone(), plan.target.port)
                .await
                .map_err(|error| {
                    let mut failure = ssh_failure(error);
                    failure.detail = format!("Jump tunnel: {}", failure.detail);
                    failure
                })?;
            Ok((Box::new(stream), Some(guard)))
        }
        _ => Ok((leaf_stream(&plan.target).await?, None)),
    }
}

pub(crate) async fn connect_profile(
    plan: &SshConnectionPlan,
    path: &Path,
) -> Result<RoutedSshConnection, SshLaunchFailure> {
    let verifier = verifier(&plan.target, path).await?;
    let auth = load_authentication(&plan.target).await.map_err(|detail| {
        SshLaunchFailure::new(SessionFailureCode::CredentialUnavailable, detail)
    })?;
    let (stream, jump) = route_stream(plan, path).await?;
    let client = RusshClient::connect_known_hosts_stream(
        stream,
        plan.target.username.clone(),
        auth,
        verifier,
    )
    .await
    .map_err(ssh_failure)?;
    Ok(RoutedSshConnection { client, jump })
}

pub(crate) async fn scan_profile(
    plan: &SshConnectionPlan,
    path: &Path,
) -> Result<ScannedHostKey, String> {
    let (stream, jump) = route_stream(plan, path)
        .await
        .map_err(|error| error.detail)?;
    let result = scan_host_key_stream(stream).await;
    if let Some(jump) = jump {
        jump.disconnect().await;
    }
    result
}

pub(crate) async fn load_authentication(
    target: &SshConnectionRecord,
) -> Result<SshAuthentication, String> {
    let profile_id = target.profile_id;
    let authentication = match target.auth_method {
        SshAuthMethod::Password => {
            let reference =
                ProfilePasswordRef::from_profile_bytes(*profile_id.as_uuid().as_bytes());
            let binding = ProfilePasswordBinding {
                host: target.host.clone(),
                port: target.port,
                username: target.username.clone(),
            };
            let secret = tokio::task::spawn_blocking(move || {
                SystemProfilePasswordVault::new().read(&reference, &binding)
            })
            .await
            .map_err(|_| "system keychain task failed")?
            .map_err(|error| match error {
                KeychainError::BindingMismatch => "SSH target changed; save its password again",
                _ => "password unavailable in system keychain; save it from the Profile editor",
            })?;
            let password = String::from_utf8(secret.expose().to_vec())
                .map_err(|_| "stored password is not valid UTF-8")?;
            SshAuthentication::Password(password)
        }
        SshAuthMethod::PrivateKey | SshAuthMethod::Certificate => {
            let path = target
                .private_key_path
                .as_deref()
                .ok_or("SSH Profile has no private key reference")?;
            let encoded = read_ssh_auth_file(path, "private key").await?;
            let private_key = match SshPrivateKey::decode_openssh(&encoded, None) {
                Ok(private_key) => private_key,
                Err(unprotected_error) => {
                    let reference = ProfileKeyPassphraseRef::from_profile_bytes(
                        *profile_id.as_uuid().as_bytes(),
                    );
                    let key_path = path.to_owned();
                    let secret = tokio::task::spawn_blocking(move || {
                        SystemProfileKeyPassphraseVault::new().read(&reference, &key_path)
                    })
                    .await
                    .map_err(|_| "system keychain task failed")?
                    .map_err(|error| match error {
                        KeychainError::Missing => format!(
                            "private key cannot be opened without a passphrase: {unprotected_error}"
                        ),
                        KeychainError::BindingMismatch => {
                            "private key path changed; save its passphrase again".into()
                        }
                        _ => "key passphrase unavailable in system keychain".into(),
                    })?;
                    let passphrase = Zeroizing::new(
                        String::from_utf8(secret.expose().to_vec())
                            .map_err(|_| "stored key passphrase is not valid UTF-8")?,
                    );
                    SshPrivateKey::decode_openssh(&encoded, Some(&passphrase))
                        .map_err(|error| format!("private key unavailable: {error}"))?
                }
            };
            if target.auth_method == SshAuthMethod::Certificate {
                let path = target
                    .certificate_path
                    .as_deref()
                    .ok_or("SSH Profile has no certificate reference")?;
                let encoded = read_ssh_auth_file(path, "certificate").await?;
                let certificate = SshCertificate::decode_openssh(&encoded)
                    .map_err(|error| format!("certificate unavailable: {error}"))?;
                SshAuthentication::Certificate {
                    private_key,
                    certificate: Box::new(certificate),
                }
            } else {
                SshAuthentication::PrivateKey(private_key)
            }
        }
        SshAuthMethod::Agent => {
            let backend = match target.agent_backend {
                SshAgentBackend::Auto => AgentBackend::Auto,
                SshAgentBackend::OpenSsh => AgentBackend::OpenSsh,
                SshAgentBackend::Pageant => AgentBackend::Pageant,
            };
            SshAuthentication::Agent {
                backend,
                identity_fingerprint: target.agent_identity.clone(),
            }
        }
    };
    Ok::<_, String>(authentication)
}
