//! SSH provider boundary and the initial russh capability declaration.

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

mod forwarding;

pub use forwarding::{ForwardHandle, ForwardLimits, RemoteForwardTarget};

const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_AUTH_ROUND_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_AGENT_SIGN_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_KEYBOARD_INTERACTIVE_ROUNDS: usize = 16;
pub const MAX_KEYBOARD_INTERACTIVE_PROMPTS: usize = 32;
pub const MAX_KEYBOARD_INTERACTIVE_METADATA_BYTES: usize = 16 * 1024;
pub const MAX_AGENT_IDENTITIES: usize = 256;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum AgentBackend {
    #[default]
    Auto,
    OpenSsh,
    Pageant,
}

type ForwardRouteKey = (String, u32);
type ForwardRouteSender = tokio::sync::mpsc::Sender<russh::Channel<russh::client::Msg>>;
type ForwardRoutes =
    Arc<std::sync::RwLock<std::collections::HashMap<ForwardRouteKey, ForwardRouteSender>>>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum AlgorithmPolicy {
    #[default]
    Modern,
    Compatible,
    Legacy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HostKeyDecision {
    Trusted,
    TrustOnce,
    TrustAndPersist,
    Reject,
    ChangedAndBlocked,
    RevokedAndBlocked,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SshCapabilities {
    pub password: bool,
    pub public_key: bool,
    pub keyboard_interactive: bool,
    pub agent: bool,
    pub certificates: bool,
    pub jump_host: bool,
    pub local_forward: bool,
    pub remote_forward: bool,
    pub dynamic_forward: bool,
    pub sftp: bool,
    pub x11_forwarding: bool,
}

pub trait SshProvider: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> SshCapabilities;
    fn algorithm_policy(&self) -> AlgorithmPolicy;
}

#[derive(Debug, Default)]
pub struct RusshProvider {
    algorithm_policy: AlgorithmPolicy,
}

impl RusshProvider {
    #[must_use]
    pub const fn new(algorithm_policy: AlgorithmPolicy) -> Self {
        Self { algorithm_policy }
    }

    #[must_use]
    pub fn client_config(&self) -> russh::client::Config {
        russh::client::Config::default()
    }
}

impl SshProvider for RusshProvider {
    fn name(&self) -> &'static str {
        "russh"
    }

    fn capabilities(&self) -> SshCapabilities {
        SshCapabilities {
            password: true,
            public_key: true,
            keyboard_interactive: true,
            agent: true,
            certificates: true,
            jump_host: true,
            local_forward: true,
            remote_forward: true,
            dynamic_forward: true,
            sftp: true,
            x11_forwarding: cfg!(feature = "x11-forwarding"),
        }
    }

    fn algorithm_policy(&self) -> AlgorithmPolicy {
        self.algorithm_policy
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedHostKey {
    sha256_fingerprint: String,
}

#[derive(Clone)]
pub struct SshPrivateKey {
    key: Arc<russh::keys::PrivateKey>,
}

#[derive(Clone)]
pub struct SshCertificate {
    certificate: russh::keys::ssh_key::Certificate,
}

impl std::fmt::Debug for SshCertificate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshCertificate")
            .field("algorithm", &self.certificate.algorithm())
            .field("key_id", &self.certificate.key_id())
            .field("valid_principals", &self.certificate.valid_principals())
            .field("valid_after", &self.certificate.valid_after())
            .field("valid_before", &self.certificate.valid_before())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SshPrivateKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshPrivateKey")
            .field("algorithm", &self.key.algorithm())
            .field("sha256_fingerprint", &self.sha256_fingerprint())
            .finish_non_exhaustive()
    }
}

impl SshPrivateKey {
    pub fn decode_openssh(encoded: &str, passphrase: Option<&str>) -> Result<Self, SshError> {
        let key =
            russh::keys::decode_secret_key(encoded, passphrase).map_err(SshError::PrivateKey)?;
        Ok(Self { key: Arc::new(key) })
    }

    #[must_use]
    pub fn sha256_fingerprint(&self) -> String {
        self.key
            .public_key()
            .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
            .to_string()
    }
}

impl SshCertificate {
    pub fn decode_openssh(encoded: &str) -> Result<Self, SshError> {
        let certificate = russh::keys::ssh_key::Certificate::from_openssh(encoded)
            .map_err(SshError::Certificate)?;
        if certificate.cert_type() != russh::keys::ssh_key::certificate::CertType::User {
            return Err(SshError::HostCertificateForUserAuthentication);
        }
        Ok(Self { certificate })
    }

    #[must_use]
    pub fn key_id(&self) -> &str {
        self.certificate.key_id()
    }

    #[must_use]
    pub fn valid_principals(&self) -> &[String] {
        self.certificate.valid_principals()
    }

