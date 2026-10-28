//! Host inventories, static/dynamic groups and immutable execution selections.
mod selector;
use crate::{CatalogSnapshot, ProfileCatalog};
use async_trait::async_trait;
use cshell_domain::*;
pub use selector::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::Write,
};
use thiserror::Error;

pub const MAX_HOSTS: usize = 10_000;
pub const MAX_HOST_GROUPS: usize = 256;
pub const MAX_HOST_TARGETS: usize = 1_000;
pub const MAX_HOST_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_GROUP_DEPTH: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCatalogSnapshot {
    pub revision: u64,
    pub catalog: HostCatalog,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostCatalogChange {
    UpsertHost(HostRecord),
    RemoveHost(HostId),
    UpsertGroup(HostGroupRecord),
    RemoveGroup(HostGroupId),
}
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HostCatalogError {
    #[error("unsupported host document format or version")]
    Unsupported,
    #[error("host document exceeds its limits")]
    Limit,
    #[error("invalid host metadata, duplicate identities or group references")]
    Invalid,
    #[error("host group ancestry or membership contains a cycle")]
    Cycle,
    #[error("unknown host group")]
    UnknownGroup,
    #[error("unknown host")]
    UnknownHost,
    #[error("selected host references an unavailable SSH Profile")]
    MissingProfile,
    #[error("invalid saved SSH Profile configuration")]
    InvalidProfile,
    #[error("selector: {0}")]
    Selector(#[from] HostSelectorError),
    #[error("target selection is empty")]
    EmptyTargets,
    #[error("target count exceeds the explicit limit; no targets were truncated")]
    TooManyTargets,
}
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HostRepositoryError {
    #[error("host catalog revision changed: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("host storage is unavailable")]
    Unavailable,
    #[error("stored host catalog is corrupt")]
    Corrupt,
}
#[async_trait]
pub trait HostRepository: fmt::Debug + Send + Sync {
    async fn load_hosts(&self) -> Result<Option<HostCatalogSnapshot>, HostRepositoryError>;
    async fn save_hosts(
        &self,
        expected_revision: u64,
        catalog: &HostCatalog,
    ) -> Result<HostCatalogSnapshot, HostRepositoryError>;
}
fn label(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn valid_host(host: &HostRecord) -> bool {
    !host.id.as_uuid().is_nil()
        && !host.profile_id.as_uuid().is_nil()
        && label(&host.name, 128)
        && host.port > 0
        && label(&host.address, 255)
        && !host.address.starts_with('-')
        && !host.address.chars().any(char::is_whitespace)
        && host.tags.len() <= 64
        && host.tags.iter().all(|tag| label(tag, 64))
        && host.labels.len() <= 64
        && host
            .labels
            .iter()
            .all(|(key, value)| label(key, 64) && !key.contains('=') && label(value, 256))
}
struct SizeLimit {
    bytes: usize,
    overflow: bool,
}
impl Write for SizeLimit {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        if self.bytes > MAX_HOST_DOCUMENT_BYTES {
            self.overflow = true;
            return Err(std::io::Error::other("host document limit"));
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn check_host_document_size<T: Serialize>(value: &T) -> Result<(), HostCatalogError> {
    let mut limit = SizeLimit {
        bytes: 0,
        overflow: false,
    };
    match serde_json::to_writer(&mut limit, value) {
        Ok(()) => Ok(()),
        Err(_) if limit.overflow => Err(HostCatalogError::Limit),
        Err(_) => Err(HostCatalogError::Invalid),
    }
}
fn compiled_groups(
    catalog: &HostCatalog,
) -> Result<BTreeMap<HostGroupId, HostSelector>, HostCatalogError> {
    let mut selectors = BTreeMap::new();
    for group in &catalog.groups {
        if let HostGroupMembership::Dynamic { selector } = &group.membership {
            selectors.insert(group.id, HostSelector::parse(selector)?);
        }
    }
    Ok(selectors)
}
pub fn validate_hosts(catalog: &HostCatalog) -> Result<(), HostCatalogError> {
    if catalog.format != "cshell.host-catalog" || catalog.version != 1 {
        return Err(HostCatalogError::Unsupported);
    }
    if catalog.hosts.len() > MAX_HOSTS || catalog.groups.len() > MAX_HOST_GROUPS {
        return Err(HostCatalogError::Limit);
    }
    let mut hosts = BTreeSet::new();
    let mut endpoints = BTreeSet::new();
    for host in &catalog.hosts {
        if !valid_host(host)
            || !hosts.insert(host.id)
            || !endpoints.insert((
                host.profile_id,
                host.address.to_ascii_lowercase(),
                host.port,
            ))
        {
            return Err(HostCatalogError::Invalid);
        }
    }
    let mut groups = BTreeMap::new();
    for group in &catalog.groups {
        if group.id.as_uuid().is_nil()
            || !label(&group.name, 128)
            || groups.insert(group.id, group).is_some()
        {
            return Err(HostCatalogError::Invalid);
        }
        if let HostGroupMembership::Static { hosts: members } = &group.membership
            && (members.len() > MAX_HOSTS || !members.is_subset(&hosts))
        {
            return Err(HostCatalogError::UnknownHost);
        }
    }
    for group in &catalog.groups {
        let mut next = Some(group.id);
        let mut visited = BTreeSet::new();
        while let Some(id) = next {
            if !visited.insert(id) {
                return Err(HostCatalogError::Cycle);
            }
            if visited.len() > MAX_GROUP_DEPTH {
                return Err(HostCatalogError::Limit);
            }
            next = groups
                .get(&id)
                .ok_or(HostCatalogError::UnknownGroup)?
                .parent_id;
        }
    }
    let selectors = compiled_groups(catalog)?;
    let mut complete = BTreeMap::new();
    for group in &catalog.groups {
        visit_group(
            group.id,
            &groups,
            &selectors,
            &mut BTreeSet::new(),
            &mut complete,
            0,
        )?;
    }
    check_host_document_size(catalog)
}
fn visit_group(
    id: HostGroupId,
    groups: &BTreeMap<HostGroupId, &HostGroupRecord>,
    selectors: &BTreeMap<HostGroupId, HostSelector>,
    active: &mut BTreeSet<HostGroupId>,
    complete: &mut BTreeMap<HostGroupId, usize>,
    depth: usize,
) -> Result<usize, HostCatalogError> {
    if depth > MAX_GROUP_DEPTH {
        return Err(HostCatalogError::Limit);
    }
    if !groups.contains_key(&id) {
        return Err(HostCatalogError::UnknownGroup);
    }
    if let Some(rank) = complete.get(&id) {
        return Ok(*rank);
    }
    if !active.insert(id) {
        return Err(HostCatalogError::Cycle);
    }
    let mut rank = 0;
    if let Some(selector) = selectors.get(&id) {
        for dependency in selector.group_references() {
            rank = rank
                .max(visit_group(dependency, groups, selectors, active, complete, depth + 1)? + 1);
        }
    }
    if rank > MAX_GROUP_DEPTH {
        return Err(HostCatalogError::Limit);
    }
    active.remove(&id);
    complete.insert(id, rank);
    Ok(rank)
}
pub fn validate_host_profiles(
    catalog: &HostCatalog,
    profiles: &CatalogSnapshot,
) -> Result<(), HostCatalogError> {
    ProfileCatalog::from_snapshot(profiles.clone())
        .map_err(|_| HostCatalogError::InvalidProfile)?;
    let ids: BTreeSet<_> = profiles
        .ssh_connections
        .iter()
        .map(|connection| connection.profile_id)
        .collect();
    if catalog
        .hosts
        .iter()
        .any(|host| !ids.contains(&host.profile_id))
    {
        return Err(HostCatalogError::MissingProfile);
    }
    Ok(())
}
pub fn encode_hosts(catalog: &HostCatalog) -> Result<Vec<u8>, HostCatalogError> {
    validate_hosts(catalog)?;
    serde_json::to_vec(catalog).map_err(|_| HostCatalogError::Invalid)
}
pub fn decode_hosts(bytes: &[u8]) -> Result<HostCatalog, HostCatalogError> {
    if bytes.len() > MAX_HOST_DOCUMENT_BYTES {
        return Err(HostCatalogError::Limit);
    }
    let catalog = serde_json::from_slice(bytes).map_err(|_| HostCatalogError::Invalid)?;
    validate_hosts(&catalog)?;
    Ok(catalog)
}
pub fn apply_host_change(
    catalog: &HostCatalog,
    change: HostCatalogChange,
) -> Result<HostCatalog, HostCatalogError> {
    let mut next = catalog.clone();
    match change {
        HostCatalogChange::UpsertHost(host) => {
            next.hosts.retain(|record| record.id != host.id);
            next.hosts.push(host);
        }
        HostCatalogChange::RemoveHost(id) => next.hosts.retain(|host| host.id != id),
        HostCatalogChange::UpsertGroup(group) => {
            next.groups.retain(|record| record.id != group.id);
            next.groups.push(group);
        }
        HostCatalogChange::RemoveGroup(id) => next.groups.retain(|group| group.id != id),
    }
    validate_hosts(&next)?;
    Ok(next)
}
fn memberships(
    catalog: &HostCatalog,
    selectors: &BTreeMap<HostGroupId, HostSelector>,
) -> Result<BTreeMap<HostGroupId, BTreeSet<HostId>>, HostCatalogError> {
    let mut result = BTreeMap::new();
    let mut waiting = BTreeMap::new();
    for group in &catalog.groups {
        match &group.membership {
            HostGroupMembership::Static { hosts } => {
                result.insert(group.id, hosts.clone());
            }
            HostGroupMembership::Dynamic { .. } => {
                waiting.insert(
                    group.id,
                    selectors.get(&group.id).ok_or(HostCatalogError::Invalid)?,
                );
            }
        }
    }
    while !waiting.is_empty() {
        let ready: Vec<_> = waiting
            .iter()
            .filter(|(_, selector)| {
                selector
                    .group_references()
                    .iter()
                    .all(|id| result.contains_key(id))
            })
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            return Err(HostCatalogError::Cycle);
        }
        for id in ready {
            let selector = waiting.remove(&id).ok_or(HostCatalogError::Invalid)?;
            let selected = catalog
                .hosts
                .iter()
                .filter(|host| selector.matches(host, &result))
                .map(|host| host.id)
                .collect();
            result.insert(id, selected);
        }
    }
    Ok(result)
}
pub fn select_hosts(
    catalog: &HostCatalog,
    selection: &HostSelection,
    limit: usize,
) -> Result<Vec<HostRecord>, HostCatalogError> {
    if limit == 0 || limit > MAX_HOST_TARGETS {
        return Err(HostCatalogError::Limit);
    }
    validate_hosts(catalog)?;
    let selectors = compiled_groups(catalog)?;
    let groups = memberships(catalog, &selectors)?;
    let selected = match selection {
        HostSelection::Group { id } => groups
            .get(id)
            .cloned()
            .ok_or(HostCatalogError::UnknownGroup)?,
        HostSelection::Explicit { hosts } => {
            if hosts
                .iter()
                .any(|id| !catalog.hosts.iter().any(|host| host.id == *id))
            {
                return Err(HostCatalogError::UnknownHost);
            }
            hosts.clone()
        }
        HostSelection::Selector { expression } => {
            let selector = HostSelector::parse(expression)?;
            if selector
                .group_references()
                .iter()
                .any(|id| !groups.contains_key(id))
            {
                return Err(HostCatalogError::UnknownGroup);
            }
            catalog
                .hosts
                .iter()
                .filter(|host| selector.matches(host, &groups))
                .map(|host| host.id)
                .collect()
        }
    };
    if selected.len() > limit {
        return Err(HostCatalogError::TooManyTargets);
    }
    let mut result: Vec<_> = catalog
        .hosts
        .iter()
        .filter(|host| selected.contains(&host.id))
        .cloned()
        .collect();
    result.sort_by_key(|host| host.id);
    Ok(result)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfileHint {
    pub profile_id: ProfileId,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub tags: BTreeSet<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostManagementSnapshot {
    pub revision: u64,
    pub profile_revision: u64,
    pub catalog: HostCatalog,
    pub profiles: Vec<HostProfileHint>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSelectionPreview {
    pub host_revision: u64,
    pub profile_revision: u64,
    pub selection: HostSelection,
    pub hosts: Vec<HostRecord>,
}
pub const MAX_HOST_SELECTION_BYTES: usize = 64 * 1024;
pub fn encode_host_selection(selection: &HostSelection) -> Result<Vec<u8>, HostCatalogError> {
    match selection {
        HostSelection::Selector { expression } => {
            HostSelector::parse(expression)?;
        }
        HostSelection::Explicit { hosts } if hosts.len() > MAX_HOST_TARGETS => {
            return Err(HostCatalogError::TooManyTargets);
        }
        HostSelection::Group { id } if id.as_uuid().is_nil() => {
            return Err(HostCatalogError::UnknownGroup);
        }
        _ => {}
    }
    let bytes = serde_json::to_vec(selection).map_err(|_| HostCatalogError::Invalid)?;
    if bytes.len() > MAX_HOST_SELECTION_BYTES {
        return Err(HostCatalogError::Limit);
    }
    Ok(bytes)
}
pub fn decode_host_selection(bytes: &[u8]) -> Result<HostSelection, HostCatalogError> {
    if bytes.len() > MAX_HOST_SELECTION_BYTES {
        return Err(HostCatalogError::Limit);
    }
    let selection = serde_json::from_slice(bytes).map_err(|_| HostCatalogError::Invalid)?;
    encode_host_selection(&selection)?;
    Ok(selection)
}
pub fn encode_host_management(view: &HostManagementSnapshot) -> Result<Vec<u8>, HostCatalogError> {
    validate_hosts(&view.catalog)?;
    if view.profiles.len() > MAX_HOSTS {
        return Err(HostCatalogError::Limit);
    }
    let mut ids = BTreeSet::new();
    for profile in &view.profiles {
        if profile.profile_id.as_uuid().is_nil()
            || !ids.insert(profile.profile_id)
            || !label(&profile.name, 128)
            || !label(&profile.address, 255)
            || profile.address.chars().any(char::is_whitespace)
            || profile.port == 0
            || profile.tags.len() > 64
            || profile.tags.iter().any(|tag| !label(tag, 64))
        {
            return Err(HostCatalogError::Invalid);
        }
    }
    check_host_document_size(view)?;
    serde_json::to_vec(view).map_err(|_| HostCatalogError::Invalid)
}
pub fn decode_host_management(bytes: &[u8]) -> Result<HostManagementSnapshot, HostCatalogError> {
    if bytes.len() > MAX_HOST_DOCUMENT_BYTES {
        return Err(HostCatalogError::Limit);
    }
    let view = serde_json::from_slice(bytes).map_err(|_| HostCatalogError::Invalid)?;
    encode_host_management(&view)?;
    Ok(view)
}
pub fn encode_host_preview(preview: &HostSelectionPreview) -> Result<Vec<u8>, HostCatalogError> {
    if preview.hosts.len() > MAX_HOST_TARGETS {
        return Err(HostCatalogError::TooManyTargets);
    }
    validate_hosts(&HostCatalog {
        hosts: preview.hosts.clone(),
        ..Default::default()
    })?;
    encode_host_selection(&preview.selection)?;
    check_host_document_size(preview)?;
    serde_json::to_vec(preview).map_err(|_| HostCatalogError::Invalid)
}
pub fn decode_host_preview(bytes: &[u8]) -> Result<HostSelectionPreview, HostCatalogError> {
    if bytes.len() > MAX_HOST_DOCUMENT_BYTES {
        return Err(HostCatalogError::Limit);
    }
    let preview = serde_json::from_slice(bytes).map_err(|_| HostCatalogError::Invalid)?;
    encode_host_preview(&preview)?;
    Ok(preview)
}
pub fn host_profile_hints(profiles: &CatalogSnapshot) -> Vec<HostProfileHint> {
    profiles
        .profiles
        .iter()
        .filter_map(|profile| {
            let connection = profiles
                .ssh_connections
                .iter()
                .find(|connection| connection.profile_id == profile.id)?;
            Some(HostProfileHint {
                profile_id: profile.id,
                name: profile.name.clone(),
                address: connection.host.clone(),
                port: connection.port,
                tags: profile.tags.clone(),
            })
        })
        .collect()
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenData {
    format: String,
    version: u32,
    id: HostTargetSnapshotId,
    host_revision: u64,
    profile_revision: u64,
    selection: HostSelection,
    hosts: Vec<HostRecord>,
    connections: Vec<SshConnectionRecord>,
}
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FrozenHostTargets {
    data: FrozenData,
}
impl fmt::Debug for FrozenHostTargets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrozenHostTargets")
            .field("id", &self.data.id)
            .field("hosts", &self.data.hosts.len())
            .field("host_revision", &self.data.host_revision)
            .field("profile_revision", &self.data.profile_revision)
            .finish_non_exhaustive()
    }
}
impl<'de> Deserialize<'de> for FrozenHostTargets {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let data = FrozenData::deserialize(deserializer)?;
        validate_frozen(&data).map_err(serde::de::Error::custom)?;
        Ok(Self { data })
    }
}
impl FrozenHostTargets {
    #[must_use]
    pub const fn id(&self) -> HostTargetSnapshotId {
        self.data.id
    }
    #[must_use]
    pub const fn host_revision(&self) -> u64 {
        self.data.host_revision
    }
    #[must_use]
    pub const fn profile_revision(&self) -> u64 {
        self.data.profile_revision
    }
    #[must_use]
    pub fn targets(&self) -> &[HostRecord] {
        &self.data.hosts
    }
    #[must_use]
    pub fn connections(&self) -> &[SshConnectionRecord] {
        &self.data.connections
    }
    #[must_use]
    pub fn selection(&self) -> &HostSelection {
        &self.data.selection
    }
    /// Preserve the original metadata/configuration for an explicitly requested new run.
    pub fn subset(&self, hosts: &BTreeSet<HostId>) -> Result<Self, HostCatalogError> {
        if hosts.is_empty() {
            return Err(HostCatalogError::EmptyTargets);
        }
        let targets: Vec<_> = self
            .data
            .hosts
            .iter()
            .filter(|host| hosts.contains(&host.id))
            .cloned()
            .collect();
        if targets.len() != hosts.len() {
            return Err(HostCatalogError::UnknownHost);
        }
        let connections = self
            .data
            .connections
            .iter()
            .map(|c| (c.profile_id, c))
            .collect();
        let required = required_connections(&targets, &connections)?;
        let data = FrozenData {
            format: self.data.format.clone(),
            version: self.data.version,
            id: HostTargetSnapshotId::new(),
            host_revision: self.data.host_revision,
            profile_revision: self.data.profile_revision,
            selection: HostSelection::Explicit {
                hosts: hosts.clone(),
            },
            hosts: targets,
            connections: self
                .data
                .connections
                .iter()
                .filter(|c| required.contains(&c.profile_id))
                .cloned()
                .collect(),
        };
        validate_frozen(&data)?;
        Ok(Self { data })
    }
    /// The original saved Profile used to validate vault credential bindings.
    #[must_use]
    pub fn credential_template(&self, host: HostId) -> Option<&SshConnectionRecord> {
        let host = self.data.hosts.iter().find(|target| target.id == host)?;
        self.data
            .connections
            .iter()
            .find(|connection| connection.profile_id == host.profile_id)
    }
    /// Resolve destination overrides. Validate credentials against `credential_template`,
    /// and verify the host key against this actual destination when executing later.
    #[must_use]
    pub fn resolved_connection(&self, host: HostId) -> Option<SshConnectionRecord> {
        let mut connection = self.credential_template(host)?.clone();
        let destination = self.data.hosts.iter().find(|target| target.id == host)?;
        connection.host.clone_from(&destination.address);
        connection.port = destination.port;
        Some(connection)
    }
}
fn required_connections(
    hosts: &[HostRecord],
    connections: &BTreeMap<ProfileId, &SshConnectionRecord>,
) -> Result<BTreeSet<ProfileId>, HostCatalogError> {
    let mut required = BTreeSet::new();
    let mut pending: Vec<_> = hosts.iter().map(|host| host.profile_id).collect();
    while let Some(id) = pending.pop() {
        if !required.insert(id) {
            continue;
        }
        let connection = connections
            .get(&id)
            .ok_or(HostCatalogError::MissingProfile)?;
        if let SshRoute::Jump { profile_id } = connection.route {
            pending.push(profile_id);
        }
    }
    Ok(required)
}
fn validate_frozen(data: &FrozenData) -> Result<(), HostCatalogError> {
    if data.format != "cshell.host-targets" || data.version != 1 {
        return Err(HostCatalogError::Unsupported);
    }
    if data.id.as_uuid().is_nil() || data.hosts.is_empty() {
        return Err(HostCatalogError::EmptyTargets);
    }
    if data.hosts.len() > MAX_HOST_TARGETS || data.connections.len() > MAX_HOST_TARGETS * 9 {
        return Err(HostCatalogError::Limit);
    }
    if data.hosts.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return Err(HostCatalogError::Invalid);
    }
    let catalog = HostCatalog {
        hosts: data.hosts.clone(),
        ..Default::default()
    };
    validate_hosts(&catalog)?;
    match &data.selection {
        HostSelection::Selector { expression } => {
            HostSelector::parse(expression)?;
        }
        HostSelection::Group { id } if id.as_uuid().is_nil() => {
            return Err(HostCatalogError::UnknownGroup);
        }
        HostSelection::Explicit { hosts }
            if hosts != &data.hosts.iter().map(|host| host.id).collect() =>
        {
            return Err(HostCatalogError::Invalid);
        }
        _ => {}
    }
    let connections: BTreeMap<_, _> = data
        .connections
        .iter()
        .map(|connection| (connection.profile_id, connection))
        .collect();
    if connections.len() != data.connections.len() {
        return Err(HostCatalogError::InvalidProfile);
    }
    let required = required_connections(&data.hosts, &connections)?;
    if required.len() != connections.len() {
        return Err(HostCatalogError::InvalidProfile);
    }
    let profiles = CatalogSnapshot {
        profiles: data
            .connections
            .iter()
            .map(|connection| ProfileRecord {
                id: connection.profile_id,
                name: connection.profile_id.to_string(),
                kind: ProfileKind::Ssh,
                folder_id: None,
                tags: BTreeSet::new(),
                favorite: false,
                terminal: TerminalOverrides::default(),
            })
            .collect(),
        ssh_connections: data.connections.clone(),
        ..Default::default()
    };
    ProfileCatalog::from_snapshot(profiles).map_err(|_| HostCatalogError::InvalidProfile)?;
    check_host_document_size(data)
}
pub fn freeze_hosts(
    hosts: &HostCatalogSnapshot,
    profiles: &CatalogSnapshot,
    selection: HostSelection,
    limit: usize,
) -> Result<FrozenHostTargets, HostCatalogError> {
    ProfileCatalog::from_snapshot(profiles.clone())
        .map_err(|_| HostCatalogError::InvalidProfile)?;
    let targets = select_hosts(&hosts.catalog, &selection, limit)?;
    if targets.is_empty() {
        return Err(HostCatalogError::EmptyTargets);
    }
    let connections: BTreeMap<_, _> = profiles
        .ssh_connections
        .iter()
        .map(|connection| (connection.profile_id, connection))
        .collect();
    let required = required_connections(&targets, &connections)?;
    let data = FrozenData {
        format: "cshell.host-targets".into(),
        version: 1,
        id: HostTargetSnapshotId::new(),
        host_revision: hosts.revision,
        profile_revision: profiles.revision,
        selection,
        hosts: targets,
        connections: required
            .iter()
            .map(|id| (*connections[id]).clone())
            .collect(),
    };
    validate_frozen(&data)?;
    Ok(FrozenHostTargets { data })
}
pub fn encode_frozen_hosts(targets: &FrozenHostTargets) -> Result<Vec<u8>, HostCatalogError> {
    validate_frozen(&targets.data)?;
    serde_json::to_vec(targets).map_err(|_| HostCatalogError::Invalid)
}
pub fn decode_frozen_hosts(bytes: &[u8]) -> Result<FrozenHostTargets, HostCatalogError> {
    if bytes.len() > MAX_HOST_DOCUMENT_BYTES {
        return Err(HostCatalogError::Limit);
    }
    serde_json::from_slice(bytes).map_err(|_| HostCatalogError::Invalid)
}

#[cfg(test)]
mod tests;
