//! Bounded Profile control messages shared by desktop and daemon.

use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, SshAgentBackend, SshAuthMethod,
    SshConnectionRecord, TerminalDefaults, TerminalOverrides,
};
use prost::{Enumeration, Message};
use std::collections::BTreeSet;
use thiserror::Error;

pub const MAX_PROFILE_CONTROL_CHANGES: usize = 2048;

#[derive(Clone, PartialEq, Message)]
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
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileImportPolicy {
    Fail = 0,
    Skip = 1,
    Replace = 2,
}

#[derive(Clone, PartialEq, Message)]
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
    #[prost(oneof = "profile_change::Change", tags = "1, 2, 3, 4, 5, 6")]
    pub change: Option<profile_change::Change>,
}

pub mod profile_change {
    use super::{ProfileFolderData, ProfileRecordData, SshConnectionData};
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
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ProfileImportItemKind {
    Folder = 0,
    Profile = 1,
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
    #[error("Profile identifier must contain 16 bytes")]
    InvalidId,
    #[error("Profile kind is unknown")]
    InvalidKind,
    #[error("SSH port is invalid")]
    InvalidPort,
    #[error("SSH authentication method is unknown")]
    InvalidAuthMethod,
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
        }
    }
}

impl TryFrom<SshConnectionData> for SshConnectionRecord {
    type Error = ProfileCodecError;
    fn try_from(value: SshConnectionData) -> Result<Self, Self::Error> {
        Ok(Self {
            profile_id: ProfileId::from_bytes(decode_id(&value.profile_id)?),
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
        })
    }
}

pub fn decode_id(bytes: &[u8]) -> Result<[u8; 16], ProfileCodecError> {
    bytes.try_into().map_err(|_| ProfileCodecError::InvalidId)
}
