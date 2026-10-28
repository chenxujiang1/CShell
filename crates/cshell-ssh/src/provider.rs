//! Registered adapters, preflight selection and a backend-neutral connection.
use crate::*;
use cshell_application::{SshProviderCatalog, SshProviderDescriptor};
use cshell_domain::{SshCapability as C, SshProviderId, SshProviderRequirements};
use std::{collections::BTreeSet, net::SocketAddr};

pub(crate) fn russh_descriptor() -> SshProviderDescriptor {
    let mut capabilities = BTreeSet::from([
        C::Password,
        C::PublicKey,
        C::KeyboardInteractive,
        C::UserCertificate,
        C::HostCertificate,
        C::Pty,
        C::Exec,
        C::Sftp,
        C::DirectTcpip,
        C::JumpHost,
        C::LocalForward,
        C::RemoteForward,
        C::DynamicForward,
        C::Socks5Proxy,
        C::HttpConnectProxy,
    ]);
    if cfg!(any(unix, windows)) {
        capabilities.extend([C::Agent, C::OpenSshAgent]);
    }
    if cfg!(windows) {
        capabilities.insert(C::Pageant);
    }
    SshProviderDescriptor {
        id: SshProviderId::Russh,
        capabilities,
        algorithm_policies: BTreeSet::from([AlgorithmPolicy::Modern]),
        max_jump_hops: cshell_domain::MAX_SSH_JUMP_HOPS as u8,
    }
}
pub fn registered_ssh_providers() -> SshProviderCatalog {
    SshProviderCatalog {
        format: "cshell.ssh-providers".into(),
        version: 1,
        providers: vec![russh_descriptor()],
    }
}
pub fn select_ssh_provider(
    requirements: &SshProviderRequirements,
) -> Result<Box<dyn SshProvider>, SshError> {
    let evaluation = registered_ssh_providers().evaluate(requirements);
    match evaluation.selected {
        Some(SshProviderId::Russh) => {
            Ok(Box::new(RusshProvider::new(requirements.algorithm_policy)))
        }
        _ => Err(SshError::ProviderUnsupported(evaluation.refusals)),
    }
}
pub(crate) fn authentication_requirements(auth: &SshAuthentication) -> SshProviderRequirements {
    let mut req = SshProviderRequirements::default();
    match auth {
        SshAuthentication::Ordered(ordered) => {for method in &ordered.methods {req.capabilities.insert(method.capability());}}
        SshAuthentication::Password(_) => {
            req.capabilities.insert(C::Password);
        }
        SshAuthentication::KeyboardInteractive { .. } => {
            req.capabilities.insert(C::KeyboardInteractive);
        }
        SshAuthentication::PrivateKey(_) => {
            req.capabilities.insert(C::PublicKey);
        }
        SshAuthentication::Certificate { .. } => {
            req.capabilities.insert(C::UserCertificate);
        }
        SshAuthentication::Agent { backend, .. } => {
            req.capabilities.insert(C::Agent);
            match backend {
                AgentBackend::OpenSsh => {
                    req.capabilities.insert(C::OpenSshAgent);
                }
                AgentBackend::Pageant => {
                    req.capabilities.insert(C::Pageant);
                }
                AgentBackend::Auto => (),
            }
        }
    }
    req
}

