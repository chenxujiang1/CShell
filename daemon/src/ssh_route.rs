use cshell_ssh::connection_progress as progress;
use cshell_domain::ConnectionStage as Stage;
use crate::session_ipc::{SshLaunchFailure, read_ssh_auth_file, ssh_launch_failure};
use cshell_domain::{SshAgentBackend, SshAuthMethod, SshConnectionRecord, SshRoute};
use cshell_ipc::SessionFailureCode;
use cshell_ssh::{
    AgentBackend, KnownHostsVerifier, ProxyProtocol, ScannedHostKey, SshAuthentication,
    SshCertificate, SshConnection, SshError, SshPrivateKey, SshProvider, SshTransport,
    open_proxy_stream, open_tcp_stream, select_ssh_provider,
};
use cshell_vault::{
    KeychainError, ProfileKeyPassphraseRef, ProfilePasswordBinding, ProfilePasswordRef,
    SystemProfileKeyPassphraseVault, SystemProfilePasswordVault,
};
use std::{path::Path, sync::Arc};
use zeroize::Zeroizing;

async fn decode_private_key_worker(
    encoded: &str,
    passphrase: Option<&str>,
) -> Result<SshPrivateKey, cshell_ssh::SshError> {
    let encoded = Zeroizing::new(encoded.to_owned());
    let passphrase = passphrase.map(|p| Zeroizing::new(p.to_owned()));
    tokio::task::spawn_blocking(move || {
        SshPrivateKey::decode_private_key(&encoded, passphrase.as_ref().map(|p| p.as_str()))
    })
    .await
    .map_err(|_| cshell_ssh::SshError::KeyDecodeWorker)?
}

