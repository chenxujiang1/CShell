//! Versioned, credential-free Profile import with deterministic conflict decisions.

use crate::{
    CatalogChange, CatalogError, CatalogSnapshot, ProfileCatalog, ProfileRepository,
    ProfileService, ProfileServiceError,
};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileRecord, SshConnectionRecord, SshRoute,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROFILE_IMPORT_FORMAT: &str = "cshell.profile-catalog";
pub const PROFILE_IMPORT_VERSION: u32 = 1;
pub const PROFILE_CONNECTION_IMPORT_VERSION: u32 = 2;
pub const PROFILE_TRANSFER_VERSION: u32 = 3;
pub const MAX_PROFILE_IMPORT_BYTES: usize = 1024 * 1024;
const MAX_IMPORT_ITEMS: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileImportDocument {
    pub format: String,
    pub version: u32,
    pub folders: Vec<ProfileFolder>,
    pub profiles: Vec<ProfileRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_connections: Vec<SshConnectionRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_connections: Vec<crate::LocalProfileTransfer>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportConflictPolicy {
    Fail,
    Skip,
    Replace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportItemKind {
    Folder,
    Profile,
    Theme,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportAction {
    Create,
    Skip,
    Replace,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportItemPreview {
    pub kind: ImportItemKind,
    pub name: String,
    pub action: ImportAction,
    pub details: String,
    pub theme: Option<cshell_domain::ThemeRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileImportPreview {
    pub base_revision: u64,
    pub items: Vec<ImportItemPreview>,
    pub can_commit: bool,
    pub change_count: usize,
}

#[derive(Error)]
pub enum ProfileImportError {
    #[error("Profile import exceeds the 1 MiB limit")]
    TooLarge,
    #[error("Profile import document is not valid JSON")]
    Json(#[from] serde_json::Error),
    #[error("unsupported Profile import format or version")]
    UnsupportedFormat,
    #[error("Profile import contains more than 2048 records")]
    TooManyItems,
    #[error("Profile import has unresolved conflicts")]
    Conflicts,
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Service(#[from] ProfileServiceError),
}

impl std::fmt::Debug for ProfileImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Serde errors can echo a malformed argument/environment value.
        std::fmt::Display::fmt(self, f)
    }
}

impl ProfileImportDocument {
    /// Canonical identity of the exact validated, credential-free document.
    pub fn fingerprint(&self) -> Result<[u8; 32], ProfileImportError> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self)?;
        Self::parse(&bytes)?;
        Ok(Sha256::digest(bytes).into())
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, ProfileImportError> {
        if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
            return Err(ProfileImportError::TooLarge);
        }
        let document: Self = serde_json::from_slice(bytes)?;
        if document.format != PROFILE_IMPORT_FORMAT
            || !matches!(
                document.version,
                PROFILE_IMPORT_VERSION
                    | PROFILE_CONNECTION_IMPORT_VERSION
                    | PROFILE_TRANSFER_VERSION
            )
            || (document.version == PROFILE_IMPORT_VERSION && !document.ssh_connections.is_empty())
            || (document.version < PROFILE_TRANSFER_VERSION
                && !document.local_connections.is_empty())
        {
            return Err(ProfileImportError::UnsupportedFormat);
        }
        if document
            .folders
            .len()
            .saturating_add(document.profiles.len())
            .saturating_add(document.ssh_connections.len())
            .saturating_add(document.local_connections.len())
            > MAX_IMPORT_ITEMS
        {
            return Err(ProfileImportError::TooManyItems);
        }
        ProfileCatalog::from_snapshot(CatalogSnapshot {
            folders: document.folders.clone(),
            profiles: document.profiles.clone(),
            ssh_connections: document.ssh_connections.clone(),
            local_connections: document
                .local_connections
                .iter()
                .map(|record| record.record(None))
                .collect(),
            ..CatalogSnapshot::default()
        })?;
        Ok(document)
    }
}

impl<R: ProfileRepository> ProfileService<R> {
    pub async fn preview_import(
        &self,
        bytes: &[u8],
        policy: ImportConflictPolicy,
    ) -> Result<ProfileImportPreview, ProfileImportError> {
        let document = ProfileImportDocument::parse(bytes)?;
        let catalog = self.load().await?;
        let (_, preview) = plan_import(&catalog, &document, policy)?;
        Ok(preview)
    }

    pub async fn commit_import(
        &self,
        expected_revision: u64,
        bytes: &[u8],
        policy: ImportConflictPolicy,
    ) -> Result<u64, ProfileImportError> {
        let document = ProfileImportDocument::parse(bytes)?;
        let catalog = self.load().await?;
        if catalog.snapshot().revision != expected_revision {
            return Err(CatalogError::StaleRevision {
                expected: expected_revision,
                actual: catalog.snapshot().revision,
            }
            .into());
        }
        let (changes, preview) = plan_import(&catalog, &document, policy)?;
        if !preview.can_commit {
            return Err(ProfileImportError::Conflicts);
        }
        self.apply_batch(expected_revision, &changes).await?;
        Ok(expected_revision + u64::from(!changes.is_empty()))
    }
}

/// Independently review a validated import against a captured catalog snapshot.
pub fn plan_profile_import(
    snapshot: &CatalogSnapshot,
    document: &ProfileImportDocument,
    policy: ImportConflictPolicy,
) -> Result<(Vec<CatalogChange>, ProfileImportPreview), ProfileImportError> {
    document.fingerprint()?;
    let catalog = ProfileCatalog::from_snapshot(snapshot.clone())?;
    plan_import(&catalog, document, policy)
}
fn ssh_import_details(
    target: &SshConnectionRecord,
    profiles: &[ProfileRecord],
    fallback_profiles: &[ProfileRecord],
    targets: &[SshConnectionRecord],
) -> String {
    let jump = match &target.route {
        SshRoute::Jump { .. } => cshell_domain::ssh_jump_chain(target, targets)
            .map(|chain| {
                format!(
                    "; jumps {}",
                    chain
                        .iter()
                        .map(|jump| {
                            let name = profiles
                                .iter()
                                .chain(fallback_profiles.iter())
                                .find(|p| p.id == jump.profile_id)
                                .map_or("SSH Profile", |p| p.name.as_str());
                            format!("{name} ({}@{}:{})", jump.username, jump.host, jump.port)
                        })
                        .collect::<Vec<_>>()
                        .join(" → ")
                )
            })
            .unwrap_or_else(|_| "; invalid jump route".into()),
        SshRoute::Direct => "; direct".into(),
        SshRoute::Socks5 { host, port } => format!("; SOCKS5 {host}:{port}"),
        SshRoute::HttpConnect { host, port } => format!("; HTTP CONNECT {host}:{port}"),
    };
    format!(
        "{}@{}:{}; {:?}{}{}{}",
        target.username,
        target.host,
        target.port,
        target.auth_method,
        target
            .private_key_path
            .as_ref()
            .map_or_else(String::new, |path| format!("; key {path}")),
        target
            .certificate_path
            .as_ref()
            .map_or_else(String::new, |path| format!("; certificate {path}")),
        jump
    )
}

pub(crate) fn plan_import(
    catalog: &ProfileCatalog,
    document: &ProfileImportDocument,
    policy: ImportConflictPolicy,
) -> Result<(Vec<CatalogChange>, ProfileImportPreview), ProfileImportError> {
    let mut working = catalog.snapshot();
    let mut changes = Vec::new();
    let mut items = Vec::with_capacity(document.folders.len() + document.profiles.len());
    let mut folder_map = std::collections::BTreeMap::<FolderId, FolderId>::new();
    let mut profile_map = std::collections::BTreeMap::<ProfileId, (ProfileId, ImportAction)>::new();
    let mut remaining: Vec<_> = document.folders.iter().collect();
    while !remaining.is_empty() {
        let Some(position) = remaining.iter().position(|folder| {
            folder
                .parent_id
                .is_none_or(|id| folder_map.contains_key(&id))
        }) else {
            return Err(CatalogError::FolderCycle.into());
        };
        let source = remaining.remove(position);
        let mut folder = source.clone();
        folder.parent_id = source.parent_id.map(|id| folder_map[&id]);
        let by_id = working.folders.iter().position(|old| old.id == folder.id);
        let by_name = working.folders.iter().position(|old| {
            old.parent_id == folder.parent_id
                && old.name.to_lowercase() == folder.name.to_lowercase()
        });
        let ambiguous = matches!((by_id, by_name), (Some(a), Some(b)) if a != b);
        let existing = by_id.or(by_name);
        let action = if ambiguous {
            ImportAction::Conflict
        } else if let Some(index) = existing {
            folder.id = working.folders[index].id;
            match policy {
                ImportConflictPolicy::Fail => ImportAction::Conflict,
                ImportConflictPolicy::Skip => ImportAction::Skip,
                ImportConflictPolicy::Replace => {
                    working.folders[index] = folder.clone();
                    changes.push(CatalogChange::UpsertFolder(folder.clone()));
                    ImportAction::Replace
                }
            }
        } else {
            working.folders.push(folder.clone());
            changes.push(CatalogChange::UpsertFolder(folder.clone()));
            ImportAction::Create
        };
        folder_map.insert(source.id, folder.id);
        items.push(ImportItemPreview {
            kind: ImportItemKind::Folder,
            name: source.name.clone(),
            action,
            details: String::new(),
            theme: None,
        });
    }
    for source in &document.profiles {
        let mut profile = source.clone();
        profile.folder_id = source.folder_id.map(|id| folder_map[&id]);
        let by_id = working.profiles.iter().position(|old| old.id == profile.id);
        let by_name = working.profiles.iter().position(|old| {
            old.folder_id == profile.folder_id
                && old.name.to_lowercase() == profile.name.to_lowercase()
        });
        let ambiguous = matches!((by_id, by_name), (Some(a), Some(b)) if a != b);
        let existing = by_id.or(by_name);
        let action = if ambiguous {
            ImportAction::Conflict
        } else if let Some(index) = existing {
            profile.id = working.profiles[index].id;
            match policy {
                ImportConflictPolicy::Fail => ImportAction::Conflict,
                ImportConflictPolicy::Skip => ImportAction::Skip,
                ImportConflictPolicy::Replace => {
                    if document.version >= PROFILE_CONNECTION_IMPORT_VERSION
                        && working.profiles[index].kind != profile.kind
                    {
                        if working
                            .ssh_connections
                            .iter()
                            .any(|target| target.profile_id == profile.id)
                        {
                            changes.push(CatalogChange::RemoveSshConnection(profile.id));
                        }
                        if working
                            .local_connections
                            .iter()
                            .any(|target| target.profile_id == profile.id)
                        {
                            changes.push(CatalogChange::RemoveLocalConnection(profile.id));
                        }
                    }
                    working.profiles[index] = profile.clone();
                    changes.push(CatalogChange::UpsertProfile(profile.clone()));
                    ImportAction::Replace
                }
            }
        } else {
            working.profiles.push(profile.clone());
            changes.push(CatalogChange::UpsertProfile(profile.clone()));
            ImportAction::Create
        };
        profile_map.insert(source.id, (profile.id, action));
        let targets = if action == ImportAction::Skip {
            &working.ssh_connections
        } else {
            &document.ssh_connections
        };
        let target_id = if action == ImportAction::Skip {
            profile.id
        } else {
            source.id
        };
        let mut details = targets
            .iter()
            .find(|target| target.profile_id == target_id)
            .map_or_else(String::new, |target| {
                ssh_import_details(target, &document.profiles, &working.profiles, targets)
            });
        if source.kind == cshell_domain::ProfileKind::Local {
            if action == ImportAction::Skip {
                if let Some(record) = working
                    .local_connections
                    .iter()
                    .find(|record| record.profile_id == profile.id)
                {
                    details = format!(
                        "Local {}; {:?}; {:?}; {} argument(s), {} environment value(s) retained",
                        record.program,
                        record.cwd,
                        record.close_policy,
                        record.args.len(),
                        record.env_overrides.len()
                    );
                }
            } else if let Some(record) = document
                .local_connections
                .iter()
                .find(|record| record.profile_id == source.id)
            {
                details = format!(
                    "Local {}; {:?}; {:?}; arguments {}; environment {}",
                    record.program,
                    record.cwd,
                    record.close_policy,
                    record.args.as_ref().map_or_else(
                        || "omitted: retain existing or use empty".into(),
                        |args| format!("{} value(s)", args.len())
                    ),
                    record.env_overrides.as_ref().map_or_else(
                        || "omitted: retain existing or use empty".into(),
                        |env| format!("{} value(s)", env.len())
                    )
                );
            }
        }
        items.push(ImportItemPreview {
            kind: ImportItemKind::Profile,
            name: source.name.clone(),
            action,
            details,
            theme: None,
        });
    }
    for source in &document.ssh_connections {
        let (id, action) = profile_map[&source.profile_id];
        if !matches!(action, ImportAction::Create | ImportAction::Replace) {
            continue;
        }
        let mut target = source.clone();
        target.profile_id = id;
        if let SshRoute::Jump { profile_id } = &mut target.route {
            *profile_id = profile_map[profile_id].0;
        }
        changes.push(CatalogChange::UpsertSshConnection(target));
    }
    for source in &document.local_connections {
        let (id, action) = profile_map[&source.profile_id];
        if !matches!(action, ImportAction::Create | ImportAction::Replace) {
            continue;
        }
        let mut target = source.record(
            working
                .local_connections
                .iter()
                .find(|target| target.profile_id == id),
        );
        target.profile_id = id;
        changes.push(CatalogChange::UpsertLocalConnection(target));
    }
    let can_commit = !items
        .iter()
        .any(|item| item.action == ImportAction::Conflict);
    if can_commit {
        // Describe the effective route, including retained profiles selected by Skip.
        // Formatting the source document would display endpoints never committed.
        let mut effective = catalog.clone();
        effective.apply_batch(catalog.snapshot_ref().revision, &changes)?;
        let effective = effective.snapshot_ref();
        for (source, item) in document.profiles.iter().zip(
            items
                .iter_mut()
                .filter(|item| item.kind == ImportItemKind::Profile),
        ) {
            let id = profile_map[&source.id].0;
            if let Some(target) = effective
                .ssh_connections
                .iter()
                .find(|t| t.profile_id == id)
            {
                item.details = ssh_import_details(
                    target,
                    &effective.profiles,
                    &[],
                    &effective.ssh_connections,
                );
            }
        }
    }
    let preview = ProfileImportPreview {
        base_revision: catalog.snapshot().revision,
        items,
        can_commit,
        change_count: changes.len(),
    };
    Ok((changes, preview))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cshell_domain::{ProfileId, ProfileKind, TerminalOverrides};
    use std::collections::BTreeSet;

    #[test]
    fn connection_import_replaces_local_kind_atomically_and_v1_rejects_connections()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut document = crate::import_openssh_config(
            b"Host srv\nUser ops\n",
            crate::OpenSshImportOptions::default(),
        )?;
        let id = ProfileId::new();
        let mut previous = document.profiles[0].clone();
        previous.id = id;
        previous.kind = ProfileKind::Local;
        let mut catalog = ProfileCatalog::from_snapshot(CatalogSnapshot {
            profiles: vec![previous],
            local_connections: vec![cshell_domain::LocalConnectionRecord {
                profile_id: id,
                program: "shell".into(),
                args: Vec::new(),
                cwd: Default::default(),
                env_overrides: Default::default(),
                close_policy: Default::default(),
            }],
            ..Default::default()
        })?;
        let (changes, preview) = plan_import(&catalog, &document, ImportConflictPolicy::Replace)?;
        assert_eq!(preview.change_count, 3);
        catalog.apply_batch(0, &changes)?;
        let snapshot = catalog.snapshot();
        assert!(snapshot.local_connections.is_empty());
        assert_eq!(snapshot.profiles[0].id, id);
        assert_eq!(snapshot.ssh_connections[0].profile_id, id);
        document.version = PROFILE_IMPORT_VERSION;
        assert!(matches!(
            ProfileImportDocument::parse(&serde_json::to_vec(&document)?),
            Err(ProfileImportError::UnsupportedFormat)
        ));
        document.ssh_connections.clear();
        let local_catalog = ProfileCatalog::from_snapshot(CatalogSnapshot {
            profiles: vec![ProfileRecord {
                kind: ProfileKind::Local,
                ..snapshot.profiles[0].clone()
            }],
            local_connections: vec![cshell_domain::LocalConnectionRecord {
                profile_id: id,
                program: "shell".into(),
                args: Vec::new(),
                cwd: Default::default(),
                env_overrides: Default::default(),
                close_policy: Default::default(),
            }],
            ..Default::default()
        })?;
        assert!(plan_import(&local_catalog, &document, ImportConflictPolicy::Replace).is_err());
        Ok(())
    }

    #[test]
    fn import_rejects_unsupported_version_and_secret_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let document = ProfileImportDocument {
            format: PROFILE_IMPORT_FORMAT.into(),
            version: PROFILE_IMPORT_VERSION,
            folders: vec![],
            profiles: vec![ProfileRecord {
                id: ProfileId::new(),
                name: "web".into(),
                kind: ProfileKind::Ssh,
                folder_id: None,
                tags: BTreeSet::new(),
                favorite: false,
                terminal: TerminalOverrides::default(),
            }],
            ssh_connections: Vec::new(),
            local_connections: Vec::new(),
        };
        let mut encoded = serde_json::to_value(document)?;
        encoded["version"] = serde_json::json!(4);
        assert!(matches!(
            ProfileImportDocument::parse(&serde_json::to_vec(&encoded)?),
            Err(ProfileImportError::UnsupportedFormat)
        ));
        encoded["version"] = serde_json::json!(1);
        encoded["profiles"][0]["password"] = serde_json::json!("secret");
        assert!(matches!(
            ProfileImportDocument::parse(&serde_json::to_vec(&encoded)?),
            Err(ProfileImportError::Json(_))
        ));
        Ok(())
    }
}
