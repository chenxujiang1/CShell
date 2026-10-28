//! Backend-independent SSH requirements. No host, command or credential data.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshProviderId {
    Russh,
    SystemOpenSsh,
    Libssh,
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAlgorithmPolicy {
    #[default]
    Modern,
    Compatible,
    Legacy,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshCapability {
    Password,
    PublicKey,
    KeyboardInteractive,
    Agent,
    OpenSshAgent,
    Pageant,
    UserCertificate,
    HostCertificate,
    Pty,
    Exec,
    Sftp,
    DirectTcpip,
    JumpHost,
    LocalForward,
    RemoteForward,
    DynamicForward,
    Socks5Proxy,
    HttpConnectProxy,
    Gssapi,
    Fido2,
    Pkcs11,
    ProxyCommand,
    X11Forwarding,
}
impl SshCapability {
    pub const ALL: [Self; 23] = [
        Self::Password,
        Self::PublicKey,
        Self::KeyboardInteractive,
        Self::Agent,
        Self::OpenSshAgent,
        Self::Pageant,
        Self::UserCertificate,
        Self::HostCertificate,
        Self::Pty,
        Self::Exec,
        Self::Sftp,
        Self::DirectTcpip,
        Self::JumpHost,
        Self::LocalForward,
        Self::RemoteForward,
        Self::DynamicForward,
        Self::Socks5Proxy,
        Self::HttpConnectProxy,
        Self::Gssapi,
        Self::Fido2,
        Self::Pkcs11,
        Self::ProxyCommand,
        Self::X11Forwarding,
    ];
    pub const fn label(self) -> &'static str {
        match self {
            Self::Password => "Password",
            Self::PublicKey => "Public key",
            Self::KeyboardInteractive => "Keyboard interactive",
            Self::Agent => "SSH agent",
            Self::OpenSshAgent => "OpenSSH agent",
            Self::Pageant => "Pageant",
            Self::UserCertificate => "OpenSSH user certificate",
            Self::HostCertificate => "OpenSSH host certificate",
            Self::Pty => "PTY",
            Self::Exec => "Exec",
            Self::Sftp => "SFTP",
            Self::DirectTcpip => "Direct TCP/IP",
            Self::JumpHost => "Jump host",
            Self::LocalForward => "Local forward",
            Self::RemoteForward => "Remote forward",
            Self::DynamicForward => "Dynamic SOCKS5 forward",
            Self::Socks5Proxy => "SOCKS5 proxy",
            Self::HttpConnectProxy => "HTTP CONNECT proxy",
            Self::Gssapi => "GSSAPI",
            Self::Fido2 => "Native FIDO2",
            Self::Pkcs11 => "Native PKCS#11",
            Self::ProxyCommand => "ProxyCommand",
            Self::X11Forwarding => "X11 forwarding",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshProviderRequirements {
    /// None selects the first matching registered provider, before any I/O.
    #[serde(default)]
    pub provider: Option<SshProviderId>,
    #[serde(default)]
    pub capabilities: BTreeSet<SshCapability>,
    #[serde(default)]
    pub jump_hops: u8,
    #[serde(default)]
    pub algorithm_policy: SshAlgorithmPolicy,
}
impl SshProviderRequirements {
    pub fn effective_capabilities(&self) -> BTreeSet<SshCapability> {
        let mut required = self.capabilities.clone();
        if self.jump_hops > 0 {
            required.insert(SshCapability::JumpHost);
        }
        if required.contains(&SshCapability::JumpHost)
            || required.contains(&SshCapability::LocalForward)
            || required.contains(&SshCapability::DynamicForward)
        {
            required.insert(SshCapability::DirectTcpip);
        }
        if required.contains(&SshCapability::UserCertificate) {
            required.insert(SshCapability::PublicKey);
        }
        if required.contains(&SshCapability::OpenSshAgent)
            || required.contains(&SshCapability::Pageant)
        {
            required.insert(SshCapability::Agent);
        }
        required
    }
}