#[derive(Clone, Debug)]
pub(crate) struct SshConnectionPlan {
    pub title: String,
    pub target: SshConnectionRecord,
    pub jumps: Vec<SshConnectionRecord>,
}
impl SshConnectionPlan {
    pub(crate) fn route_identity(
        &self,
        fingerprints: &[&str],
    ) -> Result<String, serde_json::Error> {
        if fingerprints.len() != self.jumps.len() {
            return Err(<serde_json::Error as serde::ser::Error>::custom(
                "authenticated jump route count mismatch",
            ));
        }
        let jumps: Vec<_> = self
            .jumps
            .iter()
            .zip(fingerprints)
            .map(|(record, fingerprint)| {
                (
                    &record.host,
                    record.port,
                    &record.username,
                    &record.route,
                    Some(*fingerprint),
                )
            })
            .collect();
        if jumps.len() <= 1 {
            serde_json::to_string(&(1u8, &self.target.route, jumps.first().copied()))
        } else {
            serde_json::to_string(&(2u8, &self.target.route, &jumps))
        }
    }
    pub(crate) fn matches_captured_peer(&self, peer: &cshell_domain::ClipboardTrust) -> bool {
        self.captured_jump_keys(peer).is_some()
    }
    pub(crate) fn captured_jump_keys(
        &self,
        peer: &cshell_domain::ClipboardTrust,
    ) -> Option<Vec<String>> {
        if cshell_domain::ClipboardHost::new(&self.target.host, self.target.port).as_ref()
            != Some(&peer.host)
            || self.target.username != peer.username
        {
            return None;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&peer.route) else {
            return None;
        };
        let Some(parts) = value.as_array().filter(|v| v.len() == 3) else {
            return None;
        };
        let fingerprints: Option<Vec<&str>> = match parts[0].as_u64() {
            Some(1) if parts[2].is_null() => Some(Vec::new()),
            Some(1) => parts[2]
                .as_array()
                .filter(|v| v.len() == 5)
                .and_then(|v| v[4].as_str())
                .map(|s| vec![s]),
            Some(2) => parts[2].as_array().and_then(|rows| {
                rows.iter()
                    .map(|row| {
                        row.as_array()
                            .filter(|v| v.len() == 5)
                            .and_then(|v| v[4].as_str())
                    })
                    .collect()
            }),
            _ => None,
        };
        let keys = fingerprints?;
        if !self
            .route_identity(&keys)
            .is_ok_and(|route| route == peer.route)
        {
            return None;
        }
        Some(keys.into_iter().map(str::to_owned).collect())
    }
    /// Freeze the account identities returned by this already authenticated transport.
    /// Host key verification and captured identity comparisons remain strict.
    pub(crate) fn authenticated_plan(&self,client:&SshConnection,jump:Option<&JumpConnection>)->Self {
        let mut resolved=self.clone();
        if let Some(username)=client.authenticated_username(){resolved.target.username=username.to_owned();}
        if let Some(jump)=jump {for (record,client) in resolved.jumps.iter_mut().zip(&jump.0.clients){if let Some(username)=client.authenticated_username(){record.username=username.to_owned();}}}
        resolved
    }
    pub(crate) fn verified_peer(
        &self,
        client: &SshConnection,
        jump: Option<&JumpConnection>,
    ) -> Result<cshell_domain::ClipboardTrust, serde_json::Error> {
        let fingerprints = jump.map(JumpConnection::fingerprints).unwrap_or_default();
        let resolved=self.authenticated_plan(client,jump);
        let host = cshell_domain::ClipboardHost::new(&self.target.host, self.target.port)
            .ok_or_else(|| {
                <serde_json::Error as serde::ser::Error>::custom("invalid verified peer host")
            })?;
        Ok(cshell_domain::ClipboardTrust {
            host,
            username: resolved.target.username.clone(),
            host_key_sha256: client.verified_host_key_fingerprint().into(),
            route: resolved.route_identity(&fingerprints)?,
        })
    }
    pub(crate) fn new(
        title: String,
        target: SshConnectionRecord,
        connections: &[SshConnectionRecord],
    ) -> Result<Self, String> {
        let jumps = cshell_domain::ssh_jump_chain(&target, connections)
            .map_err(|e| e.to_string())?
            .into_iter()
            .cloned()
            .collect();
        Ok(Self {
            title,
            target,
            jumps,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JumpConnection(Arc<JumpClient>);
#[derive(Debug)]
struct JumpClient {
    // Outer transport first; drop and disconnect close the inner hops first.
    clients: Vec<Arc<SshConnection>>,
}
impl JumpConnection {
    pub(crate) fn fingerprints(&self) -> Vec<&str> {
        self.0
            .clients
            .iter()
            .map(|c| c.verified_host_key_fingerprint())
            .collect()
    }
    pub async fn disconnect(&self) {
        close_jumps(&self.0.clients).await;
    }
    pub(crate) async fn disconnect_confirmed(&self) -> bool {
        let mut closed = true;
        for client in self.0.clients.iter().rev() {
            closed &= client.disconnect_confirmed().await;
        }
        closed
    }
}
async fn close_jumps(clients: &[Arc<SshConnection>]) {
    for client in clients.iter().rev() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), client.disconnect()).await;
    }
}
async fn close_jumps_confirmed(clients: &[Arc<SshConnection>]) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut closed = true;
        for client in clients.iter().rev() {
            closed &= client.disconnect_confirmed().await;
        }
        closed
    })
    .await
    .unwrap_or(false)
}
impl Drop for JumpClient {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let clients = self.clients.clone();
            runtime.spawn(async move {
                close_jumps(&clients).await;
            });
        }
    }
}

pub(crate) struct RoutedSshConnection {
    pub client: SshConnection,
    pub jump: Option<JumpConnection>,
}

async fn verifier(
    target: &SshConnectionRecord,
    path: &Path,
) -> Result<KnownHostsVerifier, SshLaunchFailure> {
    crate::host_ca_store::verifier(path, &target.host, target.port)
        .await
        .map_err(|detail| SshLaunchFailure::new(SessionFailureCode::HostKeyDataInvalid, detail))
}
fn ssh_failure(error: cshell_ssh::SshError) -> SshLaunchFailure {
    let retryable_network = error.is_network_failure();
    let mut failure = ssh_launch_failure(crate::SshSessionError::Ssh(error));
    failure.retryable_network = retryable_network;
    failure
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
                "a jump transport requires a resolved route plan",
            ));
        }
    }
    .map_err(ssh_failure)
}