// Each future adapter supplies its own variant. Never expose russh handles to
// the daemon or implement Deref: callers only receive project-owned operations.
#[derive(Clone)]
pub struct SshConnection {
    backend: ConnectionBackend,
    descriptor: SshProviderDescriptor,
    authenticated_username: Option<String>,
}
#[derive(Clone)]
enum ConnectionBackend {
    Russh(Arc<RusshClient>),
}
impl std::fmt::Debug for SshConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnection")
            .field("provider", &self.provider_id())
            .finish_non_exhaustive()
    }
}
impl SshConnection {
    pub fn authenticated_username(&self)->Option<&str>{self.authenticated_username.as_deref()}
    pub(crate) fn with_authenticated_username(mut self,username:String)->Self{self.authenticated_username=Some(username);self}
    pub async fn connect_known_hosts(
        username: String,
        authentication: SshAuthentication,
        verifier: KnownHostsVerifier,
    ) -> Result<Self, SshError> {
        let provider = select_ssh_provider(&authentication_requirements(&authentication))?;
        let (host, port) = verifier.target();
        let stream = open_tcp_stream(host, port).await?;
        provider
            .connect(stream, username, authentication, verifier)
            .await
    }
    pub(crate) fn from_russh(client: RusshClient) -> Self {
        Self {
            backend: ConnectionBackend::Russh(Arc::new(client)),
            descriptor: russh_descriptor(),
            authenticated_username:None,
        }
    }
    pub fn provider_id(&self) -> SshProviderId {
        self.descriptor.id
    }
    pub fn descriptor(&self) -> &SshProviderDescriptor {
        &self.descriptor
    }
    fn russh(&self) -> &Arc<RusshClient> {
        match &self.backend {
            ConnectionBackend::Russh(client) => client,
        }
    }
    fn require(&self, capability: C) -> Result<(), SshError> {
        if self.descriptor.capabilities.contains(&capability) {
            Ok(())
        } else {
            Err(SshError::ProviderUnsupported(vec![
                cshell_application::SshProviderRefusal::MissingCapabilities {
                    provider: self.provider_id(),
                    capabilities: BTreeSet::from([capability]),
                },
            ]))
        }
    }
    pub fn verified_host_key_fingerprint(&self) -> &str {
        self.russh().verified_host_key_fingerprint()
    }
    pub fn transport_closed(&self) -> bool {
        self.russh().transport_closed()
    }
    pub async fn disconnect(&self) -> Result<(), SshError> {
        self.russh().disconnect().await
    }
    pub async fn disconnect_confirmed(&self) -> bool {
        self.russh().disconnect_confirmed().await
    }
    pub async fn open_terminal(&self, rows: u32, cols: u32) -> Result<SshTerminal, SshError> {
        self.require(C::Pty)?;
        self.russh().open_terminal(rows, cols).await
    }
    pub async fn open_terminal_with_type(
        &self,
        rows: u32,
        cols: u32,
        terminal_type: &str,
    ) -> Result<SshTerminal, SshError> {
        self.require(C::Pty)?;
        self.russh()
            .open_terminal_with_type(rows, cols, terminal_type)
            .await
    }
    pub async fn open_terminal_tracked(&self,rows:u32,cols:u32,terminal_type:&str,marker:Option<&str>)->Result<SshTerminal,SshError> {
        self.require(C::Pty)?;
        self.russh().open_terminal_tracked(rows,cols,terminal_type,marker).await
    }
    pub async fn remote_working_directory(&self,marker:&str)->Option<String> {
        self.require(C::Exec).ok()?;
        self.russh().remote_working_directory(marker).await
    }
    pub async fn open_exec(&self) -> Result<SshExecChannel, SshError> {
        self.require(C::Exec)?;
        self.russh().open_exec().await
    }
    pub async fn file_checksum_probe(&self,path:&str,nonce:&[u8])->bool {
        if self.require(C::Exec).is_err() {return false;}
        self.russh().file_checksum_probe(path,nonce).await
    }
    pub async fn file_sha256(&self,path:&str)->Option<[u8;32]> {
        self.require(C::Exec).ok()?;
        self.russh().file_sha256(path).await
    }
    pub async fn exec_with_pty(
        &self,
        command: impl Into<Vec<u8>>,
        rows: u32,
        cols: u32,
    ) -> Result<ExecResult, SshError> {
        self.require(C::Exec)?;
        self.require(C::Pty)?;
        self.russh().exec_with_pty(command, rows, cols).await
    }
    pub async fn open_sftp(&self) -> Result<cshell_sftp::SftpClient, SshError> {
        self.require(C::Sftp)?;
        self.russh().open_sftp().await
    }
    pub async fn open_direct_stream(
        &self,
        host: String,
        port: u16,
    ) -> Result<SshTransport, SshError> {
        self.require(C::DirectTcpip)?;
        Ok(Box::new(self.russh().open_direct_stream(host, port).await?))
    }
    pub async fn start_local_forward(
        &self,
        bind: SocketAddr,
        host: impl Into<String>,
        port: u16,
        limits: ForwardLimits,
    ) -> Result<ForwardHandle, SshError> {
        self.require(C::LocalForward)?;
        self.russh()
            .start_local_forward(bind, host, port, limits)
            .await
    }
    pub async fn start_dynamic_forward(
        &self,
        bind: SocketAddr,
        limits: ForwardLimits,
    ) -> Result<ForwardHandle, SshError> {
        self.require(C::DynamicForward)?;
        self.russh().start_dynamic_forward(bind, limits).await
    }
    pub async fn start_remote_forward(
        &self,
        address: impl Into<String>,
        port: u16,
        target: RemoteForwardTarget,
        limits: ForwardLimits,
    ) -> Result<ForwardHandle, SshError> {
        self.require(C::RemoteForward)?;
        self.russh()
            .start_remote_forward(address, port, target, limits)
            .await
    }
    pub async fn cancel_remote_forward(&self, address: &str, port: u16) -> Result<(), SshError> {
        self.require(C::RemoteForward)?;
        self.russh().cancel_remote_forward(address, port).await
    }
}
