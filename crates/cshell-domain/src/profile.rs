//! Profile catalog metadata and terminal setting inheritance.
//! Connection details and secrets live in separate records and the vault.

use crate::{FolderId, ProfileId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingSource {
    Global,
    Folder(FolderId),
    Profile(ProfileId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedField<T> {
    pub value: T,
    pub source: SettingSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
