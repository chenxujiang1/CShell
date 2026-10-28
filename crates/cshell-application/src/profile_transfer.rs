//! Public Profile transfer data; no Vault or keychain access is possible here.
use crate::{
    CatalogSnapshot, MAX_PROFILE_IMPORT_BYTES, PROFILE_IMPORT_FORMAT, PROFILE_TRANSFER_VERSION,
    ProfileCatalog, ProfileImportDocument, ProfileImportError,
};
use cshell_domain::{
    LocalClosePolicy, LocalConnectionRecord, LocalWorkingDirectory, ProfileId, TerminalDefaults,
    TerminalOverrides,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalProfileTransfer {
    pub profile_id: ProfileId,
    pub program: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    pub cwd: LocalWorkingDirectory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_overrides: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub close_policy: LocalClosePolicy,
}

impl std::fmt::Debug for LocalProfileTransfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalProfileTransfer")
            .field("profile_id", &self.profile_id)
            .field("includes_args", &self.args.is_some())
            .field("includes_environment", &self.env_overrides.is_some())
            .finish_non_exhaustive()
    }
}

impl LocalProfileTransfer {
    pub(crate) fn record(&self, existing: Option<&LocalConnectionRecord>) -> LocalConnectionRecord {
        LocalConnectionRecord {
            profile_id: self.profile_id,
            program: self.program.clone(),
            cwd: self.cwd.clone(),
            close_policy: self.close_policy,
            args: self
                .args
                .clone()
                .unwrap_or_else(|| existing.map_or_else(Vec::new, |record| record.args.clone())),
            env_overrides: self.env_overrides.clone().unwrap_or_else(|| {
                existing.map_or_else(BTreeMap::new, |record| record.env_overrides.clone())
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProfileExportOptions {
    pub include_local_context: bool,
}

/// Freeze current global defaults into root overrides, retaining internal inheritance.
pub fn export_profile_catalog(
    snapshot: &CatalogSnapshot,
    options: ProfileExportOptions,
) -> Result<Vec<u8>, ProfileImportError> {
    ProfileCatalog::from_snapshot(snapshot.clone())?;
    let mut document = ProfileImportDocument {
        format: PROFILE_IMPORT_FORMAT.into(),
        version: PROFILE_TRANSFER_VERSION,
        folders: snapshot.folders.clone(),
        profiles: snapshot.profiles.clone(),
        ssh_connections: snapshot.ssh_connections.clone(),
        local_connections: snapshot
            .local_connections
            .iter()
            .map(|record| LocalProfileTransfer {
                profile_id: record.profile_id,
                program: record.program.clone(),
                cwd: record.cwd.clone(),
                close_policy: record.close_policy,
                args: options.include_local_context.then(|| record.args.clone()),
                env_overrides: options
                    .include_local_context
                    .then(|| record.env_overrides.clone()),
            })
            .collect(),
    };
    for folder in document
        .folders
        .iter_mut()
        .filter(|folder| folder.parent_id.is_none())
    {
        freeze_defaults(&mut folder.terminal, &snapshot.defaults);
    }
    for profile in document
        .profiles
        .iter_mut()
        .filter(|profile| profile.folder_id.is_none())
    {
        freeze_defaults(&mut profile.terminal, &snapshot.defaults);
    }
    let bytes = serde_json::to_vec_pretty(&document)?;
    if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(ProfileImportError::TooLarge);
    }
    ProfileImportDocument::parse(&bytes)?;
    Ok(bytes)
}

fn freeze_defaults(overrides: &mut TerminalOverrides, defaults: &TerminalDefaults) {
    overrides
        .terminal_type
        .get_or_insert_with(|| defaults.terminal_type.clone());
    overrides
        .theme
        .get_or_insert_with(|| defaults.theme.clone());
    overrides.logging.get_or_insert(defaults.logging);
}

#[cfg(test)]
mod tests;