fn preflight(
    plan: &SshConnectionPlan,
    credential_template: Option<&SshConnectionRecord>,
) -> Result<Box<dyn SshProvider>, SshLaunchFailure> {
    let expected = cshell_domain::ssh_jump_chain(&plan.target, &plan.jumps).map_err(|e| {
        SshLaunchFailure::new(SessionFailureCode::InvalidConfiguration, e.to_string())
    })?;
    if expected.len() != plan.jumps.len() || !expected.iter().zip(&plan.jumps).all(|(a, b)| *a == b)
    {
        return Err(SshLaunchFailure::new(
            SessionFailureCode::InvalidConfiguration,
            "inconsistent frozen jump route",
        ));
    }
    let mut required =
        cshell_application::ssh_connection_requirements(&plan.target, plan.jumps.len()).map_err(
            |detail| SshLaunchFailure::new(SessionFailureCode::InvalidConfiguration, detail),
        )?;
    for jump in &plan.jumps {
        let hop = cshell_application::ssh_connection_requirements(jump, 0).map_err(|detail| {
            SshLaunchFailure::new(SessionFailureCode::InvalidConfiguration, detail)
        })?;
        required.capabilities.extend(hop.capabilities);
    }
    if let Some(template) = credential_template {
        let credentials =
            cshell_application::ssh_connection_requirements(template, 0).map_err(|detail| {
                SshLaunchFailure::new(SessionFailureCode::InvalidConfiguration, detail)
            })?;
        required.capabilities.extend(credentials.capabilities);
    }
    select_ssh_provider(&required).map_err(ssh_failure)
}

async fn route_stream(
    plan: &SshConnectionPlan,
    provider: &dyn SshProvider,
    path: &Path,
    vault: &Arc<crate::vault_control::VaultController>,
    pins: Option<&[String]>,
) -> Result<(SshTransport, Option<JumpConnection>), SshLaunchFailure> {
    if plan.jumps.is_empty() {
        return Ok((leaf_stream(&plan.target).await?, None));
    }
    // Own each authenticated hop immediately. Any later error/cancellation drops
    // this guard and closes already opened hops in reverse transport order.
    let mut guard = JumpClient {
        clients: Vec::new(),
    };
    for (index, jump) in plan.jumps.iter().enumerate() {
        progress::peer((index+1) as u8,&jump.host,jump.port);
        progress::stage(Stage::Credentials);
        let connected = async {
            let mut verifier = verifier(jump, path).await?;
            if let Some(pins) = pins {
                verifier = verifier.with_fingerprint(
                    pins.get(index)
                        .ok_or_else(|| {
                            SshLaunchFailure::new(
                                SessionFailureCode::InvalidConfiguration,
                                "incomplete hop pins",
                            )
                        })?
                        .clone(),
                );
            }
            let auth = load_authentication(jump, vault).await.map_err(|detail| {
                SshLaunchFailure::new(SessionFailureCode::CredentialUnavailable, detail)
            })?;
            let stream = if let Some(previous) = guard.clients.last() {
                jump_stream(previous, jump).await?
            } else {
                leaf_stream(jump).await?
            };
            tokio::time::timeout(
                std::time::Duration::from_secs(if jump.options.keyboard_interactive || !jump.options.authentication_order.is_empty() || jump.username.is_empty() || jump.auth_method==SshAuthMethod::Password {300} else {30}),
                provider.connect_configured(stream, jump.username.clone(), auth, verifier, jump.options.clone()),
            )
            .await
            .map_err(|_| {
                SshLaunchFailure::new(
                    SessionFailureCode::NetworkUnavailable,
                    "jump SSH connection timed out",
                )
            })?
            .map_err(ssh_failure)
        }
        .await
        .map_err(|mut failure| {
            failure.detail = format!("Jump hop {}: {}", index + 1, failure.detail);
            failure
        });
        let connected = match connected {
            Ok(client) => client,
            Err(mut failure) => {
                if failure.retryable_network {
                    failure.retryable_network = close_jumps_confirmed(&guard.clients).await;
                }
                return Err(failure);
            }
        };
        guard.clients.push(Arc::new(connected));
    }
    let last = guard.clients.last().ok_or_else(|| {
        SshLaunchFailure::new(
            SessionFailureCode::InvalidConfiguration,
            "missing authenticated jump",
        )
    })?;
    progress::peer(0,&plan.target.host,plan.target.port);
    let stream = match jump_stream(last, &plan.target).await {
        Ok(stream) => stream,
        Err(mut failure) => {
            if failure.retryable_network {
                failure.retryable_network = close_jumps_confirmed(&guard.clients).await;
            }
            return Err(failure);
        }
    };
    Ok((stream, Some(JumpConnection(Arc::new(guard)))))
}
async fn jump_stream(
    client: &SshConnection,
    target: &SshConnectionRecord,
) -> Result<SshTransport, SshLaunchFailure> {
    progress::stage(Stage::JumpChannel);
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.open_direct_stream(target.host.clone(), target.port),
    )
    .await
    .map_err(|_| {
        SshLaunchFailure::new(
            SessionFailureCode::NetworkUnavailable,
            "jump channel open timed out",
        )
    })?
    .map_err(ssh_failure)?;
    Ok(stream)
}

