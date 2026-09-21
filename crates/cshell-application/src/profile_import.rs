//! Versioned, secret-free Profile import with deterministic conflict decisions.

use crate::{
    CatalogChange, CatalogError, CatalogSnapshot, ProfileCatalog, ProfileRepository,
    ProfileService, ProfileServiceError,
};
use cshell_domain::{FolderId, ProfileFolder, ProfileRecord};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROFILE_IMPORT_FORMAT: &str = "cshell.profile-catalog";
pub const PROFILE_IMPORT_VERSION: u32 = 1;
pub const MAX_PROFILE_IMPORT_BYTES: usize = 1024 * 1024;
const MAX_IMPORT_ITEMS: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileImportDocument {
    pub format: String,
    pub version: u32,
    pub folders: Vec<ProfileFolder>,
    pub profiles: Vec<ProfileRecord>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileImportPreview {
    pub base_revision: u64,
    pub items: Vec<ImportItemPreview>,
    pub can_commit: bool,
    pub change_count: usize,
}

#[derive(Debug, Error)]
pub enum ProfileImportError {
    #[error("Profile import exceeds the 1 MiB limit")]
    TooLarge,
    #[error("Profile import document is not valid JSON: {0}")]
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

impl ProfileImportDocument {
    pub fn parse(bytes: &[u8]) -> Result<Self, ProfileImportError> {
        if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
            return Err(ProfileImportError::TooLarge);
        }
        let document: Self = serde_json::from_slice(bytes)?;
        if document.format != PROFILE_IMPORT_FORMAT || document.version != PROFILE_IMPORT_VERSION {
            return Err(ProfileImportError::UnsupportedFormat);
        }
        if document
            .folders
            .len()
            .saturating_add(document.profiles.len())
            > MAX_IMPORT_ITEMS
        {
            return Err(ProfileImportError::TooManyItems);
        }
        ProfileCatalog::from_snapshot(CatalogSnapshot {
            folders: document.folders.clone(),
            profiles: document.profiles.clone(),
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

fn plan_import(
    catalog: &ProfileCatalog,
    document: &ProfileImportDocument,
    policy: ImportConflictPolicy,
) -> Result<(Vec<CatalogChange>, ProfileImportPreview), ProfileImportError> {
    let mut working = catalog.snapshot();
    let mut changes = Vec::new();
    let mut items = Vec::with_capacity(document.folders.len() + document.profiles.len());
    let mut folder_map = std::collections::BTreeMap::<FolderId, FolderId>::new();
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
                    working.profiles[index] = profile.clone();
                    changes.push(CatalogChange::UpsertProfile(profile));
                    ImportAction::Replace
                }
            }
        } else {
            working.profiles.push(profile.clone());
            changes.push(CatalogChange::UpsertProfile(profile));
            ImportAction::Create
        };
        items.push(ImportItemPreview {
            kind: ImportItemKind::Profile,
            name: source.name.clone(),
            action,
        });
    }
    let can_commit = !items
        .iter()
        .any(|item| item.action == ImportAction::Conflict);
    if can_commit {
        catalog.preview_batch(&changes)?;
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
        };
        let mut encoded = serde_json::to_value(document)?;
        encoded["version"] = serde_json::json!(2);
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
