//! Bounded Profile control messages shared by desktop and daemon.

use cshell_domain::{
    FolderId, LocalConnectionRecord, LocalWorkingDirectory, ProfileFolder, ProfileId, ProfileKind,
    ProfileRecord, SshAgentBackend, SshAuthMethod, SshConnectionRecord, TerminalDefaults,
    TerminalOverrides,
};
use prost::{Enumeration, Message};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use zeroize::Zeroize;

pub const MAX_PROFILE_CONTROL_CHANGES: usize = 2048;

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ProfileRequest {
    #[prost(enumeration = "ProfileOperation", tag = "1")]
    pub operation: i32,
    #[prost(uint64, tag = "2")]
    pub expected_revision: u64,
    #[prost(message, repeated, tag = "3")]
    pub changes: Vec<ProfileChange>,
    #[prost(bytes = "vec", tag = "4")]
    pub import_json: Vec<u8>,
    #[prost(enumeration = "ProfileImportPolicy", tag = "5")]
    pub import_policy: i32,
    #[prost(bytes = "vec", tag = "6")]
    pub credential_profile_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "7")]
    pub credential_secret: Vec<u8>,
    #[prost(bytes = "vec", tag = "8")]
    pub host_key_token: Vec<u8>,
    #[prost(string, tag = "9")]
    pub host_key_fingerprint: String,
    #[prost(bool, tag = "10")]
    pub include_local_context: bool,
    #[prost(bytes = "vec", repeated, tag = "11")]
    pub export_profile_ids: Vec<Vec<u8>>,
}

