//! Bounded Theme transfer with the same revision and transaction as Profiles.
use crate::{
    CatalogChange, CatalogError, CatalogSnapshot, ImportAction, ImportConflictPolicy,
    ImportItemKind, ImportItemPreview, ProfileCatalog, ProfileImportPreview, ProfileRepository,
    ProfileService, ProfileServiceError,
};
use cshell_domain::ThemeRecord;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const THEME_FORMAT: &str = "cshell.terminal-themes";
pub const THEME_VERSION: u32 = 1;
pub const MAX_THEME_BYTES: usize = 256 * 1024;
pub const MAX_THEMES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeDocument {
    pub format: String,
    pub version: u32,
    pub themes: Vec<ThemeRecord>,
}

#[derive(Debug, Error)]
pub enum ThemeTransferError {
    #[error("Theme file exceeds 256 KiB")]
    TooLarge,
    #[error("Theme document is not valid JSON, TOML or XCS")]
    InvalidDocument,
    #[error("unsupported Theme format or version")]
    UnsupportedFormat,
    #[error("Theme import has unresolved conflicts")]
    Conflicts,
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Service(#[from] ProfileServiceError),
}

pub fn validate_themes(themes: &[ThemeRecord]) -> Result<(), CatalogError> {
    let mut names = std::collections::BTreeSet::new();
    if themes.len() > MAX_THEMES {
        return Err(CatalogError::InvalidTheme);
    }
    for theme in themes {
        let name = &theme.name;
        if name.is_empty()
            || name.len() > 64
            || !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
            || matches!(name.as_str(), "default" | "dark" | "light")
            || !names.insert(name)
        {
            return Err(CatalogError::InvalidTheme);
        }
    }
    Ok(())
}

impl ThemeDocument {
    pub fn from_xcs(bytes: &[u8]) -> Result<Self, ThemeTransferError> {
        let themes =
            cshell_theme::import_xcs(bytes).map_err(|_| ThemeTransferError::InvalidDocument)?;
        validate_themes(&themes)?;
        Ok(Self {
            format: THEME_FORMAT.into(),
            version: THEME_VERSION,
            themes,
        })
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, ThemeTransferError> {
        if bytes.len() > MAX_THEME_BYTES {
            return Err(ThemeTransferError::TooLarge);
        }
        let document: Self =
            serde_json::from_slice(bytes).map_err(|_| ThemeTransferError::InvalidDocument)?;
        if document.format != THEME_FORMAT || document.version != THEME_VERSION {
            return Err(ThemeTransferError::UnsupportedFormat);
        }
        validate_themes(&document.themes)?;
        Ok(document)
    }
}

pub fn export_themes(snapshot: &CatalogSnapshot) -> Result<Vec<u8>, ThemeTransferError> {
    validate_themes(&snapshot.themes)?;
    let mut themes = snapshot.themes.clone();
    themes.sort_by(|a, b| a.name.cmp(&b.name));
    let bytes = serde_json::to_vec_pretty(&ThemeDocument {
        format: THEME_FORMAT.into(),
        version: THEME_VERSION,
        themes,
    })
    .map_err(|_| ThemeTransferError::InvalidDocument)?;
    ThemeDocument::parse(&bytes)?;
    Ok(bytes)
}

fn plan(
    catalog: &ProfileCatalog,
    document: &ThemeDocument,
    policy: ImportConflictPolicy,
) -> Result<(Vec<CatalogChange>, ProfileImportPreview), ThemeTransferError> {
    let snapshot = catalog.snapshot();
    let mut changes = Vec::new();
    let mut items = Vec::new();
    for theme in &document.themes {
        let existing = snapshot.themes.iter().find(|old| old.name == theme.name);
        let action = match (existing, policy) {
            (None, _) => ImportAction::Create,
            (Some(_), ImportConflictPolicy::Fail) => ImportAction::Conflict,
            (Some(_), ImportConflictPolicy::Skip) => ImportAction::Skip,
            (Some(_), ImportConflictPolicy::Replace) => ImportAction::Replace,
        };
        if matches!(action, ImportAction::Create | ImportAction::Replace) {
            changes.push(CatalogChange::UpsertTheme(theme.clone()));
        }
        let retained = existing
            .filter(|_| action == ImportAction::Skip)
            .unwrap_or(theme);
        let palette = retained.palette;
        items.push(ImportItemPreview {
            kind: ImportItemKind::Theme,
            name: theme.name.clone(),
            action,
            theme: Some(retained.clone()),
            details: format!(
                "foreground #{:02x}{:02x}{:02x}; background #{:02x}{:02x}{:02x}; 16 ANSI colors",
                palette.foreground[0],
                palette.foreground[1],
                palette.foreground[2],
                palette.background[0],
                palette.background[1],
                palette.background[2]
            ),
        });
    }
    let can_commit = !items
        .iter()
        .any(|item| item.action == ImportAction::Conflict);
    if can_commit {
        catalog.preview_batch(&changes)?;
    }
    let preview = ProfileImportPreview {
        base_revision: snapshot.revision,
        change_count: changes.len(),
        items,
        can_commit,
    };
    Ok((changes, preview))
}

impl<R: ProfileRepository> ProfileService<R> {
    pub async fn preview_themes(
        &self,
        bytes: &[u8],
        policy: ImportConflictPolicy,
    ) -> Result<ProfileImportPreview, ThemeTransferError> {
        let document = ThemeDocument::parse(bytes)?;
        let catalog = self.load().await?;
        Ok(plan(&catalog, &document, policy)?.1)
    }
    pub async fn commit_themes(
        &self,
        expected_revision: u64,
        bytes: &[u8],
        policy: ImportConflictPolicy,
    ) -> Result<(), ThemeTransferError> {
        let document = ThemeDocument::parse(bytes)?;
        let catalog = self.load().await?;
        let actual = catalog.snapshot().revision;
        if actual != expected_revision {
            return Err(CatalogError::StaleRevision {
                expected: expected_revision,
                actual,
            }
            .into());
        }
        let (changes, preview) = plan(&catalog, &document, policy)?;
        if !preview.can_commit {
            return Err(ThemeTransferError::Conflicts);
        }
        self.apply_batch(expected_revision, &changes).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
