//! Pure, bounded aggregation of explicitly selected metadata sources.
//! File discovery and reads belong to the cshell-import adapter.
use crate::*;
use cshell_domain::{FolderId, ProfileFolder, ProfileId, TerminalOverrides};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_FOREIGN_SOURCE_FILES: usize = 512;
pub const MAX_FOREIGN_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FOREIGN_DIRECTORY_ENTRIES: usize = 4096;
pub const MAX_FOREIGN_FOLDERS: usize = 256;
pub const MAX_FOREIGN_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForeignSessionFormat {
    PuttyRegistry,
    SecureCrtIni,
    XshellSession,
    MobaXtermUnicode,
    MobaXtermWindows1252,
}
impl ForeignSessionFormat {
    /// None denotes a syntactically valid non-SSH single-session source.
    /// Parser errors, including malformed SSH sessions, always stop the batch.
    pub fn convert(
        self,
        bytes: &[u8],
        name: &str,
        default_username: &str,
    ) -> Result<Option<ForeignProfileImportResult>, ForeignBatchError> {
        let result = match self {
            Self::PuttyRegistry => {
                import_putty_registry(bytes, PuttyImportOptions { default_username })
                    .map(Into::into)
                    .map_err(|_| ForeignBatchError::Source)?
            }
            Self::SecureCrtIni => {
                if !crate::securecrt_import::batch_is_ssh(bytes)
                    .map_err(|_| ForeignBatchError::Source)?
                {
                    return Ok(None);
                }
                import_securecrt_ini(
                    bytes,
                    SecureCrtImportOptions {
                        session_name: name,
                        default_username,
                    },
                )
                .map_err(|_| ForeignBatchError::Source)?
            }
            Self::XshellSession => {
                if !crate::xshell_import::batch_is_ssh(bytes)
                    .map_err(|_| ForeignBatchError::Source)?
                {
                    return Ok(None);
                }
                import_xshell_session(
                    bytes,
                    XshellImportOptions {
                        session_name: name,
                        default_username,
                    },
                )
                .map_err(|_| ForeignBatchError::Source)?
            }
            Self::MobaXtermUnicode | Self::MobaXtermWindows1252 => import_mobaxterm_sessions(
                bytes,
                MobaXtermImportOptions {
                    default_username,
                    encoding: if self == Self::MobaXtermUnicode {
                        MobaXtermEncoding::Unicode
                    } else {
                        MobaXtermEncoding::Windows1252
                    },
                },
            )
            .map_err(|_| ForeignBatchError::Source)?,
        };
        Ok(Some(result))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ForeignBatchError {
    #[error("invalid or unsupported migration manifest")]
    Manifest,
    #[error("migration source uses an invalid or ambiguous relative path or folder")]
    Path,
    #[error("migration batch exceeds its file, byte, folder, depth or session budget")]
    Limit,
    #[error("a migration source is malformed or uses unsupported SSH settings; batch refused")]
    Source,
    #[error("migration batch contains no supported SSH sessions")]
    Empty,
    #[error("converted migration metadata is invalid or has conflicting folder/session names")]
    Target,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignSourceManifest {
    pub format: String,
    pub version: u32,
    pub entries: Vec<ForeignSourceEntry>,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignSourceEntry {
    pub path: String,
    pub format: ForeignSessionFormat,
    #[serde(default)]
    pub folder: Vec<String>,
}
impl std::fmt::Debug for ForeignSourceManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignSourceManifest")
            .field("entries", &self.entries.len())
            .finish()
    }
}
impl std::fmt::Debug for ForeignSourceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignSourceEntry")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}
pub fn valid_foreign_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.trim() == value
        && !matches!(value, "." | "..")
        && !value.ends_with('.')
        && !value
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        && !matches!(
            value
                .split('.')
                .next()
                .unwrap_or("")
                .trim_end()
                .to_ascii_uppercase()
                .as_str(),
            "CON"
                | "CONIN$"
                | "CONOUT$"
                | "CLOCK$"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "COM¹"
                | "COM²"
                | "COM³"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
                | "LPT¹"
                | "LPT²"
                | "LPT³"
        )
}
impl ForeignSourceManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, ForeignBatchError> {
        if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
            return Err(ForeignBatchError::Limit);
        }
        let manifest: Self =
            serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
                .map_err(|_| ForeignBatchError::Manifest)?;
        if manifest.format != "cshell.foreign-source-manifest" || manifest.version != 1 {
            return Err(ForeignBatchError::Manifest);
        }
        if manifest.entries.is_empty() || manifest.entries.len() > MAX_FOREIGN_SOURCE_FILES {
            return Err(ForeignBatchError::Limit);
        }
        let mut identities = BTreeSet::new();
        let mut path_components = BTreeMap::new();
        for entry in &manifest.entries {
            let parts: Vec<_> = entry.path.split('/').collect();
            if parts.len() > MAX_FOREIGN_DEPTH + 1
                || !parts.iter().all(|p| valid_foreign_component(p))
                || entry.folder.len() > MAX_FOREIGN_DEPTH
                || !entry.folder.iter().all(|p| valid_foreign_component(p))
                || !identities.insert(entry.path.to_lowercase())
            {
                return Err(ForeignBatchError::Path);
            }
            for end in 1..=parts.len() {
                let original: Vec<_> = parts[..end].iter().map(|s| s.to_string()).collect();
                let folded: Vec<_> = parts[..end].iter().map(|s| s.to_lowercase()).collect();
                if path_components
                    .insert(folded, original.clone())
                    .is_some_and(|previous| previous != original)
                {
                    return Err(ForeignBatchError::Path);
                }
            }
        }
        Ok(manifest)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignBatchCounts {
    pub source_files: usize,
    pub input_bytes: usize,
    pub directory_entries: usize,
    pub ignored_files: usize,
    pub defaults_skipped: usize,
    pub non_ssh_files_skipped: usize,
}
impl ForeignBatchCounts {
    pub fn valid(&self) -> bool {
        (1..=MAX_FOREIGN_SOURCE_FILES).contains(&self.source_files)
            && (1..=MAX_FOREIGN_INPUT_BYTES).contains(&self.input_bytes)
            && self.directory_entries <= MAX_FOREIGN_DIRECTORY_ENTRIES
            && self.ignored_files <= MAX_FOREIGN_DIRECTORY_ENTRIES
            && self.defaults_skipped <= MAX_FOREIGN_DIRECTORY_ENTRIES
            && self.non_ssh_files_skipped <= self.source_files
            && if self.directory_entries == 0 {
                self.ignored_files == 0 && self.defaults_skipped == 0
            } else {
                self.source_files + self.ignored_files + self.defaults_skipped
                    <= self.directory_entries
            }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignBatchMemberSummary {
    pub summary: ForeignProfileImportSummary,
    pub profiles: Vec<ProfileId>,
    /// Original source folders only; generated manifest/directory prefixes are separate.
    pub folders: Vec<FolderId>,
    /// Generated ancestor chain, root first. Shared prefixes may recur across members.
    pub prefix_folders: Vec<FolderId>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignBatchSummary {
    pub format: String,
    pub counts: ForeignBatchCounts,
    pub imported: usize,
    pub folders_created: usize,
    pub members: Vec<ForeignBatchMemberSummary>,
}
impl ForeignBatchSummary {
    pub fn notices(&self) -> Vec<String> {
        let mut notices = vec![format!(
            "Migration batch: {} source files, {} SSH sessions, {} folders; {} defaults/informational files, {} non-SSH files and {} unrelated files skipped.",
            self.counts.source_files, self.imported, self.folders_created, self.counts.defaults_skipped,
            self.counts.non_ssh_files_skipped, self.counts.ignored_files),
            "Source credentials, trust overrides and executable behavior are not migrated. Review each omission below; credentials must be rebound explicitly before connecting.".into()];
        // Numbered source groups deliberately omit original file paths and bytes.
        for (index, member) in self.members.iter().enumerate() {
            if matches!(member.summary, ForeignProfileImportSummary::Batch(_)) {
                continue;
            }
            for line in member.summary.notices() {
                notices.push(format!("Converted source group {}: {line}", index + 1));
            }
        }
        notices
    }
    pub(crate) fn validate_document(&self, document: &ProfileImportDocument) -> bool {
        if self.format != "cshell.foreign-batch-summary"
            || !self.counts.valid()
            || self.members.is_empty()
            || self.members.len() > MAX_FOREIGN_SOURCE_FILES
            || self.members.len() + self.counts.non_ssh_files_skipped != self.counts.source_files
            || self.imported != document.profiles.len()
            || self.folders_created != document.folders.len()
            || document.folders.len() > MAX_FOREIGN_FOLDERS
            || document.fingerprint().is_err()
        {
            return false;
        }
        let mut profiles = BTreeSet::new();
        let mut folders = BTreeSet::new();
        let mut prefixes = BTreeSet::new();
        for member in &self.members {
            if matches!(member.summary, ForeignProfileImportSummary::Batch(_))
                || member.profiles.is_empty()
                || member.profiles.len() > 512
                || member.folders.len() > MAX_FOREIGN_FOLDERS
                || member.prefix_folders.len() > MAX_FOREIGN_DEPTH
                || member.profiles.iter().any(|id| !profiles.insert(*id))
                || member.folders.iter().any(|id| !folders.insert(*id))
            {
                return false;
            }
            let pids: BTreeSet<_> = member.profiles.iter().copied().collect();
            let fids: BTreeSet<_> = member.folders.iter().copied().collect();
            let mut prefix_parent = None;
            for id in &member.prefix_folders {
                let Some(folder) = document.folders.iter().find(|f| f.id == *id) else {
                    return false;
                };
                if folder.parent_id != prefix_parent || !valid_foreign_component(&folder.name) {
                    return false;
                }
                prefixes.insert(*id);
                prefix_parent = Some(*id);
            }
            let mut leaf = document.clone();
            leaf.profiles.retain(|p| pids.contains(&p.id));
            leaf.ssh_connections
                .retain(|c| pids.contains(&c.profile_id));
            leaf.folders.retain(|f| fids.contains(&f.id));
            if leaf.profiles.len() != pids.len() || leaf.folders.len() != fids.len() {
                return false;
            }
            for p in &mut leaf.profiles {
                if !p.folder_id.is_some_and(|id| fids.contains(&id)) {
                    if p.folder_id != prefix_parent {
                        return false;
                    }
                    p.folder_id = None;
                }
            }
            for f in &mut leaf.folders {
                if !f.parent_id.is_some_and(|id| fids.contains(&id)) {
                    if f.parent_id != prefix_parent {
                        return false;
                    }
                    f.parent_id = None;
                }
            }
            if !member.summary.validate(&leaf) {
                return false;
            }
        }
        profiles.len() == document.profiles.len()
            && folders.is_disjoint(&prefixes)
            && folders.len() + prefixes.len() == document.folders.len()
            && document.folders.iter().all(|f| {
                if f.id.as_uuid().is_nil() || f.terminal != TerminalOverrides::default() {
                    return false;
                }
                let mut cursor = Some(f.id);
                for _ in 0..MAX_FOREIGN_DEPTH {
                    let Some(id) = cursor else {
                        return true;
                    };
                    let Some(node) = document.folders.iter().find(|n| n.id == id) else {
                        return false;
                    };
                    cursor = node.parent_id;
                }
                cursor.is_none()
            })
    }
}

/// An aggregate contains converted metadata only; raw source buffers are never retained.
#[derive(Debug)]
pub struct ForeignBatchBuilder {
    document: ProfileImportDocument,
    members: Vec<ForeignBatchMemberSummary>,
    prefixes: BTreeMap<Vec<String>, FolderId>,
}
impl Default for ForeignBatchBuilder {
    fn default() -> Self {
        Self {
            document: ProfileImportDocument {
                format: PROFILE_IMPORT_FORMAT.into(),
                version: PROFILE_CONNECTION_IMPORT_VERSION,
                folders: vec![],
                profiles: vec![],
                ssh_connections: vec![],
                local_connections: vec![],
            },
            members: vec![],
            prefixes: BTreeMap::new(),
        }
    }
}
impl ForeignBatchBuilder {
    pub fn push(
        &mut self,
        mut source: ForeignProfileImportResult,
        prefix: &[String],
    ) -> Result<(), ForeignBatchError> {
        if matches!(source.summary, ForeignProfileImportSummary::Batch(_))
            || !source.summary.validate(&source.document)
        {
            return Err(ForeignBatchError::Source);
        }
        if prefix.len() > MAX_FOREIGN_DEPTH || !prefix.iter().all(|p| valid_foreign_component(p)) {
            return Err(ForeignBatchError::Path);
        }
        if self.members.len() >= MAX_FOREIGN_SOURCE_FILES
            || self.document.profiles.len() + source.document.profiles.len() > 512
        {
            return Err(ForeignBatchError::Limit);
        }
        let mut member = ForeignBatchMemberSummary {
            summary: source.summary,
            profiles: source.document.profiles.iter().map(|p| p.id).collect(),
            folders: source.document.folders.iter().map(|f| f.id).collect(),
            prefix_folders: Vec::new(),
        };
        let mut parent = None;
        for end in 1..=prefix.len() {
            let key = prefix[..end].to_vec();
            // Lowercase aliases are rejected rather than silently merged.
            if self.prefixes.keys().any(|existing| {
                existing != &key
                    && existing
                        .iter()
                        .map(|s| s.to_lowercase())
                        .collect::<Vec<_>>()
                        == key.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>()
            }) {
                return Err(ForeignBatchError::Path);
            }
            let id = if let Some(id) = self.prefixes.get(&key) {
                *id
            } else {
                let id = FolderId::new();
                self.document.folders.push(ProfileFolder {
                    id,
                    name: prefix[end - 1].clone(),
                    parent_id: parent,
                    terminal: TerminalOverrides::default(),
                });
                self.prefixes.insert(key, id);
                id
            };
            member.prefix_folders.push(id);
            parent = Some(id);
        }
        for folder in &mut source.document.folders {
            if folder.parent_id.is_none() {
                folder.parent_id = parent;
            }
        }
        for profile in &mut source.document.profiles {
            if profile.folder_id.is_none() {
                profile.folder_id = parent;
            }
        }
        self.document.folders.extend(source.document.folders);
        self.document.profiles.extend(source.document.profiles);
        self.document
            .ssh_connections
            .extend(source.document.ssh_connections);
        if self.document.folders.len() > MAX_FOREIGN_FOLDERS {
            return Err(ForeignBatchError::Limit);
        }
        self.members.push(member);
        Ok(())
    }
    pub fn finish(
        self,
        counts: ForeignBatchCounts,
    ) -> Result<ForeignProfileImportResult, ForeignBatchError> {
        if self.document.profiles.is_empty() {
            return Err(ForeignBatchError::Empty);
        }
        let summary = ForeignProfileImportSummary::Batch(ForeignBatchSummary {
            format: "cshell.foreign-batch-summary".into(),
            counts,
            imported: self.document.profiles.len(),
            folders_created: self.document.folders.len(),
            members: self.members,
        });
        if !summary.validate(&self.document) {
            return Err(ForeignBatchError::Target);
        }
        Ok(ForeignProfileImportResult {
            document: self.document,
            summary,
        })
    }
}