pub(crate) async fn connect_profile(
    plan: &SshConnectionPlan,
    path: &Path,
    vault: &Arc<crate::vault_control::VaultController>,
) -> Result<RoutedSshConnection, SshLaunchFailure> {
    connect_frozen_target(plan, &plan.target, path, vault).await
}

/// The original Profile authenticates credentials; the resolved destination
/// determines host trust. A host inventory override must not rebind secrets.
pub(crate) async fn connect_frozen_target(
    plan: &SshConnectionPlan,
    credential_template: &SshConnectionRecord,
    path: &Path,
    vault: &Arc<crate::vault_control::VaultController>,
) -> Result<RoutedSshConnection, SshLaunchFailure> {
    connect_target(plan, credential_template, path, vault, None).await
}
pub(crate) async fn connect_profile_pinned(
    plan: &SshConnectionPlan,
    path: &Path,
    vault: &Arc<crate::vault_control::VaultController>,
    peer: &cshell_domain::ClipboardTrust,
) -> Result<RoutedSshConnection, SshLaunchFailure> {
    connect_target(plan, &plan.target, path, vault, Some(peer)).await
}
async fn connect_target(
    plan: &SshConnectionPlan,
    credential_template: &SshConnectionRecord,
    path: &Path,
    vault: &Arc<crate::vault_control::VaultController>,
    peer: Option<&cshell_domain::ClipboardTrust>,
) -> Result<RoutedSshConnection, SshLaunchFailure> {
    progress::peer(0,&plan.target.host,plan.target.port);
    progress::stage(Stage::Credentials);
    let provider = preflight(plan, Some(credential_template))?;
    let _trust = crate::host_ca_store::coordination().read_owned().await;
    let pins = peer
        .map(|peer| {
            plan.captured_jump_keys(peer).ok_or_else(|| {
                SshLaunchFailure::new(
                    SessionFailureCode::InvalidConfiguration,
                    "captured route mismatch",
                )
            })
        })
        .transpose()?;
    let mut verifier = verifier(&plan.target, path).await?;
    if let Some(peer) = peer {
        verifier = verifier.with_fingerprint(peer.host_key_sha256.clone());
    }
    let auth = load_authentication(credential_template, vault)
        .await
        .map_err(|detail| {
            SshLaunchFailure::new(SessionFailureCode::CredentialUnavailable, detail)
        })?;
    let (stream, jump) =
        route_stream(plan, provider.as_ref(), path, vault, pins.as_deref()).await?;
    progress::peer(0,&plan.target.host,plan.target.port);
    let client = provider
        .connect_configured(stream, plan.target.username.clone(), auth, verifier, plan.target.options.clone())
        .await
        .map_err(ssh_failure);
    let client = match client {
        Ok(client) => client,
        Err(mut failure) => {
            if failure.retryable_network
                && let Some(jump) = &jump
            {
                failure.retryable_network = tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    jump.disconnect_confirmed(),
                )
                .await
                .unwrap_or(false);
            }
            return Err(failure);
        }
    };
    Ok(RoutedSshConnection { client, jump })
}

pub(crate) async fn scan_profile(
    plan: &SshConnectionPlan,
    path: &Path,
    vault: &Arc<crate::vault_control::VaultController>,
) -> Result<ScannedHostKey, String> {
    let mut cancellation = vault.cancellation();
    vault
        .ensure_owned()
        .map_err(|_| "Credential storage is owned by another daemon".to_owned())?;
    let scan = async {
        let provider = preflight(plan, None).map_err(|e| e.detail)?;
        let _trust = crate::host_ca_store::coordination().read_owned().await;
        let (stream, jump) = route_stream(plan, provider.as_ref(), path, vault, None)
            .await
            .map_err(|error| error.detail)?;
        let result = provider.scan(stream).await;
        if let Some(jump) = jump {
            jump.disconnect().await;
        }
        result
    };
    tokio::select! {
        biased;
        _ = cancellation.changed() => Err("Vault was locked during host-key scan".into()),
        result = scan => result,
    }
}

