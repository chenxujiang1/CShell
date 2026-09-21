//! Profile catalog metadata and terminal setting inheritance.
//! Connection details and secrets live in separate records and the vault.

use crate::{FolderId, ProfileId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProfileKind {
    Ssh,
    Local,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalOverrides {
    pub terminal_type: Option<String>,
    pub theme: Option<String>,
    pub logging: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
pub struct ProfileFolder {
    pub id: FolderId,
    pub name: String,
    pub parent_id: Option<FolderId>,
    pub terminal: TerminalOverrides,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileRecord {
    pub id: ProfileId,
    pub name: String,
    pub kind: ProfileKind,
    pub folder_id: Option<FolderId>,
    pub tags: BTreeSet<String>,
    pub favorite: bool,
    pub terminal: TerminalOverrides,
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
