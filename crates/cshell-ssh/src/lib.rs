//! SSH provider boundary and the initial russh capability declaration.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

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
}

#[derive(Debug, Error)]
pub enum SshError {
    #[error(transparent)]
    Protocol(#[from] russh::Error),
    #[error("SSH authentication was rejected")]
    AuthenticationRejected,
    #[error("SSH connection attempt timed out")]
    ConnectionTimeout,
    #[error("SSH command channel closed without an exit status")]
    MissingExitStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: u32,
}

pub struct RusshClient {
    session: russh::client::Handle<VerifiedClient>,
}

impl std::fmt::Debug for RusshClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RusshClient")
            .finish_non_exhaustive()
    }
}

impl RusshClient {
    pub async fn connect_password<A>(
        address: A,
        username: impl Into<String>,
        password: impl Into<String>,
        host_key: PinnedHostKey,
    ) -> Result<Self, SshError>
    where
        A: tokio::net::ToSocketAddrs,
    {
        let mut config = RusshProvider::default().client_config();
        // Interactive sessions may legitimately remain idle for a long time.
        // Bound connection establishment externally instead of enabling
        // russh's whole-session inactivity garbage collection.
        config.inactivity_timeout = None;
        config.keepalive_interval = Some(Duration::from_secs(15));
        config.keepalive_max = 3;
        let mut session = tokio::time::timeout(
            Duration::from_secs(30),
            russh::client::connect(Arc::new(config), address, VerifiedClient { host_key }),
        )
        .await
        .map_err(|_elapsed| SshError::ConnectionTimeout)??;
        if !session
            .authenticate_password(username, password)
            .await?
            .success()
        {
            return Err(SshError::AuthenticationRejected);
        }
        Ok(Self { session })
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

    pub async fn disconnect(self) -> Result<(), SshError> {
        self.session
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{AlgorithmPolicy, PinnedHostKey, RusshClient, RusshProvider, SshProvider};
    use rand::rng;
    use russh::keys::ssh_key::{Algorithm, HashAlg, PrivateKey};
    use russh::server::{Auth, Msg, Session};
    use russh::{Channel, ChannelId};
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

    #[derive(Debug)]
    struct ProtocolTestServer;

    impl russh::server::Handler for ProtocolTestServer {
        type Error = russh::Error;

        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if user == "cshell" && password == "phase0" {
                Auth::Accept
            } else {
                Auth::reject()
            })
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<Msg>,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_russh_password_host_key_pty_and_exec_round_trip() {
        let host_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let fingerprint = host_key.fingerprint(HashAlg::Sha256).to_string();
        let mut server_config = russh::server::Config::default();
        server_config.keys.push(host_key);
        server_config.auth_rejection_time = std::time::Duration::from_millis(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let running =
                russh::server::run_stream(Arc::new(server_config), stream, ProtocolTestServer)
                    .await
                    .unwrap();
            if let Err(error) = running.await {
                assert!(
                    matches!(
                        error,
                        russh::Error::IO(ref error)
                            if error.kind() == std::io::ErrorKind::ConnectionReset
                    ),
                    "unexpected SSH test server failure: {error}"
                );
            }
        });

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
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
