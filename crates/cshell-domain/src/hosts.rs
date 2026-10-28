//! Host inventory references connection templates without storing credentials.
use crate::{HostGroupId, HostId, ProfileId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPlatformHint {
    #[default]
    Unknown,
    Posix,
    Linux,
    Macos,
    Windows,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRecord {
    pub id: HostId,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub profile_id: ProfileId,
    pub tags: BTreeSet<String>,
    pub labels: BTreeMap<String, String>,
    pub platform_hint: HostPlatformHint,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostGroupMembership {
    Static { hosts: BTreeSet<HostId> },
    Dynamic { selector: String },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostGroupRecord {
    pub id: HostGroupId,
    pub name: String,
    /// Organizational ancestry. Membership is always explicit.
    pub parent_id: Option<HostGroupId>,
    pub membership: HostGroupMembership,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCatalog {
    pub format: String,
    pub version: u32,
    pub hosts: Vec<HostRecord>,
    pub groups: Vec<HostGroupRecord>,
}
impl Default for HostCatalog {
    fn default() -> Self {
        Self {
            format: "cshell.host-catalog".into(),
            version: 1,
            hosts: Vec::new(),
            groups: Vec::new(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostSelection {
    Selector { expression: String },
    Group { id: HostGroupId },
    Explicit { hosts: BTreeSet<HostId> },
}
