//! Profile catalog metadata and terminal setting inheritance.
//! Connection details and secrets live in separate records and the vault.

use crate::{FolderId, ProfileId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const DEFAULT_TERMINAL_FONT_PIXELS: u16 = 28;
pub const MIN_TERMINAL_FONT_PIXELS: u16 = 12;
pub const MAX_TERMINAL_FONT_PIXELS: u16 = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Ssh,
    Local,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalOverrides {
    pub terminal_type: Option<String>,
    pub theme: Option<String>,
    pub logging: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalDefaults {
    pub terminal_type: String,
    pub theme: String,
    pub logging: bool,
}

impl Default for TerminalDefaults {
    fn default() -> Self {
        Self {
            terminal_type: "xterm-256color".into(),
            theme: "default".into(),
            logging: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFolder {
    pub id: FolderId,
    pub name: String,
    pub parent_id: Option<FolderId>,
    pub terminal: TerminalOverrides,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRecord {
    pub id: ProfileId,
    pub name: String,
    pub kind: ProfileKind,
    pub folder_id: Option<FolderId>,
    pub tags: BTreeSet<String>,
    pub favorite: bool,
    pub terminal: TerminalOverrides,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshConnectionRecord {
    #[serde(default, skip_serializing_if = "SshConnectionOptions::is_default")]
    pub options: SshConnectionOptions,
    pub profile_id: ProfileId,
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub auth_method: SshAuthMethod,
    #[serde(default)]
    pub private_key_path: Option<String>,
    #[serde(default)]
    pub certificate_path: Option<String>,
    #[serde(default)]
    pub agent_backend: SshAgentBackend,
    #[serde(default)]
    pub agent_identity: Option<String>,
    #[serde(default)]
    pub route: SshRoute,
}

/// Non-secret transport settings. Missing settings preserve legacy behavior.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SshConnectionOptions {
    /// None inherits the desktop's live global transfer default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sftp_verification: Option<SftpVerification>,
    pub authentication_order: Vec<SshAuthStep>,
    pub keyboard_interactive: bool,
    /// None inherits the desktop's global ZMODEM download directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zmodem_download_directory: Option<String>,
    pub keepalive_enabled: bool,
    pub keepalive_interval_secs: u32,
    pub keepalive_max: u32,
    pub auto_reconnect: bool,
    pub reconnect_delay_secs: u32,
    pub reconnect_attempts: u32,
}

impl Default for SshConnectionOptions {
    fn default() -> Self {
        Self {
            sftp_verification: None,
            authentication_order: Vec::new(),
            keyboard_interactive: false,
            zmodem_download_directory: None,
            keepalive_enabled: true,
            keepalive_interval_secs: 15,
            keepalive_max: 3,
            auto_reconnect: false,
            reconnect_delay_secs: 5,
            reconnect_attempts: 3,
        }
    }
}

/// Checks performed for new transfers. Legacy queue documents retain their original rules.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpVerification {
    #[default]
    Standard,
    Full,
}

impl SshConnectionOptions {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn new_profile() -> Self {
        Self { authentication_order: vec![SshAuthStep::KeyboardInteractive,SshAuthStep::Password], auto_reconnect: true, ..Self::default() }
    }

    pub fn is_valid(&self) -> bool {
        self.authentication_order.len()<=5 && self.authentication_order.iter().collect::<std::collections::BTreeSet<_>>().len()==self.authentication_order.len()
            && self.zmodem_download_directory.as_ref().is_none_or(|p| !p.trim().is_empty() && p.len() <= 4096 && !p.contains('\0'))
            && (1..=3600).contains(&self.keepalive_interval_secs)
            && (1..=10).contains(&self.keepalive_max)
            && (1..=300).contains(&self.reconnect_delay_secs)
            && (1..=20).contains(&self.reconnect_attempts)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SshRoute {
    #[default]
    Direct,
    Socks5 {
        host: String,
        port: u16,
    },
    HttpConnect {
        host: String,
        port: u16,
    },
    Jump {
        profile_id: ProfileId,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAuthMethod {
    #[default]
    Password,
    PrivateKey,
    Certificate,
    Agent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAgentBackend {
    #[default]
    Auto,
    OpenSsh,
    Pageant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum SettingSource {
    Global,
    Folder(FolderId),
    Profile(ProfileId),
    Tag(crate::TerminalPolicyId),
    Host(crate::HostId),
    Temporary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedField<T> {
    pub value: T,
    pub source: SettingSource,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedTerminalSettings {
    pub terminal_type: ResolvedField<String>,
    pub theme: ResolvedField<String>,
    pub logging: ResolvedField<bool>,
}

/// Saved, non-secret local process configuration. Arguments are passed verbatim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConnectionRecord {
    pub profile_id: ProfileId,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: LocalWorkingDirectory,
    pub env_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub close_policy: LocalClosePolicy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalClosePolicy {
    #[default]
    KeepAlive,
    TerminateOnViewClose,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalWorkingDirectory {
    #[default]
    Inherit,
    Home,
    Explicit {
        path: String,
    },
}

/// Private daemon/IPC variables must not be exported to terminal processes.
#[must_use]
pub fn reserved_local_environment_name(key: &str) -> bool {
    key.get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("CSHELL_"))
}

#[derive(Clone,Copy,Debug,Eq,PartialEq,Ord,PartialOrd,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum SshAuthStep {KeyboardInteractive,Password,PrivateKey,Certificate,Agent}
impl SshConnectionRecord {
    pub fn authentication_steps(&self)->Vec<SshAuthStep> {
        if !self.options.authentication_order.is_empty(){return self.options.authentication_order.clone();}
        match self.auth_method {
            SshAuthMethod::Password if self.options.keyboard_interactive => vec![SshAuthStep::KeyboardInteractive, SshAuthStep::Password],
            SshAuthMethod::Password => vec![SshAuthStep::Password],
            SshAuthMethod::PrivateKey => vec![SshAuthStep::PrivateKey],
            SshAuthMethod::Certificate => vec![SshAuthStep::Certificate],
            SshAuthMethod::Agent => vec![SshAuthStep::Agent],
        }
    }
    pub fn uses_password(&self)->bool {self.authentication_steps().iter().any(|m|matches!(m,SshAuthStep::Password|SshAuthStep::KeyboardInteractive))}
    pub fn uses_private_key(&self)->bool {self.authentication_steps().iter().any(|m|matches!(m,SshAuthStep::PrivateKey|SshAuthStep::Certificate))}
}