    fn validate_private_key(&self, private_key: &SshPrivateKey) -> Result<(), SshError> {
        if private_key.key.public_key().key_data() == self.certificate.public_key() {
            Ok(())
        } else {
            Err(SshError::CertificateKeyMismatch)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardInteractivePrompt {
    pub text: String,
    pub echo: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardInteractiveChallenge {
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<KeyboardInteractivePrompt>,
}

impl PinnedHostKey {
    #[must_use]
    pub fn sha256(fingerprint: impl Into<String>) -> Self {
        Self {
            sha256_fingerprint: fingerprint.into(),
        }
    }
}

#[derive(Debug)]
struct VerifiedClient {
    host_key: PinnedHostKey,
    forward_routes: ForwardRoutes,
}

impl russh::client::Handler for VerifiedClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key
            .public_key()
            .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
            .to_string();
        Ok(fingerprint == self.host_key.sha256_fingerprint)
    }

    fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::client::ChannelOpenHandle,
        _session: &mut russh::client::Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let sender = self.forward_routes.read().ok().and_then(|routes| {
            routes
                .get(&(connected_address.to_owned(), connected_port))
                .cloned()
        });
        async move {
            if let Some(sender) = sender
                && sender.try_send(channel).is_ok()
            {
                reply.accept().await;
            }
            Ok(())
        }
    }
}

#[derive(Debug, Error)]
pub enum SshError {
    #[error(transparent)]
    Protocol(#[from] russh::Error),
    #[error("SSH authentication was rejected")]
    AuthenticationRejected,
    #[error("SSH authentication was cancelled")]
    AuthenticationCancelled,
    #[error("SSH authentication round timed out")]
    AuthenticationTimeout,
    #[error("SSH private key could not be decoded: {0}")]
    PrivateKey(#[source] russh::keys::Error),
    #[error("OpenSSH certificate could not be decoded: {0}")]
    Certificate(#[source] russh::keys::ssh_key::Error),
    #[error("a host certificate cannot be used for user authentication")]
    HostCertificateForUserAuthentication,
    #[error("OpenSSH certificate does not match the selected private key")]
    CertificateKeyMismatch,
    #[error("SSH agent operation failed: {0}")]
    Agent(#[source] russh::keys::Error),
    #[error("SSH agent signing authentication failed: {0}")]
    AgentAuthentication(String),
    #[error("SSH agent returned {actual} identities; maximum is {maximum}")]
    AgentIdentityLimit { actual: usize, maximum: usize },
    #[error("SSH agent backend {backend:?} is not supported on this platform")]
    AgentBackendUnsupported { backend: AgentBackend },
    #[error("no Windows SSH agent backend was available: {0}")]
    AgentBackendsUnavailable(String),
    #[error("server only advertised legacy RSA/SHA-1 user authentication")]
    LegacyRsaSignatureRejected,
    #[error("keyboard-interactive challenge exceeded a safety limit")]
    KeyboardInteractiveLimitExceeded,
    #[error("keyboard-interactive response count {actual} does not match prompt count {expected}")]
    KeyboardInteractiveResponseCount { expected: usize, actual: usize },
    #[error("SSH connection attempt timed out")]
    ConnectionTimeout,
    #[error("SSH command channel closed without an exit status")]
    MissingExitStatus,
    #[error("SFTP subsystem failed: {0}")]
    Sftp(#[from] cshell_sftp::SftpError),
    #[error("SSH forwarding I/O failed: {0}")]
    ForwardIo(#[source] std::io::Error),
    #[error("SSH forwarding configuration is invalid: {0}")]
    InvalidForwardConfig(String),
    #[error("SOCKS5 handshake failed: {0}")]
    Socks5(String),
    #[error("SSH forwarding task failed: {0}")]
    ForwardTask(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: u32,
}

pub struct RusshClient {
    pub(crate) session: russh::client::Handle<VerifiedClient>,
    pub(crate) forward_routes: ForwardRoutes,
}

impl std::fmt::Debug for RusshClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RusshClient")
            .finish_non_exhaustive()
    }
}

impl RusshClient {
    pub async fn connect_certificate<A>(
        address: A,
        username: impl Into<String>,
        private_key: SshPrivateKey,
        certificate: SshCertificate,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError>
    where
        A: tokio::net::ToSocketAddrs,
    {
        certificate.validate_private_key(&private_key)?;
        let (mut session, forward_routes) = connect_verified(address, host_key).await?;
        authenticate_with_certificate(&mut session, username.into(), private_key, certificate)
            .await?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    pub async fn connect_certificate_via(
        jump: &Self,
        target_host: impl Into<String>,
        target_port: u16,
        username: impl Into<String>,
        private_key: SshPrivateKey,
        certificate: SshCertificate,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError> {
        certificate.validate_private_key(&private_key)?;
        let stream = jump
            .open_direct_stream(target_host.into(), target_port)
            .await?;
        let (mut session, forward_routes) = connect_verified_stream(stream, host_key).await?;
        authenticate_with_certificate(&mut session, username.into(), private_key, certificate)
            .await?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    pub async fn connect_password_via(
        jump: &Self,
        target_host: impl Into<String>,
        target_port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError> {
        let stream = jump
            .open_direct_stream(target_host.into(), target_port)
            .await?;
        let (mut session, forward_routes) = connect_verified_stream(stream, host_key).await?;
        let result = tokio::time::timeout(
            SSH_AUTH_ROUND_TIMEOUT,
            session.authenticate_password(username, password),
        )
        .await
        .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
        ensure_authenticated(result)?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    pub async fn connect_public_key_via(
        jump: &Self,
        target_host: impl Into<String>,
        target_port: u16,
        username: impl Into<String>,
        private_key: SshPrivateKey,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError> {
        let stream = jump
            .open_direct_stream(target_host.into(), target_port)
            .await?;
        let (mut session, forward_routes) = connect_verified_stream(stream, host_key).await?;
        let hash_alg = modern_rsa_hash(&session, &private_key.key).await?;
        let result = tokio::time::timeout(
            SSH_AUTH_ROUND_TIMEOUT,
            session.authenticate_publickey(
                username,
                russh::keys::PrivateKeyWithHashAlg::new(private_key.key, hash_alg),
            ),
        )
        .await
        .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
        ensure_authenticated(result)?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    async fn open_direct_stream(
        &self,
        target_host: String,
        target_port: u16,
    ) -> Result<russh::ChannelStream<russh::client::Msg>, SshError> {
        if target_host.is_empty() || target_port == 0 {
            return Err(SshError::InvalidForwardConfig(
                "jump target host and port must be non-empty and non-zero".to_owned(),
            ));
        }
        let channel = tokio::time::timeout(
            SSH_CONNECT_TIMEOUT,
            self.session.channel_open_direct_tcpip(
                target_host,
                u32::from(target_port),
                "127.0.0.1",
                0,
            ),
        )
        .await
        .map_err(|_elapsed| SshError::ConnectionTimeout)??;
        Ok(channel.into_stream())
    }

    pub async fn connect_password<A>(
        address: A,
        username: impl Into<String>,
        password: impl Into<String>,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError>
    where
        A: tokio::net::ToSocketAddrs,
    {
        let (mut session, forward_routes) = connect_verified(address, host_key).await?;
        let result = tokio::time::timeout(
            SSH_AUTH_ROUND_TIMEOUT,
            session.authenticate_password(username, password),
        )
        .await
        .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
        ensure_authenticated(result)?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    pub async fn connect_public_key<A>(
        address: A,
        username: impl Into<String>,
        private_key: SshPrivateKey,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError>
    where
        A: tokio::net::ToSocketAddrs,
    {
        let (mut session, forward_routes) = connect_verified(address, host_key).await?;
        let username = username.into();
        let hash_alg = modern_rsa_hash(&session, &private_key.key).await?;
        let result = tokio::time::timeout(
            SSH_AUTH_ROUND_TIMEOUT,
            session.authenticate_publickey(
                username,
                russh::keys::PrivateKeyWithHashAlg::new(private_key.key, hash_alg),
            ),
        )
        .await
        .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
        ensure_authenticated(result)?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    pub async fn connect_agent<A>(
        address: A,
        username: impl Into<String>,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError>
    where
        A: tokio::net::ToSocketAddrs,
    {
        Self::connect_agent_with_backend(address, username, host_key, AgentBackend::Auto).await
    }

    pub async fn connect_agent_with_backend<A>(
        address: A,
        username: impl Into<String>,
        host_key: PinnedHostKey,
        backend: AgentBackend,
    ) -> Result<Self, SshError>
    where
        A: tokio::net::ToSocketAddrs,
    {
        let (mut session, forward_routes) = connect_verified(address, host_key).await?;
        let mut agent =
            tokio::time::timeout(SSH_AUTH_ROUND_TIMEOUT, connect_agent_backend(backend))
                .await
                .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
        authenticate_with_agent(&mut session, username.into(), &mut agent).await?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    pub async fn connect_agent_via(
        jump: &Self,
        target_host: impl Into<String>,
        target_port: u16,
        username: impl Into<String>,
        host_key: PinnedHostKey,
        backend: AgentBackend,
    ) -> Result<Self, SshError> {
        let stream = jump
            .open_direct_stream(target_host.into(), target_port)
            .await?;
        let (mut session, forward_routes) = connect_verified_stream(stream, host_key).await?;
        let mut agent =
            tokio::time::timeout(SSH_AUTH_ROUND_TIMEOUT, connect_agent_backend(backend))
                .await
                .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
        authenticate_with_agent(&mut session, username.into(), &mut agent).await?;
        Ok(Self {
            session,
            forward_routes,
        })
    }

    pub async fn connect_keyboard_interactive<A, R, F>(
        address: A,
        username: impl Into<String>,
        host_key: PinnedHostKey,
        mut responder: R,
    ) -> Result<Self, SshError>
    where
        A: tokio::net::ToSocketAddrs,
        R: FnMut(KeyboardInteractiveChallenge) -> F + Send,
        F: Future<Output = Result<Vec<String>, SshError>> + Send,
    {
        let (mut session, forward_routes) = connect_verified(address, host_key).await?;
        let mut response = tokio::time::timeout(
            SSH_AUTH_ROUND_TIMEOUT,
            session.authenticate_keyboard_interactive_start(username, None),
        )
        .await
        .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
        let mut rounds = 0_usize;
        loop {
            match response {
                russh::client::KeyboardInteractiveAuthResponse::Success => {
                    return Ok(Self {
                        session,
                        forward_routes,
                    });
                }
                russh::client::KeyboardInteractiveAuthResponse::Failure { .. } => {
                    return Err(SshError::AuthenticationRejected);
                }
                russh::client::KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    rounds = rounds.saturating_add(1);
                    let metadata_bytes = name
                        .len()
                        .saturating_add(instructions.len())
                        .saturating_add(prompts.iter().map(|prompt| prompt.prompt.len()).sum());
                    if rounds > MAX_KEYBOARD_INTERACTIVE_ROUNDS
                        || prompts.len() > MAX_KEYBOARD_INTERACTIVE_PROMPTS
                        || metadata_bytes > MAX_KEYBOARD_INTERACTIVE_METADATA_BYTES
                    {
                        return Err(SshError::KeyboardInteractiveLimitExceeded);
                    }
                    let challenge = KeyboardInteractiveChallenge {
                        name,
                        instructions,
                        prompts: prompts
                            .into_iter()
                            .map(|prompt| KeyboardInteractivePrompt {
                                text: prompt.prompt,
                                echo: prompt.echo,
                            })
                            .collect(),
                    };
                    let expected = challenge.prompts.len();
                    let answers = responder(challenge).await?;
                    if answers.len() != expected {
                        return Err(SshError::KeyboardInteractiveResponseCount {
                            expected,
                            actual: answers.len(),
                        });
                    }
                    response = tokio::time::timeout(
                        SSH_AUTH_ROUND_TIMEOUT,
                        session.authenticate_keyboard_interactive_respond(answers),
                    )
                    .await
                    .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
                }
            }
        }
    }

    pub async fn exec_with_pty(
        &self,
        command: impl Into<Vec<u8>>,
        rows: u32,
        cols: u32,
    ) -> Result<ExecResult, SshError> {
        let mut channel = self.session.channel_open_session().await?;
        channel
            .request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
            .await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status = None;
        let mut channel_eof = false;
        while let Some(message) = channel.wait().await {
            match message {
                russh::ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                russh::ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
                russh::ChannelMsg::ExitStatus {
                    exit_status: status,
                } => {
                    exit_status = Some(status);
                    if channel_eof {
                        break;
                    }
                }
                russh::ChannelMsg::Eof => {
                    channel_eof = true;
                    if exit_status.is_some() {
                        break;
                    }
                }
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(ExecResult {
            stdout,
            stderr,
            exit_status: exit_status.ok_or(SshError::MissingExitStatus)?,
        })
    }

    pub async fn open_sftp(&self) -> Result<cshell_sftp::SftpClient, SshError> {
        let channel = self.session.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        Ok(cshell_sftp::SftpClient::connect(channel.into_stream()).await?)
    }

    pub async fn disconnect(self) -> Result<(), SshError> {
        self.session
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await?;
        Ok(())
    }
}

async fn connect_verified<A>(
    address: A,
    host_key: PinnedHostKey,
) -> Result<(russh::client::Handle<VerifiedClient>, ForwardRoutes), SshError>
where
    A: tokio::net::ToSocketAddrs,
{
    let config = verified_config();

    let forward_routes = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let session = tokio::time::timeout(
        SSH_CONNECT_TIMEOUT,
        russh::client::connect(
            Arc::new(config),
            address,
            VerifiedClient {
                host_key,
                forward_routes: Arc::clone(&forward_routes),
            },
        ),
    )
    .await
    .map_err(|_elapsed| SshError::ConnectionTimeout)?
    .map_err(SshError::Protocol)?;
    Ok((session, forward_routes))
}

async fn connect_verified_stream<R>(
    stream: R,
    host_key: PinnedHostKey,
) -> Result<(russh::client::Handle<VerifiedClient>, ForwardRoutes), SshError>
where
    R: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let config = verified_config();
    let forward_routes = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let session = tokio::time::timeout(
        SSH_CONNECT_TIMEOUT,
        russh::client::connect_stream(
            Arc::new(config),
            stream,
            VerifiedClient {
                host_key,
                forward_routes: Arc::clone(&forward_routes),
            },
        ),
    )
    .await
    .map_err(|_elapsed| SshError::ConnectionTimeout)?
    .map_err(SshError::Protocol)?;
    Ok((session, forward_routes))
}

fn verified_config() -> russh::client::Config {
    let mut config = RusshProvider::default().client_config();
    // Interactive sessions may legitimately remain idle for a long time.
    // Bound connection establishment externally instead of enabling russh's
    // whole-session inactivity garbage collection.
    config.inactivity_timeout = None;
    config.keepalive_interval = Some(Duration::from_secs(15));
    config.keepalive_max = 3;
    config
}

fn ensure_authenticated(result: russh::client::AuthResult) -> Result<(), SshError> {
    if result.success() {
        Ok(())
    } else {
        Err(SshError::AuthenticationRejected)
    }
}

async fn modern_rsa_hash(
    session: &russh::client::Handle<VerifiedClient>,
    key: &russh::keys::PrivateKey,
) -> Result<Option<russh::keys::ssh_key::HashAlg>, SshError> {
    if !key.algorithm().is_rsa() {
        return Ok(None);
    }
    match session.best_supported_rsa_hash().await? {
        Some(Some(hash)) => Ok(Some(hash)),
        // Older OpenSSH servers may omit RFC 8308 EXT_INFO while still
        // accepting RFC 8332. Prefer SHA-512 and never silently use ssh-rsa.
        None => Ok(Some(russh::keys::ssh_key::HashAlg::Sha512)),
        Some(None) => Err(SshError::LegacyRsaSignatureRejected),
    }
}

type DynamicAgent = russh::keys::agent::client::AgentClient<
    Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>,
>;

#[cfg(unix)]
async fn connect_agent_backend(backend: AgentBackend) -> Result<DynamicAgent, SshError> {
    match backend {
        AgentBackend::Auto | AgentBackend::OpenSsh => {
            russh::keys::agent::client::AgentClient::connect_env()
                .await
                .map(russh::keys::agent::client::AgentClient::dynamic)
                .map_err(SshError::Agent)
        }
        AgentBackend::Pageant => Err(SshError::AgentBackendUnsupported { backend }),
    }
}

#[cfg(windows)]
async fn connect_agent_backend(backend: AgentBackend) -> Result<DynamicAgent, SshError> {
    match backend {
        AgentBackend::OpenSsh => connect_windows_openssh_agent().await,
        AgentBackend::Pageant => connect_windows_pageant().await,
        AgentBackend::Auto => match connect_windows_openssh_agent().await {
            Ok(agent) => Ok(agent),
            Err(openssh_error) => connect_windows_pageant().await.map_err(|pageant_error| {
                SshError::AgentBackendsUnavailable(format!(
                    "OpenSSH: {openssh_error}; Pageant: {pageant_error}"
                ))
            }),
        },
    }
}

#[cfg(windows)]
async fn connect_windows_openssh_agent() -> Result<DynamicAgent, SshError> {
    russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map(russh::keys::agent::client::AgentClient::dynamic)
        .map_err(SshError::Agent)
}

#[cfg(windows)]
async fn connect_windows_pageant() -> Result<DynamicAgent, SshError> {
    russh::keys::agent::client::AgentClient::connect_pageant()
        .await
        .map(russh::keys::agent::client::AgentClient::dynamic)
        .map_err(SshError::Agent)
}

#[cfg(not(any(unix, windows)))]
async fn connect_agent_backend(backend: AgentBackend) -> Result<DynamicAgent, SshError> {
    Err(SshError::AgentBackendUnsupported { backend })
}

async fn authenticate_with_certificate(
    session: &mut russh::client::Handle<VerifiedClient>,
    username: String,
    private_key: SshPrivateKey,
    certificate: SshCertificate,
) -> Result<(), SshError> {
    let result = tokio::time::timeout(
        SSH_AUTH_ROUND_TIMEOUT,
        session.authenticate_openssh_cert(username, private_key.key, certificate.certificate),
    )
    .await
    .map_err(|_elapsed| SshError::AuthenticationTimeout)??;
    ensure_authenticated(result)
}

async fn authenticate_with_agent<S>(
    session: &mut russh::client::Handle<VerifiedClient>,
    username: String,
    agent: &mut russh::keys::agent::client::AgentClient<S>,
) -> Result<(), SshError>
where
    S: russh::keys::agent::client::AgentStream + Send + Unpin,
{
    let identities = tokio::time::timeout(SSH_AUTH_ROUND_TIMEOUT, agent.request_identities())
        .await
        .map_err(|_elapsed| SshError::AuthenticationTimeout)?
        .map_err(SshError::Agent)?;
    if identities.len() > MAX_AGENT_IDENTITIES {
        return Err(SshError::AgentIdentityLimit {
            actual: identities.len(),
            maximum: MAX_AGENT_IDENTITIES,
        });
    }
    for identity in identities {
        let hash_alg = if identity.public_key().algorithm().is_rsa() {
            match session.best_supported_rsa_hash().await? {
                Some(Some(hash)) => Some(hash),
                None => Some(russh::keys::ssh_key::HashAlg::Sha512),
                Some(None) => continue,
            }
        } else {
            None
        };
        let result = match identity {
            russh::keys::agent::AgentIdentity::PublicKey { key, .. } => {
                tokio::time::timeout(
                    SSH_AGENT_SIGN_TIMEOUT,
                    session.authenticate_publickey_with(username.clone(), key, hash_alg, agent),
                )
                .await
            }
            russh::keys::agent::AgentIdentity::Certificate { certificate, .. } => {
                tokio::time::timeout(
                    SSH_AGENT_SIGN_TIMEOUT,
                    session.authenticate_certificate_with(
                        username.clone(),
                        certificate,
                        hash_alg,
                        agent,
                    ),
                )
                .await
            }
        }
        .map_err(|_elapsed| SshError::AuthenticationTimeout)?
        .map_err(|error| SshError::AgentAuthentication(error.to_string()))?;
        if result.success() {
            return Ok(());
        }
    }
    Err(SshError::AuthenticationRejected)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        AgentBackend, AlgorithmPolicy, KeyboardInteractiveChallenge, PinnedHostKey, RusshClient,
        RusshProvider, SshCertificate, SshError, SshPrivateKey, SshProvider,
        authenticate_with_agent, connect_verified,
    };
    use futures::stream;
    use rand::rng;
    use russh::keys::ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey};
    use russh::server::{Auth, Msg, Response, Session};
    use russh::{Channel, ChannelId};
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn modern_policy_is_the_default() {
        let provider = RusshProvider::default();
        assert_eq!(provider.algorithm_policy(), AlgorithmPolicy::Modern);
        assert_eq!(
            provider.capabilities().x11_forwarding,
            cfg!(feature = "x11-forwarding")
        );
    }

    #[test]
    fn backend_type_is_hidden_behind_provider() {
        let provider = RusshProvider::default();
        let _config = provider.client_config();
        assert_eq!(provider.name(), "russh");
    }

    #[derive(Debug, Default)]
    struct ProtocolTestServer {
        accepted_public_key: Option<PublicKey>,
        accepted_certificate_key_id: Option<String>,
        channels: HashMap<ChannelId, Channel<Msg>>,
        remote_forwards: HashMap<(String, u32), tokio::sync::oneshot::Sender<()>>,
    }

    #[derive(Debug, Default)]
    struct InitOnlySftpServer;

    impl russh_sftp::server::Handler for InitOnlySftpServer {
        type Error = russh_sftp::protocol::StatusCode;

        fn unimplemented(&self) -> Self::Error {
            russh_sftp::protocol::StatusCode::OpUnsupported
        }
    }

    impl russh::server::Handler for ProtocolTestServer {
        type Error = russh::Error;

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if user == "cshell" && password == "phase0" {
                Auth::Accept
            } else {
                Auth::reject()
            })
        }

        async fn auth_publickey(
            &mut self,
            user: &str,
            public_key: &PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(
                if user == "cshell" && self.accepted_public_key.as_ref() == Some(public_key) {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }

        async fn auth_openssh_certificate(
            &mut self,
            user: &str,
            certificate: &russh::keys::ssh_key::Certificate,
        ) -> Result<Auth, Self::Error> {
            Ok(
                if user == "cshell"
                    && self.accepted_certificate_key_id.as_deref() == Some(certificate.key_id())
                {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }

        async fn auth_keyboard_interactive<'a>(
            &'a mut self,
            user: &str,
            _submethods: &str,
            response: Option<Response<'a>>,
        ) -> Result<Auth, Self::Error> {
            if user != "cshell" {
                return Ok(Auth::reject());
            }
            let Some(response) = response else {
                return Ok(Auth::Partial {
                    name: Cow::Borrowed("CShell integration"),
                    instructions: Cow::Borrowed("Complete both prompts"),
                    prompts: Cow::Owned(vec![
                        (Cow::Borrowed("Verification code: "), false),
                        (Cow::Borrowed("Visible label: "), true),
                    ]),
                });
            };
            let responses: Vec<_> = response.collect();
            Ok(
                if responses.len() == 2
                    && responses[0].as_ref() == b"654321"
                    && responses[1].as_ref() == b"operator"
                {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        fn channel_open_direct_tcpip(
            &mut self,
            channel: Channel<Msg>,
            host_to_connect: &str,
            port_to_connect: u32,
            _originator_address: &str,
            _originator_port: u32,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            let host = host_to_connect.to_owned();
            async move {
                let Ok(port) = u16::try_from(port_to_connect) else {
                    reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
                    return Ok(());
                };
                match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                    Ok(mut socket) => {
                        reply.accept().await;
                        tokio::spawn(async move {
                            let mut stream = channel.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                        });
                    }
                    Err(_) => {
                        reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
                    }
                }
                Ok(())
            }
        }

        fn tcpip_forward(
            &mut self,
            address: &str,
            port: &mut u32,
            session: &mut Session,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            let address = address.to_owned();
            let requested_port = *port;
            let handle = session.handle();
            async move {
                let Ok(requested_port) = u16::try_from(requested_port) else {
                    return Ok(false);
                };
                let Ok(listener) =
                    tokio::net::TcpListener::bind((address.as_str(), requested_port)).await
                else {
                    return Ok(false);
                };
                let actual_port = listener.local_addr()?.port();
                *port = u32::from(actual_port);
                let key = (address.clone(), u32::from(actual_port));
                let (stop, mut stopped) = tokio::sync::oneshot::channel();
                if self.remote_forwards.insert(key, stop).is_some() {
                    return Ok(false);
                }
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = &mut stopped => break,
                            accepted = listener.accept() => {
                                let Ok((mut socket, peer)) = accepted else { break };
                                let Ok(channel) = handle.channel_open_forwarded_tcpip(
                                    address.clone(),
                                    u32::from(actual_port),
                                    peer.ip().to_string(),
                                    u32::from(peer.port()),
                                ).await else { break };
                                tokio::spawn(async move {
                                    let mut stream = channel.into_stream();
                                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                                });
                            }
                        }
                    }
                });
                Ok(true)
            }
        }

        fn cancel_tcpip_forward(
            &mut self,
            address: &str,
            port: u32,
            _session: &mut Session,
        ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
            let stopped = self.remote_forwards.remove(&(address.to_owned(), port));
            async move {
                if let Some(stop) = stopped {
                    let _ = stop.send(());
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }

        async fn subsystem_request(
            &mut self,
            channel: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if name != "sftp" {
                session.channel_failure(channel)?;
                return Ok(());
            }
            let Some(channel_stream) = self.channels.remove(&channel) else {
                session.channel_failure(channel)?;
                return Ok(());
            };
            session.channel_success(channel)?;
            russh_sftp::server::run(channel_stream.into_stream(), InitOnlySftpServer).await;
            Ok(())
        }

        async fn pty_request(
            &mut self,
            channel: ChannelId,
            term: &str,
            col_width: u32,
            row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if term == "xterm-256color" && col_width == 80 && row_height == 24 {
                session.channel_success(channel)?;
            } else {
                session.channel_failure(channel)?;
            }
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            command: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if command == b"phase0-probe" {
                session.channel_success(channel)?;
                session.data(channel, b"CSHELL_SSH_OK\r\n".to_vec())?;
                session.exit_status_request(channel, 0)?;
            } else {
                session.channel_failure(channel)?;
                session.exit_status_request(channel, 127)?;
            }
            session.eof(channel)?;
            session.close(channel)?;
            Ok(())
        }
    }

    async fn spawn_protocol_server(
        accepted_public_key: Option<PublicKey>,
    ) -> (std::net::SocketAddr, String, tokio::task::JoinHandle<()>) {
        spawn_protocol_server_with_auth(accepted_public_key, None).await
    }

    async fn spawn_protocol_server_with_auth(
        accepted_public_key: Option<PublicKey>,
        accepted_certificate_key_id: Option<String>,
    ) -> (std::net::SocketAddr, String, tokio::task::JoinHandle<()>) {
        let host_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let fingerprint = host_key.fingerprint(HashAlg::Sha256).to_string();
        let mut server_config = russh::server::Config::default();
        server_config.keys.push(host_key);
        server_config.auth_rejection_time = std::time::Duration::from_millis(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let running = russh::server::run_stream(
                Arc::new(server_config),
                stream,
                ProtocolTestServer {
                    accepted_public_key,
                    accepted_certificate_key_id,
                    ..ProtocolTestServer::default()
                },
            )
            .await
            .unwrap();
            if let Err(error) = running.await {
                assert!(
                    matches!(
                        error,
                        russh::Error::IO(ref error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::UnexpectedEof
                            )
                    ),
                    "unexpected SSH test server failure: {error}"
                );
            }
        });
        (address, fingerprint, server)
    }

    fn create_user_certificate(
        user_key: &PrivateKey,
        cert_type: russh::keys::ssh_key::certificate::CertType,
    ) -> russh::keys::ssh_key::Certificate {
        let ca_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut builder = russh::keys::ssh_key::certificate::Builder::new_with_random_nonce(
            &mut rng(),
            user_key.public_key(),
            now.saturating_sub(60),
            now.saturating_add(3600),
        )
        .unwrap();
        builder.serial(7).unwrap();
        builder.key_id("cshell-test-certificate").unwrap();
        builder.cert_type(cert_type).unwrap();
        builder.valid_principal("cshell").unwrap();
        builder.sign(&ca_key).unwrap()
    }

    async fn await_protocol_server(server: tokio::task::JoinHandle<()>) {
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    async fn spawn_echo_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut payload = [0_u8; 5];
            tokio::io::AsyncReadExt::read_exact(&mut socket, &mut payload)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut socket, &payload)
                .await
                .unwrap();
        });
        (address, server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_russh_password_host_key_pty_and_exec_round_trip() {
        let (address, fingerprint, server) = spawn_protocol_server(None).await;

        let client = RusshClient::connect_password(
            address,
            "cshell",
            "phase0",
            PinnedHostKey::sha256(fingerprint),
        )
        .await
        .unwrap();
        let result = client.exec_with_pty(b"phase0-probe", 24, 80).await.unwrap();
        assert_eq!(result.exit_status, 0);
        assert_eq!(result.stdout, b"CSHELL_SSH_OK\r\n");
        assert!(result.stderr.is_empty());
        client.disconnect().await.unwrap();
        await_protocol_server(server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_russh_authenticated_sftp_subsystem_round_trip() {
        let (address, fingerprint, server) = spawn_protocol_server(None).await;
        let client = RusshClient::connect_password(
            address,
            "cshell",
            "phase0",
            PinnedHostKey::sha256(fingerprint),
        )
        .await
        .unwrap();
        let sftp = client.open_sftp().await.unwrap();
        sftp.close().await.unwrap();
        client.disconnect().await.unwrap();
        await_protocol_server(server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_russh_openssh_private_key_host_key_pty_and_exec_round_trip() {
        let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let encoded = client_key.to_openssh(LineEnding::LF).unwrap();
        let private_key = SshPrivateKey::decode_openssh(&encoded, None).unwrap();
        assert_eq!(
            private_key.sha256_fingerprint(),
            client_key.fingerprint(HashAlg::Sha256).to_string()
        );
        assert!(!format!("{private_key:?}").contains(encoded.as_str()));
        let (address, fingerprint, server) =
            spawn_protocol_server(Some(client_key.public_key().clone())).await;

        let client = RusshClient::connect_public_key(
            address,
            "cshell",
            private_key,
            PinnedHostKey::sha256(fingerprint),
        )
        .await
        .unwrap();
        let result = client.exec_with_pty(b"phase0-probe", 24, 80).await.unwrap();
        assert_eq!(result.exit_status, 0);
        assert_eq!(result.stdout, b"CSHELL_SSH_OK\r\n");
        assert!(result.stderr.is_empty());
        client.disconnect().await.unwrap();
        await_protocol_server(server).await;
    }

    #[test]
    fn certificate_rejects_host_type_and_mismatched_private_key() {
        let user_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let host_certificate =
            create_user_certificate(&user_key, russh::keys::ssh_key::certificate::CertType::Host);
        let host_encoded = host_certificate.to_openssh().unwrap();
        assert!(matches!(
            SshCertificate::decode_openssh(&host_encoded),
            Err(SshError::HostCertificateForUserAuthentication)
        ));

        let user_certificate =
            create_user_certificate(&user_key, russh::keys::ssh_key::certificate::CertType::User);
        let certificate =
            SshCertificate::decode_openssh(&user_certificate.to_openssh().unwrap()).unwrap();
        let other_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let other_private =
            SshPrivateKey::decode_openssh(&other_key.to_openssh(LineEnding::LF).unwrap(), None)
                .unwrap();
        assert!(matches!(
            certificate.validate_private_key(&other_private),
            Err(SshError::CertificateKeyMismatch)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_russh_openssh_user_certificate_auth_round_trip() {
        let user_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let private_key =
            SshPrivateKey::decode_openssh(&user_key.to_openssh(LineEnding::LF).unwrap(), None)
                .unwrap();
        let raw_certificate =
            create_user_certificate(&user_key, russh::keys::ssh_key::certificate::CertType::User);
        let encoded_certificate = raw_certificate.to_openssh().unwrap();
        let certificate = SshCertificate::decode_openssh(&encoded_certificate).unwrap();
        assert_eq!(certificate.key_id(), "cshell-test-certificate");
        assert_eq!(certificate.valid_principals(), &["cshell".to_owned()]);
        assert!(!format!("{certificate:?}").contains(&encoded_certificate));

        let (address, fingerprint, server) =
            spawn_protocol_server_with_auth(None, Some("cshell-test-certificate".to_owned())).await;
        let client = RusshClient::connect_certificate(
            address,
            "cshell",
            private_key,
            certificate,
            PinnedHostKey::sha256(fingerprint),
        )
        .await
        .unwrap();
        let result = client.exec_with_pty(b"phase0-probe", 24, 80).await.unwrap();
        assert_eq!(result.exit_status, 0);
        assert_eq!(result.stdout, b"CSHELL_SSH_OK\r\n");
        client.disconnect().await.unwrap();
        await_protocol_server(server).await;
    }

    #[test]
    fn agent_backend_defaults_to_auto() {
        assert_eq!(AgentBackend::default(), AgentBackend::Auto);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pageant_is_explicitly_rejected_off_windows() {
        assert!(matches!(
            super::connect_agent_backend(AgentBackend::Pageant).await,
            Err(SshError::AgentBackendUnsupported {
                backend: AgentBackend::Pageant
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_russh_keyboard_interactive_challenge_pty_and_exec_round_trip() {
        let (address, fingerprint, server) = spawn_protocol_server(None).await;
        let mut challenge_count = 0_usize;
        let client = RusshClient::connect_keyboard_interactive(
            address,
            "cshell",
            PinnedHostKey::sha256(fingerprint),
            |challenge: KeyboardInteractiveChallenge| {
                challenge_count += 1;
                async move {
                    assert_eq!(challenge.name, "CShell integration");
                    assert_eq!(challenge.instructions, "Complete both prompts");
                    assert_eq!(challenge.prompts.len(), 2);
                    assert_eq!(challenge.prompts[0].text, "Verification code: ");
                    assert!(!challenge.prompts[0].echo);
                    assert_eq!(challenge.prompts[1].text, "Visible label: ");
                    assert!(challenge.prompts[1].echo);
                    Ok(vec!["654321".to_owned(), "operator".to_owned()])
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(challenge_count, 1);
        let result = client.exec_with_pty(b"phase0-probe", 24, 80).await.unwrap();
        assert_eq!(result.exit_status, 0);
        assert_eq!(result.stdout, b"CSHELL_SSH_OK\r\n");
        assert!(result.stderr.is_empty());
        client.disconnect().await.unwrap();
        await_protocol_server(server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_russh_agent_signature_host_key_pty_and_exec_round_trip() {
        let rejected_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let accepted_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let (address, fingerprint, server) =
            spawn_protocol_server(Some(accepted_key.public_key().clone())).await;

        let (agent_client_stream, agent_server_stream) = tokio::io::duplex(64 * 1024);
        russh::keys::agent::server::serve(
            stream::iter([Ok::<_, std::io::Error>(agent_server_stream)]),
            (),
        )
        .await
        .unwrap();
        let mut agent = russh::keys::agent::client::AgentClient::connect(agent_client_stream);
        agent.add_identity(&rejected_key, &[]).await.unwrap();
        agent.add_identity(&accepted_key, &[]).await.unwrap();

        let (mut session, forward_routes) =
            connect_verified(address, PinnedHostKey::sha256(fingerprint))
                .await
                .unwrap();
        authenticate_with_agent(&mut session, "cshell".to_owned(), &mut agent)
            .await
            .unwrap();
        let client = RusshClient {
            session,
            forward_routes,
        };
        let result = client.exec_with_pty(b"phase0-probe", 24, 80).await.unwrap();
        assert_eq!(result.exit_status, 0);
        assert_eq!(result.stdout, b"CSHELL_SSH_OK\r\n");
        assert!(result.stderr.is_empty());
        client.disconnect().await.unwrap();
        await_protocol_server(server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_forward_bridges_tcp_through_authenticated_ssh() {
        let (target, echo_server) = spawn_echo_server().await;
        let (ssh_address, fingerprint, ssh_server) = spawn_protocol_server(None).await;
        let client = Arc::new(
            RusshClient::connect_password(
                ssh_address,
                "cshell",
                "phase0",
                PinnedHostKey::sha256(fingerprint),
            )
            .await
            .unwrap(),
        );
        let forward = client
            .start_local_forward(
                "127.0.0.1:0".parse().unwrap(),
                target.ip().to_string(),
                target.port(),
                super::ForwardLimits::default(),
            )
            .await
            .unwrap();
        let mut socket =
            tokio::net::TcpStream::connect((forward.bound_address(), forward.bound_port()))
                .await
                .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut socket, b"hello")
            .await
            .unwrap();
        let mut echoed = [0_u8; 5];
        tokio::io::AsyncReadExt::read_exact(&mut socket, &mut echoed)
            .await
            .unwrap();
        assert_eq!(&echoed, b"hello");
        forward.shutdown().await.unwrap();
        echo_server.await.unwrap();
        Arc::try_unwrap(client).unwrap().disconnect().await.unwrap();
        await_protocol_server(ssh_server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dynamic_forward_negotiates_socks5_and_bridges_tcp() {
        let (target, echo_server) = spawn_echo_server().await;
        let (ssh_address, fingerprint, ssh_server) = spawn_protocol_server(None).await;
        let client = Arc::new(
            RusshClient::connect_password(
                ssh_address,
                "cshell",
                "phase0",
                PinnedHostKey::sha256(fingerprint),
            )
            .await
            .unwrap(),
        );
        let forward = client
            .start_dynamic_forward(
                "127.0.0.1:0".parse().unwrap(),
                super::ForwardLimits::default(),
            )
            .await
            .unwrap();
        let mut socket =
            tokio::net::TcpStream::connect((forward.bound_address(), forward.bound_port()))
                .await
                .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut socket, &[5, 1, 0])
            .await
            .unwrap();
        let mut method = [0_u8; 2];
        tokio::io::AsyncReadExt::read_exact(&mut socket, &mut method)
            .await
            .unwrap();
        assert_eq!(method, [5, 0]);
        let octets = match target.ip() {
            std::net::IpAddr::V4(address) => address.octets(),
            std::net::IpAddr::V6(_) => unreachable!(),
        };
        let mut request = vec![5, 1, 0, 1];
        request.extend_from_slice(&octets);
        request.extend_from_slice(&target.port().to_be_bytes());
        tokio::io::AsyncWriteExt::write_all(&mut socket, &request)
            .await
            .unwrap();
        let mut reply = [0_u8; 10];
        tokio::io::AsyncReadExt::read_exact(&mut socket, &mut reply)
            .await
            .unwrap();
        assert_eq!(reply[1], 0);
        tokio::io::AsyncWriteExt::write_all(&mut socket, b"hello")
            .await
            .unwrap();
        let mut echoed = [0_u8; 5];
        tokio::io::AsyncReadExt::read_exact(&mut socket, &mut echoed)
            .await
            .unwrap();
        assert_eq!(&echoed, b"hello");
        forward.shutdown().await.unwrap();
        echo_server.await.unwrap();
        Arc::try_unwrap(client).unwrap().disconnect().await.unwrap();
        await_protocol_server(ssh_server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remote_forward_accepts_server_side_tcp_and_cancels_cleanly() {
        let (target, echo_server) = spawn_echo_server().await;
        let (ssh_address, fingerprint, ssh_server) = spawn_protocol_server(None).await;
        let client = Arc::new(
            RusshClient::connect_password(
                ssh_address,
                "cshell",
                "phase0",
                PinnedHostKey::sha256(fingerprint),
            )
            .await
            .unwrap(),
        );
        let forward = client
            .start_remote_forward(
                "127.0.0.1",
                0,
                super::RemoteForwardTarget {
                    host: target.ip().to_string(),
                    port: target.port(),
                },
                super::ForwardLimits::default(),
            )
            .await
            .unwrap();
        assert_ne!(forward.bound_port(), 0);
        let mut socket =
            tokio::net::TcpStream::connect((forward.bound_address(), forward.bound_port()))
                .await
                .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut socket, b"hello")
            .await
            .unwrap();
        let mut echoed = [0_u8; 5];
        tokio::io::AsyncReadExt::read_exact(&mut socket, &mut echoed)
            .await
            .unwrap();
        assert_eq!(&echoed, b"hello");
        drop(socket);
        forward.shutdown().await.unwrap();
        echo_server.await.unwrap();
        Arc::try_unwrap(client).unwrap().disconnect().await.unwrap();
        await_protocol_server(ssh_server).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_hop_stream_performs_verified_ssh_handshake_at_each_hop() {
        let (target_address, target_fingerprint, target_server) = spawn_protocol_server(None).await;
        let (second_address, second_fingerprint, second_server) = spawn_protocol_server(None).await;
        let (first_address, first_fingerprint, first_server) = spawn_protocol_server(None).await;
        let first = RusshClient::connect_password(
            first_address,
            "cshell",
            "phase0",
            PinnedHostKey::sha256(first_fingerprint),
        )
        .await
        .unwrap();
        let second = RusshClient::connect_password_via(
            &first,
            second_address.ip().to_string(),
            second_address.port(),
            "cshell",
            "phase0",
            PinnedHostKey::sha256(second_fingerprint),
        )
        .await
        .unwrap();
        let target = RusshClient::connect_password_via(
            &second,
            target_address.ip().to_string(),
            target_address.port(),
            "cshell",
            "phase0",
            PinnedHostKey::sha256(target_fingerprint),
        )
        .await
        .unwrap();
        let result = target.exec_with_pty(b"phase0-probe", 24, 80).await.unwrap();
        assert_eq!(result.stdout, b"CSHELL_SSH_OK\r\n");
        target.disconnect().await.unwrap();
        second.disconnect().await.unwrap();
        first.disconnect().await.unwrap();
        await_protocol_server(target_server).await;
        await_protocol_server(second_server).await;
        await_protocol_server(first_server).await;
    }
}
