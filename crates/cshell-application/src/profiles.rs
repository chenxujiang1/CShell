//! Transactional, storage independent profile catalog use cases.

use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, ResolvedField,
    ResolvedTerminalSettings, SettingSource, TerminalDefaults, TerminalOverrides,
};
use std::collections::BTreeSet;
use thiserror::Error;

const MAX_NAME_BYTES: usize = 128;
const MAX_TAG_BYTES: usize = 64;
const MAX_TAGS: usize = 64;
const MAX_BATCH_CHANGES: usize = 2048;
const MAX_SEARCH_RESULTS: usize = 256;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogSnapshot {
    pub revision: u64,
    pub defaults: TerminalDefaults,
    pub folders: Vec<ProfileFolder>,
    pub profiles: Vec<ProfileRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogChange {
    UpsertFolder(ProfileFolder),
    RemoveFolder(FolderId),
    UpsertProfile(ProfileRecord),
    RemoveProfile(ProfileId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeOutcome {
    FolderCreated(FolderId),
    FolderUpdated(FolderId),
    FolderRemoved(FolderId),
    ProfileCreated(ProfileId),
    ProfileUpdated(ProfileId),
    ProfileRemoved(ProfileId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogPreview {
    pub base_revision: u64,
    pub outcomes: Vec<ChangeOutcome>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileQuery {
    pub text: String,
    pub folder_id: Option<FolderId>,
    pub tag: Option<String>,
    pub kind: Option<ProfileKind>,
    pub favorites_only: bool,
    pub limit: usize,
}

impl Default for ProfileQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            folder_id: None,
            tag: None,
            kind: None,
            favorites_only: false,
            limit: 100,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CatalogError {
    #[error("catalog revision changed: expected {expected}, actual {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("catalog revision is exhausted")]
    RevisionExhausted,
    #[error("unknown folder {0}")]
    UnknownFolder(FolderId),
    #[error("unknown profile {0}")]
    UnknownProfile(ProfileId),
    #[error("folder ancestry contains a cycle")]
    FolderCycle,
    #[error("name or terminal setting is empty, too long, or contains control characters")]
    InvalidText,
    #[error("sibling names must be unique without regard to case")]
    DuplicateName,
    #[error("tag is empty, too long, or contains control characters")]
    InvalidTag,
    #[error("too many tags")]
    TooManyTags,
    #[error("batch exceeds the change limit")]
    TooManyChanges,
    #[error("search limit must be between 1 and 256")]
    InvalidSearchLimit,
}

/// Persistence boundary. Implementations must commit the entire next snapshot in one
/// transaction and compare the stored revision with expected_revision.
#[async_trait::async_trait]
pub trait ProfileRepository: std::fmt::Debug + Send + Sync {
    async fn load(&self) -> Result<CatalogSnapshot, ProfileRepositoryError>;

    async fn commit(
        &self,
        expected_revision: u64,
        next: CatalogSnapshot,
    ) -> Result<(), ProfileRepositoryError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProfileRepositoryError {
    #[error("profile catalog changed concurrently")]
    Conflict,
    #[error("profile catalog is unavailable")]
    Unavailable,
    #[error("stored profile catalog failed validation")]
    Corrupt,
}

#[derive(Debug, Error)]
pub enum ProfileServiceError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Repository(#[from] ProfileRepositoryError),
}

#[derive(Debug)]
pub struct ProfileService<R: ProfileRepository> {
    repository: R,
}

impl<R: ProfileRepository> ProfileService<R> {
    pub fn new(repository: R) -> Self {
        Self { repository }
    }

    pub fn into_repository(self) -> R {
        self.repository
    }

    pub async fn load(&self) -> Result<ProfileCatalog, ProfileServiceError> {
        Ok(ProfileCatalog::from_snapshot(
            self.repository.load().await?,
        )?)
    }

    pub async fn preview_batch(
        &self,
        changes: &[CatalogChange],
    ) -> Result<CatalogPreview, ProfileServiceError> {
        Ok(self.load().await?.preview_batch(changes)?)
    }

    pub async fn apply_batch(
        &self,
        expected_revision: u64,
        changes: &[CatalogChange],
    ) -> Result<CatalogPreview, ProfileServiceError> {
        let mut catalog = self.load().await?;
        let preview = catalog.apply_batch(expected_revision, changes)?;
        if !changes.is_empty() {
            self.repository
                .commit(expected_revision, catalog.snapshot())
                .await?;
        }
        Ok(preview)
    }

    pub async fn search(
        &self,
        query: &ProfileQuery,
    ) -> Result<Vec<ProfileRecord>, ProfileServiceError> {
        Ok(self.load().await?.search(query)?)
    }

    pub async fn resolve(
        &self,
        id: ProfileId,
    ) -> Result<ResolvedTerminalSettings, ProfileServiceError> {
        Ok(self.load().await?.resolve(id)?)
    }
}

#[derive(Clone, Debug)]
pub struct ProfileCatalog {
    snapshot: CatalogSnapshot,
}

impl ProfileCatalog {
    pub fn from_snapshot(snapshot: CatalogSnapshot) -> Result<Self, CatalogError> {
        validate(&snapshot)?;
        Ok(Self { snapshot })
    }

    #[must_use]
    pub fn snapshot(&self) -> CatalogSnapshot {
        self.snapshot.clone()
    }

    pub fn preview_batch(&self, changes: &[CatalogChange]) -> Result<CatalogPreview, CatalogError> {
        let (_, preview) = self.prepare(changes)?;
        Ok(preview)
    }

    pub fn apply_batch(
        &mut self,
        expected_revision: u64,
        changes: &[CatalogChange],
    ) -> Result<CatalogPreview, CatalogError> {
        if self.snapshot.revision != expected_revision {
            return Err(CatalogError::StaleRevision {
                expected: expected_revision,
                actual: self.snapshot.revision,
            });
        }
        let (mut next, preview) = self.prepare(changes)?;
        if !changes.is_empty() {
            next.revision = next
                .revision
                .checked_add(1)
                .ok_or(CatalogError::RevisionExhausted)?;
            self.snapshot = next;
        }
        Ok(preview)
    }

    /// Folder filtering is exact; callers can request each descendant explicitly.
    pub fn search(&self, query: &ProfileQuery) -> Result<Vec<ProfileRecord>, CatalogError> {
        if !(1..=MAX_SEARCH_RESULTS).contains(&query.limit) {
            return Err(CatalogError::InvalidSearchLimit);
        }
        let needle = query.text.trim().to_lowercase();
        let tag = query.tag.as_ref().map(|value| value.to_lowercase());
        let mut matches: Vec<_> = self
            .snapshot
            .profiles
            .iter()
            .filter(|profile| {
                query
                    .folder_id
                    .is_none_or(|id| profile.folder_id == Some(id))
                    && query.kind.is_none_or(|kind| profile.kind == kind)
                    && (!query.favorites_only || profile.favorite)
                    && tag.as_ref().is_none_or(|value| {
                        profile
                            .tags
                            .iter()
                            .any(|candidate| candidate.to_lowercase() == *value)
                    })
                    && (needle.is_empty()
                        || profile.name.to_lowercase().contains(&needle)
                        || profile
                            .tags
                            .iter()
                            .any(|candidate| candidate.to_lowercase().contains(&needle)))
            })
            .cloned()
            .collect();
        matches.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then(left.id.cmp(&right.id))
        });
        matches.truncate(query.limit);
        Ok(matches)
    }

    pub fn resolve(&self, id: ProfileId) -> Result<ResolvedTerminalSettings, CatalogError> {
        let profile = self
            .snapshot
            .profiles
            .iter()
            .find(|record| record.id == id)
            .ok_or(CatalogError::UnknownProfile(id))?;
        let mut resolved = ResolvedTerminalSettings {
            terminal_type: ResolvedField {
                value: self.snapshot.defaults.terminal_type.clone(),
                source: SettingSource::Global,
            },
            theme: ResolvedField {
                value: self.snapshot.defaults.theme.clone(),
                source: SettingSource::Global,
            },
            logging: ResolvedField {
                value: self.snapshot.defaults.logging,
                source: SettingSource::Global,
            },
        };
        let mut lineage = Vec::new();
        let mut cursor = profile.folder_id;
        while let Some(folder_id) = cursor {
            let folder = self
                .snapshot
                .folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .ok_or(CatalogError::UnknownFolder(folder_id))?;
            lineage.push(folder);
            cursor = folder.parent_id;
        }
        for folder in lineage.into_iter().rev() {
            merge_settings(
                &mut resolved,
                &folder.terminal,
                SettingSource::Folder(folder.id),
            );
        }
        merge_settings(
            &mut resolved,
            &profile.terminal,
            SettingSource::Profile(profile.id),
        );
        Ok(resolved)
    }

    fn prepare(
        &self,
        changes: &[CatalogChange],
    ) -> Result<(CatalogSnapshot, CatalogPreview), CatalogError> {
        if changes.len() > MAX_BATCH_CHANGES {
            return Err(CatalogError::TooManyChanges);
        }
        let mut next = self.snapshot.clone();
        let mut outcomes = Vec::with_capacity(changes.len());
        for change in changes {
            let outcome = match change {
                CatalogChange::UpsertFolder(folder) => {
                    if let Some(existing) =
                        next.folders.iter_mut().find(|item| item.id == folder.id)
                    {
                        *existing = folder.clone();
                        ChangeOutcome::FolderUpdated(folder.id)
                    } else {
                        next.folders.push(folder.clone());
                        ChangeOutcome::FolderCreated(folder.id)
                    }
                }
                CatalogChange::RemoveFolder(id) => {
                    let old_len = next.folders.len();
                    next.folders.retain(|folder| folder.id != *id);
                    if next.folders.len() == old_len {
                        return Err(CatalogError::UnknownFolder(*id));
                    }
                    ChangeOutcome::FolderRemoved(*id)
                }
                CatalogChange::UpsertProfile(profile) => {
                    if let Some(existing) =
                        next.profiles.iter_mut().find(|item| item.id == profile.id)
                    {
                        *existing = profile.clone();
                        ChangeOutcome::ProfileUpdated(profile.id)
                    } else {
                        next.profiles.push(profile.clone());
                        ChangeOutcome::ProfileCreated(profile.id)
                    }
                }
                CatalogChange::RemoveProfile(id) => {
                    let old_len = next.profiles.len();
                    next.profiles.retain(|profile| profile.id != *id);
                    if next.profiles.len() == old_len {
                        return Err(CatalogError::UnknownProfile(*id));
                    }
                    ChangeOutcome::ProfileRemoved(*id)
                }
            };
            outcomes.push(outcome);
        }
        validate(&next)?;
        Ok((
            next,
            CatalogPreview {
                base_revision: self.snapshot.revision,
                outcomes,
            },
        ))
    }
}

fn merge_settings(
    resolved: &mut ResolvedTerminalSettings,
    overrides: &TerminalOverrides,
    source: SettingSource,
) {
    if let Some(value) = &overrides.terminal_type {
        resolved.terminal_type = ResolvedField {
            value: value.clone(),
            source,
        };
    }
    if let Some(value) = &overrides.theme {
        resolved.theme = ResolvedField {
            value: value.clone(),
            source,
        };
    }
    if let Some(value) = overrides.logging {
        resolved.logging = ResolvedField { value, source };
    }
}

fn valid_text(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_NAME_BYTES
        && !value.chars().any(char::is_control)
}

fn validate(snapshot: &CatalogSnapshot) -> Result<(), CatalogError> {
    if !valid_text(&snapshot.defaults.terminal_type) || !valid_text(&snapshot.defaults.theme) {
        return Err(CatalogError::InvalidText);
    }
    let mut folder_names = BTreeSet::new();
    let mut folder_ids = BTreeSet::new();
    for folder in &snapshot.folders {
        if !valid_text(&folder.name) {
            return Err(CatalogError::InvalidText);
        }
        validate_overrides(&folder.terminal)?;
        if !folder_ids.insert(folder.id)
            || !folder_names.insert((folder.parent_id, folder.name.to_lowercase()))
        {
            return Err(CatalogError::DuplicateName);
        }
    }
    for folder in &snapshot.folders {
        let mut visited = BTreeSet::from([folder.id]);
        let mut cursor = folder.parent_id;
        while let Some(id) = cursor {
            if !visited.insert(id) {
                return Err(CatalogError::FolderCycle);
            }
            let parent = snapshot
                .folders
                .iter()
                .find(|candidate| candidate.id == id)
                .ok_or(CatalogError::UnknownFolder(id))?;
            cursor = parent.parent_id;
        }
    }
    let mut profile_names = BTreeSet::new();
    let mut profile_ids = BTreeSet::new();
    for profile in &snapshot.profiles {
        if !valid_text(&profile.name) {
            return Err(CatalogError::InvalidText);
        }
        validate_overrides(&profile.terminal)?;
        if profile.tags.len() > MAX_TAGS {
            return Err(CatalogError::TooManyTags);
        }
        if profile.tags.iter().any(|tag| {
            tag.trim().is_empty() || tag.len() > MAX_TAG_BYTES || tag.chars().any(char::is_control)
        }) {
            return Err(CatalogError::InvalidTag);
        }
        if let Some(folder_id) = profile.folder_id
            && !folder_ids.contains(&folder_id)
        {
            return Err(CatalogError::UnknownFolder(folder_id));
        }
        if !profile_ids.insert(profile.id)
            || !profile_names.insert((profile.folder_id, profile.name.to_lowercase()))
        {
            return Err(CatalogError::DuplicateName);
        }
    }
    Ok(())
}

fn validate_overrides(overrides: &TerminalOverrides) -> Result<(), CatalogError> {
    if overrides
        .terminal_type
        .as_deref()
        .is_some_and(|value| !valid_text(value))
        || overrides
            .theme
            .as_deref()
            .is_some_and(|value| !valid_text(value))
    {
        return Err(CatalogError::InvalidText);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, parent_id: Option<FolderId>) -> ProfileFolder {
        ProfileFolder {
            id: FolderId::new(),
            name: name.into(),
            parent_id,
            terminal: TerminalOverrides::default(),
        }
    }

    fn profile(name: &str, folder_id: Option<FolderId>) -> ProfileRecord {
        ProfileRecord {
            id: ProfileId::new(),
            name: name.into(),
            kind: ProfileKind::Ssh,
            folder_id,
            tags: BTreeSet::new(),
            favorite: false,
            terminal: TerminalOverrides::default(),
        }
    }

    #[test]
    fn batch_is_atomic_and_preview_does_not_commit() -> Result<(), CatalogError> {
        let mut catalog = ProfileCatalog::from_snapshot(CatalogSnapshot::default())?;
        let folder = folder("Production", None);
        let record = profile("web-01", Some(folder.id));
        let batch = vec![
            CatalogChange::UpsertFolder(folder.clone()),
            CatalogChange::UpsertProfile(record.clone()),
        ];
        let preview = catalog.preview_batch(&batch)?;
        assert_eq!(preview.base_revision, 0);
        assert!(catalog.snapshot().profiles.is_empty());
        catalog.apply_batch(0, &batch)?;
        assert_eq!(catalog.snapshot().revision, 1);
        assert_eq!(catalog.search(&ProfileQuery::default())?, vec![record]);

        let broken = vec![
            CatalogChange::RemoveFolder(folder.id),
            CatalogChange::UpsertProfile(profile("web-02", Some(FolderId::new()))),
        ];
        assert!(matches!(
            catalog.apply_batch(1, &broken),
            Err(CatalogError::UnknownFolder(_))
        ));
        assert_eq!(catalog.snapshot().revision, 1);
        assert_eq!(catalog.snapshot().folders, vec![folder]);
        assert!(matches!(
            catalog.apply_batch(0, &[]),
            Err(CatalogError::StaleRevision { .. })
        ));
        Ok(())
    }

    #[test]
    fn inheritance_tracks_the_source_of_each_setting() -> Result<(), CatalogError> {
        let mut root = folder("Root", None);
        root.terminal.theme = Some("dark".into());
        let mut child = folder("Child", Some(root.id));
        child.terminal.logging = Some(true);
        let mut record = profile("shell", Some(child.id));
        record.terminal.terminal_type = Some("screen-256color".into());
        let catalog = ProfileCatalog::from_snapshot(CatalogSnapshot {
            folders: vec![root.clone(), child.clone()],
            profiles: vec![record.clone()],
            ..CatalogSnapshot::default()
        })?;
        let result = catalog.resolve(record.id)?;
        assert_eq!(
            result.terminal_type.source,
            SettingSource::Profile(record.id)
        );
        assert_eq!(result.theme.source, SettingSource::Folder(root.id));
        assert_eq!(result.logging.source, SettingSource::Folder(child.id));
        assert!(result.logging.value);
        Ok(())
    }

    #[test]
    fn search_filters_case_insensitively_and_has_a_limit() -> Result<(), CatalogError> {
        let mut first = profile("Prod SSH", None);
        first.tags.insert("Critical".into());
        first.favorite = true;
        let second = profile("dev", None);
        let catalog = ProfileCatalog::from_snapshot(CatalogSnapshot {
            profiles: vec![second, first.clone()],
            ..CatalogSnapshot::default()
        })?;
        let result = catalog.search(&ProfileQuery {
            text: "CRIT".into(),
            favorites_only: true,
            limit: 1,
            ..ProfileQuery::default()
        })?;
        assert_eq!(result, vec![first]);
        assert_eq!(
            catalog.search(&ProfileQuery {
                limit: 0,
                ..ProfileQuery::default()
            }),
            Err(CatalogError::InvalidSearchLimit)
        );
        Ok(())
    }

    #[test]
    fn recovery_rejects_cycles_and_duplicate_siblings() {
        let mut first = folder("Ops", None);
        let mut second = folder("ops", Some(first.id));
        first.parent_id = Some(second.id);
        assert!(matches!(
            ProfileCatalog::from_snapshot(CatalogSnapshot {
                folders: vec![first.clone(), second.clone()],
                ..CatalogSnapshot::default()
            }),
            Err(CatalogError::FolderCycle)
        ));
        second.parent_id = None;
        first.parent_id = None;
        assert!(matches!(
            ProfileCatalog::from_snapshot(CatalogSnapshot {
                folders: vec![first, folder("OPS", None)],
                ..CatalogSnapshot::default()
            }),
            Err(CatalogError::DuplicateName)
        ));
    }
}