impl std::fmt::Debug for ProfileRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileRequest")
            .field("operation", &self.operation)
            .field("expected_revision", &self.expected_revision)
            .field("changes", &self.changes.len())
            .field("credential_secret", &"[REDACTED]")
            .field("host_key_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Drop for ProfileRequest {
    fn drop(&mut self) {
        self.credential_secret.zeroize();
        self.host_key_token.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileOperation {
    List = 0,
    ApplyChanges = 1,
    PreviewImport = 2,
    CommitImport = 3,
    SetPassword = 4,
    DeletePassword = 5,
    SetKeyPassphrase = 6,
    DeleteKeyPassphrase = 7,
    PreviewHostKey = 8,
    ConfirmHostKey = 9,
    DiscoverLocalShells = 10,
    ExportCatalog = 11,
    PreviewThemes = 12,
    CommitThemes = 13,
    ExportThemes = 14,
    ExportOpenSsh = 15,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileImportPolicy {
    Fail = 0,
    Skip = 1,
    Replace = 2,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct ProfileResponse {
    #[prost(enumeration = "ProfileStatus", tag = "1")]
    pub status: i32,
    #[prost(uint64, tag = "2")]
    pub revision: u64,
    #[prost(message, optional, tag = "3")]
    pub catalog: Option<ProfileCatalogData>,
    #[prost(message, optional, tag = "4")]
    pub preview: Option<ProfileImportPreviewData>,
    #[prost(string, tag = "5")]
    pub detail: String,
    #[prost(message, optional, boxed, tag = "6")]
    pub host_key_preview: Option<Box<HostKeyPreviewData>>,
    #[prost(message, repeated, tag = "7")]
    pub local_shells: Vec<LocalShellData>,
    #[prost(bytes = "vec", tag = "8")]
    pub export_json: Vec<u8>,
}

impl std::fmt::Debug for ProfileResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileResponse")
            .field("status", &self.status)
            .field("revision", &self.revision)
            .field("has_catalog", &self.catalog.is_some())
            .field("has_import_preview", &self.preview.is_some())
            .field("has_export", &!self.export_json.is_empty())
            .field("host_key_preview", &self.host_key_preview)
            .field("local_shell_count", &self.local_shells.len())
            .field("detail", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct HostKeyPreviewData {
    #[prost(bytes = "vec", tag = "1")]
    pub profile_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub host: String,
    #[prost(uint32, tag = "3")]
    pub port: u32,
    #[prost(string, tag = "4")]
    pub algorithm: String,
    #[prost(string, tag = "5")]
    pub fingerprint: String,
    #[prost(string, tag = "6")]
    pub public_key_line: String,
    #[prost(bytes = "vec", tag = "7")]
    pub token: Vec<u8>,
    #[prost(uint64, tag = "8")]
    pub expires_unix_seconds: u64,
}

impl std::fmt::Debug for HostKeyPreviewData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostKeyPreviewData")
            .field("port", &self.port)
            .field("expires_unix_seconds", &self.expires_unix_seconds)
            .field("token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Drop for HostKeyPreviewData {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileStatus {
    Ok = 0,
    Invalid = 1,
    Conflict = 2,
    Unavailable = 3,
    Corrupt = 4,
    Unsupported = 5,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileCatalogData {
    #[prost(uint64, tag = "1")]
    pub revision: u64,
    #[prost(message, optional, tag = "2")]
    pub defaults: Option<ProfileDefaultsData>,
    #[prost(message, repeated, tag = "3")]
    pub folders: Vec<ProfileFolderData>,
    #[prost(message, repeated, tag = "4")]
    pub profiles: Vec<ProfileRecordData>,
    #[prost(message, repeated, tag = "5")]
    pub ssh_connections: Vec<SshConnectionData>,
    #[prost(message, repeated, tag = "6")]
    pub local_connections: Vec<LocalConnectionData>,
    #[prost(message, repeated, tag = "7")]
    pub themes: Vec<ThemeData>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ThemeData {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(uint32, tag = "2")]
    pub foreground: u32,
    #[prost(uint32, tag = "3")]
    pub background: u32,
    #[prost(uint32, tag = "4")]
    pub cursor: u32,
    #[prost(uint32, tag = "5")]
    pub selection: u32,
    #[prost(uint32, repeated, tag = "6")]
    pub ansi: Vec<u32>,
}

impl From<&cshell_domain::ThemeRecord> for ThemeData {
    fn from(theme: &cshell_domain::ThemeRecord) -> Self {
        let pack = |[r, g, b]: [u8; 3]| (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        Self {
            name: theme.name.clone(),
            foreground: pack(theme.palette.foreground),
            background: pack(theme.palette.background),
            cursor: pack(theme.palette.cursor),
            selection: pack(theme.palette.selection),
            ansi: theme.palette.ansi.iter().copied().map(pack).collect(),
        }
    }
}

impl TryFrom<ThemeData> for cshell_domain::ThemeRecord {
    type Error = ProfileCodecError;
    fn try_from(theme: ThemeData) -> Result<Self, Self::Error> {
        if theme.ansi.len() != 16 {
            return Err(ProfileCodecError::InvalidTheme);
        }
        let unpack = |color: u32| -> Result<[u8; 3], ProfileCodecError> {
            if color > 0x00ff_ffff {
                return Err(ProfileCodecError::InvalidTheme);
            }
            Ok([(color >> 16) as u8, (color >> 8) as u8, color as u8])
        };
        let ansi = theme
            .ansi
            .into_iter()
            .map(unpack)
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|_| ProfileCodecError::InvalidTheme)?;
        Ok(Self {
            name: theme.name,
            palette: cshell_domain::TerminalPalette {
                foreground: unpack(theme.foreground)?,
                background: unpack(theme.background)?,
                cursor: unpack(theme.cursor)?,
                selection: unpack(theme.selection)?,
                ansi,
            },
        })
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileDefaultsData {
    #[prost(string, tag = "1")]
    pub terminal_type: String,
    #[prost(string, tag = "2")]
    pub theme: String,
    #[prost(bool, tag = "3")]
    pub logging: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileTerminalOverridesData {
    #[prost(string, optional, tag = "1")]
    pub terminal_type: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub theme: Option<String>,
    #[prost(bool, optional, tag = "3")]
    pub logging: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileFolderData {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub parent_id: Option<Vec<u8>>,
    #[prost(message, optional, tag = "4")]
    pub terminal: Option<ProfileTerminalOverridesData>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileRecordData {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(enumeration = "ProfileKindData", tag = "3")]
    pub kind: i32,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub folder_id: Option<Vec<u8>>,
    #[prost(string, repeated, tag = "5")]
    pub tags: Vec<String>,
    #[prost(bool, tag = "6")]
    pub favorite: bool,
    #[prost(message, optional, tag = "7")]
    pub terminal: Option<ProfileTerminalOverridesData>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LocalShellData {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub program: String,
    #[prost(string, repeated, tag = "3")]
    pub args: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LocalConnectionData {
    #[prost(bytes = "vec", tag = "1")]
    pub profile_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub program: String,
    #[prost(string, repeated, tag = "3")]
    pub args: Vec<String>,
    #[prost(uint32, tag = "4")]
    pub cwd_kind: u32,
    #[prost(string, tag = "5")]
    pub cwd_path: String,
    #[prost(btree_map = "string, string", tag = "6")]
    pub env_overrides: BTreeMap<String, String>,
    #[prost(uint32, tag = "7")]
    pub close_policy: u32,
}

impl From<&LocalConnectionRecord> for LocalConnectionData {
    fn from(value: &LocalConnectionRecord) -> Self {
        let (cwd_kind, cwd_path) = match &value.cwd {
            LocalWorkingDirectory::Inherit => (0, String::new()),
            LocalWorkingDirectory::Home => (1, String::new()),
            LocalWorkingDirectory::Explicit { path } => (2, path.clone()),
        };
        Self {
            profile_id: value.profile_id.as_uuid().as_bytes().to_vec(),
            program: value.program.clone(),
            args: value.args.clone(),
            cwd_kind,
            cwd_path,
            env_overrides: value.env_overrides.clone(),
            close_policy: match value.close_policy {
                cshell_domain::LocalClosePolicy::KeepAlive => 0,
                cshell_domain::LocalClosePolicy::TerminateOnViewClose => 1,
            },
        }
    }
}

impl TryFrom<LocalConnectionData> for LocalConnectionRecord {
    type Error = ProfileCodecError;
    fn try_from(value: LocalConnectionData) -> Result<Self, Self::Error> {
        let cwd = match (value.cwd_kind, value.cwd_path.as_str()) {
            (0, "") => LocalWorkingDirectory::Inherit,
            (1, "") => LocalWorkingDirectory::Home,
            (2, path) if !path.is_empty() => LocalWorkingDirectory::Explicit { path: path.into() },
            _ => return Err(ProfileCodecError::InvalidLocalDirectory),
        };
        Ok(Self {
            profile_id: ProfileId::from_bytes(decode_id(&value.profile_id)?),
            program: value.program,
            args: value.args,
            cwd,
            env_overrides: value.env_overrides,
            close_policy: match value.close_policy {
                0 => cshell_domain::LocalClosePolicy::KeepAlive,
                1 => cshell_domain::LocalClosePolicy::TerminateOnViewClose,
                _ => return Err(ProfileCodecError::InvalidLocalClosePolicy),
            },
        })
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct SshConnectionData {
    #[prost(bytes = "vec", tag = "1")]
    pub profile_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub host: String,
    #[prost(uint32, tag = "3")]
    pub port: u32,
    #[prost(string, tag = "4")]
    pub username: String,
    #[prost(enumeration = "SshAuthMethodData", tag = "5")]
    pub auth_method: i32,
    #[prost(string, optional, tag = "6")]
    pub private_key_path: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub certificate_path: Option<String>,
    #[prost(enumeration = "SshAgentBackendData", tag = "8")]
    pub agent_backend: i32,
    #[prost(string, optional, tag = "9")]
    pub agent_identity: Option<String>,
    #[prost(message, optional, tag = "10")]
    pub route: Option<SshRouteData>,
    #[prost(message, optional, tag = "11")]
    pub options: Option<SshConnectionOptionsData>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SshConnectionOptionsData {
    #[prost(uint32, optional, tag = "10")]
    pub sftp_verification: Option<u32>,
    #[prost(uint32,repeated,tag="9")]
    pub authentication_order: Vec<u32>,
    #[prost(bool, tag = "8")]
    pub keyboard_interactive: bool,
    #[prost(string, optional, tag = "7")]
    pub zmodem_download_directory: Option<String>,
    #[prost(bool, tag = "1")]
    pub keepalive_enabled: bool,
    #[prost(uint32, tag = "2")]
    pub keepalive_interval_secs: u32,
    #[prost(uint32, tag = "3")]
    pub keepalive_max: u32,
    #[prost(bool, tag = "4")]
    pub auto_reconnect: bool,
    #[prost(uint32, tag = "5")]
    pub reconnect_delay_secs: u32,
    #[prost(uint32, tag = "6")]
    pub reconnect_attempts: u32,
}

impl From<&cshell_domain::SshConnectionOptions> for SshConnectionOptionsData {
    fn from(value: &cshell_domain::SshConnectionOptions) -> Self {
        Self {
            sftp_verification: value.sftp_verification.map(|mode|match mode {cshell_domain::SftpVerification::Standard=>0,cshell_domain::SftpVerification::Full=>1}),
            authentication_order: value.authentication_order.iter().map(|m|*m as u32).collect(),
            keyboard_interactive: value.keyboard_interactive,
            zmodem_download_directory: value.zmodem_download_directory.clone(),
            keepalive_enabled: value.keepalive_enabled,
            keepalive_interval_secs: value.keepalive_interval_secs,
            keepalive_max: value.keepalive_max,
            auto_reconnect: value.auto_reconnect,
            reconnect_delay_secs: value.reconnect_delay_secs,
            reconnect_attempts: value.reconnect_attempts,
        }
    }
}

impl From<SshConnectionOptionsData> for cshell_domain::SshConnectionOptions {
    fn from(value: SshConnectionOptionsData) -> Self {
        Self {
            sftp_verification: value.sftp_verification.and_then(|mode|match mode {0=>Some(cshell_domain::SftpVerification::Standard),1=>Some(cshell_domain::SftpVerification::Full),_=>None}),
            authentication_order: value.authentication_order.iter().filter_map(|m|match m {0=>Some(cshell_domain::SshAuthStep::KeyboardInteractive),1=>Some(cshell_domain::SshAuthStep::Password),2=>Some(cshell_domain::SshAuthStep::PrivateKey),3=>Some(cshell_domain::SshAuthStep::Certificate),4=>Some(cshell_domain::SshAuthStep::Agent),_=>None}).collect(),
            keyboard_interactive: value.keyboard_interactive,
            zmodem_download_directory: value.zmodem_download_directory,
            keepalive_enabled: value.keepalive_enabled,
            keepalive_interval_secs: value.keepalive_interval_secs,
            keepalive_max: value.keepalive_max,
            auto_reconnect: value.auto_reconnect,
            reconnect_delay_secs: value.reconnect_delay_secs,
            reconnect_attempts: value.reconnect_attempts,
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct SshRouteData {
    #[prost(uint32, tag = "1")]
    pub kind: u32,
    #[prost(string, tag = "2")]
    pub host: String,
    #[prost(uint32, tag = "3")]
    pub port: u32,
    #[prost(bytes = "vec", tag = "4")]
    pub jump_profile_id: Vec<u8>,
}

impl From<&cshell_domain::SshRoute> for SshRouteData {
    fn from(route: &cshell_domain::SshRoute) -> Self {
        match route {
            cshell_domain::SshRoute::Direct => Self::default(),
            cshell_domain::SshRoute::Socks5 { host, port } => Self {
                kind: 1,
                host: host.clone(),
                port: (*port).into(),
                jump_profile_id: Vec::new(),
            },
            cshell_domain::SshRoute::HttpConnect { host, port } => Self {
                kind: 2,
                host: host.clone(),
                port: (*port).into(),
                jump_profile_id: Vec::new(),
            },
            cshell_domain::SshRoute::Jump { profile_id } => Self {
                kind: 3,
                jump_profile_id: profile_id.as_uuid().as_bytes().to_vec(),
                ..Self::default()
            },
        }
    }
}

impl TryFrom<SshRouteData> for cshell_domain::SshRoute {
    type Error = ProfileCodecError;
    fn try_from(value: SshRouteData) -> Result<Self, Self::Error> {
        match value.kind {
            0 if value.host.is_empty() && value.port == 0 && value.jump_profile_id.is_empty() => {
                Ok(Self::Direct)
            }
            1 | 2 if value.jump_profile_id.is_empty() => {
                let port = u16::try_from(value.port)
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or(ProfileCodecError::InvalidPort)?;
                Ok(if value.kind == 1 {
                    Self::Socks5 {
                        host: value.host,
                        port,
                    }
                } else {
                    Self::HttpConnect {
                        host: value.host,
                        port,
                    }
                })
            }
            3 if value.host.is_empty() && value.port == 0 => Ok(Self::Jump {
                profile_id: ProfileId::from_bytes(decode_id(&value.jump_profile_id)?),
            }),
            _ => Err(ProfileCodecError::InvalidRoute),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SshAuthMethodData {
    Password = 0,
    PrivateKey = 1,
    Certificate = 2,
    Agent = 3,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum SshAgentBackendData {
    Auto = 0,
    OpenSsh = 1,
    Pageant = 2,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileKindData {
    Ssh = 0,
    Local = 1,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileChange {
    #[prost(oneof = "profile_change::Change", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub change: Option<profile_change::Change>,
}

pub mod profile_change {
    use super::{LocalConnectionData, ProfileFolderData, ProfileRecordData, SshConnectionData};
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Change {
        #[prost(message, tag = "1")]
        UpsertFolder(ProfileFolderData),
        #[prost(bytes, tag = "2")]
        RemoveFolder(Vec<u8>),
        #[prost(message, tag = "3")]
        UpsertProfile(ProfileRecordData),
        #[prost(bytes, tag = "4")]
        RemoveProfile(Vec<u8>),
        #[prost(message, tag = "5")]
        UpsertSshConnection(SshConnectionData),
        #[prost(bytes, tag = "6")]
        RemoveSshConnection(Vec<u8>),
        #[prost(message, tag = "7")]
        UpsertLocalConnection(LocalConnectionData),
        #[prost(bytes, tag = "8")]
        RemoveLocalConnection(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileImportPreviewData {
    #[prost(uint64, tag = "1")]
    pub base_revision: u64,
    #[prost(message, repeated, tag = "2")]
    pub items: Vec<ProfileImportItemData>,
    #[prost(bool, tag = "3")]
    pub can_commit: bool,
    #[prost(uint32, tag = "4")]
    pub change_count: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfileImportItemData {
    #[prost(enumeration = "ProfileImportItemKind", tag = "1")]
    pub kind: i32,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(enumeration = "ProfileImportAction", tag = "3")]
    pub action: i32,
    #[prost(string, tag = "4")]
    pub details: String,
    #[prost(message, optional, tag = "5")]
    pub theme: Option<ThemeData>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileImportItemKind {
    Folder = 0,
    Profile = 1,
    Theme = 2,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileImportAction {
    Create = 0,
    Skip = 1,
    Replace = 2,
    Conflict = 3,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProfileCodecError {
    #[error("Theme colors require 24-bit RGB and exactly 16 ANSI entries")]
    InvalidTheme,
    #[error("invalid local working directory policy")]
    InvalidLocalDirectory,
    #[error("unknown local close policy")]
    InvalidLocalClosePolicy,
    #[error("Profile identifier must contain 16 bytes")]
    InvalidId,
    #[error("Profile kind is unknown")]
    InvalidKind,
    #[error("SSH port is invalid")]
    InvalidPort,
    #[error("SSH authentication method is unknown")]
    InvalidAuthMethod,
    #[error("invalid SSH route")]
    InvalidRoute,
    #[error("SSH agent backend is unknown")]
    InvalidAgentBackend,
    #[error("Profile change is missing")]
    MissingChange,
    #[error("Profile request contains too many changes")]
    TooManyChanges,
    #[error("Profile response is missing defaults")]
    MissingDefaults,
}

impl From<&TerminalDefaults> for ProfileDefaultsData {
    fn from(value: &TerminalDefaults) -> Self {
        Self {
            terminal_type: value.terminal_type.clone(),
            theme: value.theme.clone(),
            logging: value.logging,
        }
    }
}

impl From<ProfileDefaultsData> for TerminalDefaults {
    fn from(value: ProfileDefaultsData) -> Self {
        Self {
            terminal_type: value.terminal_type,
            theme: value.theme,
            logging: value.logging,
        }
    }
}

impl From<&TerminalOverrides> for ProfileTerminalOverridesData {
    fn from(value: &TerminalOverrides) -> Self {
        Self {
            terminal_type: value.terminal_type.clone(),
            theme: value.theme.clone(),
            logging: value.logging,
        }
    }
}

fn decode_overrides(value: Option<ProfileTerminalOverridesData>) -> TerminalOverrides {
    value.map_or_else(TerminalOverrides::default, |value| TerminalOverrides {
        terminal_type: value.terminal_type,
        theme: value.theme,
        logging: value.logging,
    })
}

impl From<&ProfileFolder> for ProfileFolderData {
    fn from(value: &ProfileFolder) -> Self {
        Self {
            id: value.id.as_uuid().as_bytes().to_vec(),
            name: value.name.clone(),
            parent_id: value.parent_id.map(|id| id.as_uuid().as_bytes().to_vec()),
            terminal: Some((&value.terminal).into()),
        }
    }
}

impl TryFrom<ProfileFolderData> for ProfileFolder {
    type Error = ProfileCodecError;
    fn try_from(value: ProfileFolderData) -> Result<Self, Self::Error> {
        Ok(Self {
            id: FolderId::from_bytes(decode_id(&value.id)?),
            name: value.name,
            parent_id: value
                .parent_id
                .as_deref()
                .map(decode_id)
                .transpose()?
                .map(FolderId::from_bytes),
            terminal: decode_overrides(value.terminal),
        })
    }
}

impl From<&ProfileRecord> for ProfileRecordData {
    fn from(value: &ProfileRecord) -> Self {
        Self {
            id: value.id.as_uuid().as_bytes().to_vec(),
            name: value.name.clone(),
            kind: match value.kind {
                ProfileKind::Ssh => ProfileKindData::Ssh as i32,
                ProfileKind::Local => ProfileKindData::Local as i32,
            },
            folder_id: value.folder_id.map(|id| id.as_uuid().as_bytes().to_vec()),
            tags: value.tags.iter().cloned().collect(),
            favorite: value.favorite,
            terminal: Some((&value.terminal).into()),
        }
    }
}

impl TryFrom<ProfileRecordData> for ProfileRecord {
    type Error = ProfileCodecError;
    fn try_from(value: ProfileRecordData) -> Result<Self, Self::Error> {
        let kind = match ProfileKindData::try_from(value.kind)
            .map_err(|_| ProfileCodecError::InvalidKind)?
        {
            ProfileKindData::Ssh => ProfileKind::Ssh,
            ProfileKindData::Local => ProfileKind::Local,
        };
        Ok(Self {
            id: ProfileId::from_bytes(decode_id(&value.id)?),
            name: value.name,
            kind,
            folder_id: value
                .folder_id
                .as_deref()
                .map(decode_id)
                .transpose()?
                .map(FolderId::from_bytes),
            tags: value.tags.into_iter().collect::<BTreeSet<_>>(),
            favorite: value.favorite,
            terminal: decode_overrides(value.terminal),
        })
    }
}

impl From<&SshConnectionRecord> for SshConnectionData {
    fn from(value: &SshConnectionRecord) -> Self {
        Self {
            profile_id: value.profile_id.as_uuid().as_bytes().to_vec(),
            host: value.host.clone(),
            port: u32::from(value.port),
            username: value.username.clone(),
            auth_method: match value.auth_method {
                SshAuthMethod::Password => SshAuthMethodData::Password,
                SshAuthMethod::PrivateKey => SshAuthMethodData::PrivateKey,
                SshAuthMethod::Certificate => SshAuthMethodData::Certificate,
                SshAuthMethod::Agent => SshAuthMethodData::Agent,
            } as i32,
            private_key_path: value.private_key_path.clone(),
            certificate_path: value.certificate_path.clone(),
            agent_backend: match value.agent_backend {
                SshAgentBackend::Auto => SshAgentBackendData::Auto,
                SshAgentBackend::OpenSsh => SshAgentBackendData::OpenSsh,
                SshAgentBackend::Pageant => SshAgentBackendData::Pageant,
            } as i32,
            agent_identity: value.agent_identity.clone(),
            route: Some(SshRouteData::from(&value.route)),
            options: Some(SshConnectionOptionsData::from(&value.options)),
        }
    }
}

impl TryFrom<SshConnectionData> for SshConnectionRecord {
    type Error = ProfileCodecError;
    fn try_from(value: SshConnectionData) -> Result<Self, Self::Error> {
        if value.options.as_ref().is_some_and(|o|o.authentication_order.len()>5 || o.authentication_order.iter().any(|m|*m>4) || o.sftp_verification.is_some_and(|m|m>1)) {return Err(ProfileCodecError::InvalidAuthMethod);}

        Ok(Self {
            profile_id: ProfileId::from_bytes(decode_id(&value.profile_id)?),
            options: value.options.map(Into::into).unwrap_or_default(),
            host: value.host,
            port: u16::try_from(value.port)
                .ok()
                .filter(|port| *port > 0)
                .ok_or(ProfileCodecError::InvalidPort)?,
            username: value.username,
            auth_method: match SshAuthMethodData::try_from(value.auth_method)
                .map_err(|_| ProfileCodecError::InvalidAuthMethod)?
            {
                SshAuthMethodData::Password => SshAuthMethod::Password,
                SshAuthMethodData::PrivateKey => SshAuthMethod::PrivateKey,
                SshAuthMethodData::Certificate => SshAuthMethod::Certificate,
                SshAuthMethodData::Agent => SshAuthMethod::Agent,
            },
            private_key_path: value.private_key_path,
            certificate_path: value.certificate_path,
            agent_backend: match SshAgentBackendData::try_from(value.agent_backend)
                .map_err(|_| ProfileCodecError::InvalidAgentBackend)?
            {
                SshAgentBackendData::Auto => SshAgentBackend::Auto,
                SshAgentBackendData::OpenSsh => SshAgentBackend::OpenSsh,
                SshAgentBackendData::Pageant => SshAgentBackend::Pageant,
            },
            agent_identity: value.agent_identity,
            route: value
                .route
                .map(cshell_domain::SshRoute::try_from)
                .transpose()?
                .unwrap_or_default(),
        })
    }
}

pub fn decode_id(bytes: &[u8]) -> Result<[u8; 16], ProfileCodecError> {
    bytes.try_into().map_err(|_| ProfileCodecError::InvalidId)
}

#[cfg(test)]
mod theme_tests {
    use super::*;
    #[test]
    fn ssh_connection_options_wire_round_trip_and_legacy_defaults() {
        let legacy = SshConnectionData {
            profile_id: ProfileId::new().as_uuid().as_bytes().to_vec(),
            host: "example.test".into(), port: 22, username: "operator".into(),
            ..Default::default()
        };
        let old = SshConnectionRecord::try_from(legacy.clone()).unwrap();
        assert_eq!(old.options, cshell_domain::SshConnectionOptions::default());
        assert!(!old.options.auto_reconnect);
        let desired = cshell_domain::SshConnectionOptions {
            sftp_verification: Some(cshell_domain::SftpVerification::Full),
            keyboard_interactive: true,
            zmodem_download_directory: Some("D:/downloads/profile".into()),
            keepalive_interval_secs: 43, keepalive_max: 5,
            reconnect_delay_secs: 9, reconnect_attempts: 7,
            ..cshell_domain::SshConnectionOptions::new_profile()
        };
        let configured = SshConnectionData { options: Some((&desired).into()), ..legacy };
        let decoded = SshConnectionData::decode(configured.encode_to_vec().as_slice()).unwrap();
        assert_eq!(SshConnectionRecord::try_from(decoded).unwrap().options, desired);
        let mut invalid=SshConnectionData::from(&old);invalid.options.as_mut().unwrap().sftp_verification=Some(2);
        assert!(SshConnectionRecord::try_from(invalid).is_err());
    }
    #[test]
    fn palette_ipc_roundtrip_rejects_invalid_channels_and_ansi_length()
    -> Result<(), ProfileCodecError> {
        let theme = cshell_domain::ThemeRecord {
            name: "paper".into(),
            palette: cshell_domain::TerminalPalette {
                foreground: [255, 128, 0],
                background: [1, 2, 3],
                cursor: [4, 5, 6],
                selection: [7, 8, 9],
                ansi: [[10, 11, 12]; 16],
            },
        };
        let wire = ThemeData::from(&theme);
        let decoded = ThemeData::decode(wire.encode_to_vec().as_slice())
            .map_err(|_| ProfileCodecError::InvalidTheme)?;
        assert_eq!(cshell_domain::ThemeRecord::try_from(decoded)?, theme);
        let mut invalid = wire.clone();
        invalid.ansi.pop();
        assert!(cshell_domain::ThemeRecord::try_from(invalid).is_err());
        let mut invalid = wire.clone();
        invalid.foreground = 0x0100_0000;
        assert!(cshell_domain::ThemeRecord::try_from(invalid).is_err());
        let mut invalid = wire;
        invalid.ansi[0] = u32::MAX;
        assert!(cshell_domain::ThemeRecord::try_from(invalid).is_err());
        Ok(())
    }
}