pub(crate) async fn load_authentication(
    target: &SshConnectionRecord,
    vault: &Arc<crate::vault_control::VaultController>,
) -> Result<SshAuthentication, String> {
    if target.auth_method==SshAuthMethod::Password || !target.options.authentication_order.is_empty() || target.username.is_empty() {
        return crate::keyboard_auth::ordered_authentication(target,vault).await;
    }
    load_single_authentication(target,vault,target.auth_method).await
}
pub(crate) async fn load_single_authentication(target:&SshConnectionRecord,vault:&Arc<crate::vault_control::VaultController>,method:SshAuthMethod)->Result<SshAuthentication,String> {
    let controller = vault.clone();
    let id = target.profile_id;
    let portable = tokio::task::spawn_blocking(move || controller.mode(id))
        .await
        .map_err(|_| "credential task failed")?
        .map_err(|error| error.to_string())?;
    let profile_id = target.profile_id;
    let authentication = match method {
        SshAuthMethod::Password => {
            let reference =
                ProfilePasswordRef::from_profile_bytes(*profile_id.as_uuid().as_bytes());
            let binding = ProfilePasswordBinding {
                host: target.host.clone(),
                port: target.port,
                username: target.username.clone(),
            };
            let controller = vault.clone();
            let credential_target = target.clone();
            let secret = tokio::task::spawn_blocking(move || {
                if portable {
                    controller
                        .read(&credential_target, cshell_vault::VaultCredentialKind::Password)
                        .map_err(|e| match e {cshell_vault::VaultError::Missing=>KeychainError::Missing,cshell_vault::VaultError::BindingMismatch=>KeychainError::BindingMismatch,_=>KeychainError::Unavailable})
                } else {
                    SystemProfilePasswordVault::new().read(&reference, &binding)
                }
            })
            .await
            .map_err(|_| "system keychain task failed")?;
            let password = match secret {
                Ok(secret) => Some(Zeroizing::new(std::str::from_utf8(secret.expose()).map_err(|_| "stored password is not valid UTF-8")?.to_owned())),
                Err(KeychainError::Missing) if target.options.keyboard_interactive => None,
                Err(KeychainError::BindingMismatch) => return Err("SSH target changed; save its password again".into()),
                Err(_) => return Err("password unavailable in credential storage; save it from the Profile editor".into()),
            };
            if target.options.keyboard_interactive {
                crate::keyboard_auth::authentication(vault.clone(), target, password)
            } else {
                SshAuthentication::Password(password.ok_or("password unavailable")?.to_string())
            }
        }
        SshAuthMethod::PrivateKey | SshAuthMethod::Certificate => {
            let path = target
                .private_key_path
                .as_deref()
                .ok_or("SSH Profile has no private key reference")?;
            let encoded = if portable {
                let controller = vault.clone();
                let target = target.clone();
                let bytes = tokio::task::spawn_blocking(move || {
                    controller.read(&target, cshell_vault::VaultCredentialKind::PrivateKey)
                })
                .await
                .map_err(|_| "credential task failed")?
                .map_err(|error| error.to_string())?;
                Zeroizing::new(
                    std::str::from_utf8(bytes.expose())
                        .map_err(|_| "invalid stored key")?
                        .to_owned(),
                )
            } else {
                read_ssh_auth_file(path, "private key").await?
            };
            let private_key = match decode_private_key_worker(&encoded, None).await {
                Ok(private_key) => private_key,
                Err(error @ SshError::RsaKey(_)) => {
                    return Err(format!("private key unavailable: {error}"));
                }
                Err(error) if matches!(&error, SshError::LegacyPem(reason) if *reason != cshell_ssh::LegacyPemError::PassphraseRequired) => {
                    return Err(format!("private key unavailable: {error}"));
                }
                Err(error) if matches!(&error,SshError::Ppk(reason)if *reason!=cshell_ssh::PpkError::PassphraseRequired) =>
                {
                    return Err(format!("private key unavailable: {error}"));
                }
                Err(unprotected_error) => {
                    let reference = ProfileKeyPassphraseRef::from_profile_bytes(
                        *profile_id.as_uuid().as_bytes(),
                    );
                    let key_path = path.to_owned();
                    let controller = vault.clone();
                    let target = target.clone();
                    let secret = tokio::task::spawn_blocking(move || {
                        if portable {
                            controller
                                .read(&target, cshell_vault::VaultCredentialKind::KeyPassphrase)
                                .map_err(|_| KeychainError::Unavailable)
                        } else {
                            SystemProfileKeyPassphraseVault::new().read(&reference, &key_path)
                        }
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
                        std::str::from_utf8(secret.expose())
                            .map_err(|_| "stored key passphrase is not valid UTF-8")?
                            .to_owned(),
                    );
                    decode_private_key_worker(&encoded, Some(&passphrase))
                        .await
                        .map_err(|error| format!("private key unavailable: {error}"))?
                }
            };
            if method == SshAuthMethod::Certificate {
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
