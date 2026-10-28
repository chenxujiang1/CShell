//! Declarative non-secret metadata edits. No connection or credential mutation.
use crate::{CatalogChange, CatalogSnapshot, ProfileCatalog};
use cshell_domain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
pub const MAX_PROFILE_BATCH_BYTES: usize = 64 * 1024;
pub const MAX_PROFILE_BATCH_REPLY: usize = 2 * 1024 * 1024;
pub const MAX_PROFILE_BATCH_SELECTION: usize = 128;
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FieldEdit<T> {
    #[default]
    Keep,
    Set(T),
    Clear,
}
impl<T: Clone> FieldEdit<T> {
    fn apply(&self, value: &mut Option<T>) {
        match self {
            Self::Keep => {}
            Self::Set(next) => *value = Some(next.clone()),
            Self::Clear => *value = None,
        }
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    content = "values",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TagEdit {
    #[default]
    Keep,
    Replace(BTreeSet<String>),
    Append(BTreeSet<String>),
    Remove(BTreeSet<String>),
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalBatchEdit {
    #[serde(default)]
    pub terminal_type: FieldEdit<String>,
    #[serde(default)]
    pub theme: FieldEdit<String>,
    #[serde(default)]
    pub logging: FieldEdit<bool>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatchPatch {
    #[serde(default)]
    pub name_prefix: String,
    #[serde(default)]
    pub name_suffix: String,
    #[serde(default)]
    pub folder: FieldEdit<FolderId>,
    #[serde(default)]
    pub tags: TagEdit,
    #[serde(default)]
    pub favorite: Option<bool>,
    #[serde(default)]
    pub terminal: TerminalBatchEdit,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatchDraft {
    pub format: String,
    pub version: u32,
    pub revision: u64,
    pub profiles: BTreeSet<ProfileId>,
    pub patch: ProfileBatchPatch,
}
impl ProfileBatchDraft {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "cshell.profile-batch"
            || self.version != 1
            || self.profiles.is_empty()
            || self.profiles.len() > MAX_PROFILE_BATCH_SELECTION
            || self.profiles.iter().any(|id| id.as_uuid().is_nil())
        {
            return Err("invalid profile batch format/selection");
        }
        for value in [&self.patch.name_prefix, &self.patch.name_suffix] {
            if value.len() > 64 || value.chars().any(char::is_control) {
                return Err("invalid batch name prefix/suffix");
            }
        }
        if matches!(self.patch.folder,FieldEdit::Set(id) if id.as_uuid().is_nil()) {
            return Err("nil batch folder");
        }
        for value in [
            &self.patch.terminal.terminal_type,
            &self.patch.terminal.theme,
        ] {
            if let FieldEdit::Set(text) = value
                && (text.is_empty() || text.len() > 128 || text.chars().any(char::is_control))
            {
                return Err("invalid batch terminal override");
            }
        }
        if let TagEdit::Replace(tags) | TagEdit::Append(tags) | TagEdit::Remove(tags) =
            &self.patch.tags
        {
            if tags.len() > 64
                || tags
                    .iter()
                    .any(|t| t.is_empty() || t.len() > 64 || t.chars().any(char::is_control))
            {
                return Err("invalid batch tags");
            }
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<[u8; 32], &'static str> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| "batch encoding failed")?;
        if bytes.len() > MAX_PROFILE_BATCH_BYTES {
            return Err("profile batch size limit");
        }
        Ok(Sha256::digest(bytes).into())
    }
}
pub fn decode_profile_batch(bytes: &[u8]) -> Result<ProfileBatchDraft, &'static str> {
    if bytes.len() > MAX_PROFILE_BATCH_BYTES {
        return Err("profile batch size limit");
    }
    let value: ProfileBatchDraft =
        serde_json::from_slice(bytes).map_err(|_| "invalid profile batch document")?;
    value.validate()?;
    Ok(value)
}
fn edited_profile(before: &ProfileRecord, patch: &ProfileBatchPatch) -> ProfileRecord {
    let mut after = before.clone();
    after.name = format!("{}{}{}", patch.name_prefix, before.name, patch.name_suffix);
    patch.folder.apply(&mut after.folder_id);
    match &patch.tags {
        TagEdit::Keep => {}
        TagEdit::Replace(tags) => after.tags = tags.clone(),
        TagEdit::Append(tags) => after.tags.extend(tags.iter().cloned()),
        TagEdit::Remove(tags) => after.tags.retain(|tag| !tags.contains(tag)),
    }
    if let Some(value) = patch.favorite {
        after.favorite = value;
    }
    patch
        .terminal
        .terminal_type
        .apply(&mut after.terminal.terminal_type);
    patch.terminal.theme.apply(&mut after.terminal.theme);
    patch.terminal.logging.apply(&mut after.terminal.logging);
    after
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatchRow {
    pub before: ProfileRecord,
    pub after: ProfileRecord,
    pub settings_before: ResolvedTerminalSettings,
    pub settings_after: ResolvedTerminalSettings,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatchPlan {
    pub format: String,
    pub version: u32,
    pub revision: u64,
    pub request_sha256: [u8; 32],
    pub rows: Vec<ProfileBatchRow>,
}
impl ProfileBatchPlan {
    pub fn fingerprint(&self) -> Result<[u8; 32], String> {
        if self.format != "cshell.profile-batch-plan"
            || self.version != 1
            || self.rows.is_empty()
            || self.rows.len() > MAX_PROFILE_BATCH_SELECTION
        {
            return Err("invalid batch review plan".into());
        }
        let mut ids = BTreeSet::new();
        for row in &self.rows {
            if row.before.id.as_uuid().is_nil()
                || row.before.id != row.after.id
                || !ids.insert(row.before.id)
            {
                return Err("invalid batch review selection".into());
            }
        }
        let bytes = serde_json::to_vec(self).map_err(|_| "batch review encoding failed")?;
        if bytes.len() > MAX_PROFILE_BATCH_REPLY {
            return Err("batch review size limit".into());
        }
        Ok(Sha256::digest(bytes).into())
    }
}
pub fn plan_profile_batch(
    snapshot: &CatalogSnapshot,
    draft: &ProfileBatchDraft,
) -> Result<(ProfileBatchPlan, Vec<CatalogChange>), String> {
    draft.validate().map_err(str::to_owned)?;
    if snapshot.revision != draft.revision {
        return Err("profile batch revision conflict".into());
    }
    let before = ProfileCatalog::from_snapshot(snapshot.clone()).map_err(|e| e.to_string())?;
    let mut changes = Vec::new();
    let mut selected = Vec::new();
    for id in &draft.profiles {
        let original = snapshot
            .profiles
            .iter()
            .find(|r| r.id == *id)
            .ok_or("selected Profile no longer exists")?
            .clone();
        let after = edited_profile(&original, &draft.patch);
        if after != original {
            changes.push(CatalogChange::UpsertProfile(after.clone()));
        }
        selected.push((original, after));
    }
    if changes.is_empty() {
        return Err("profile batch has no changes".into());
    }
    let mut after = ProfileCatalog::from_snapshot(snapshot.clone()).map_err(|e| e.to_string())?;
    after
        .apply_batch(draft.revision, &changes)
        .map_err(|e| e.to_string())?;
    let rows = selected
        .into_iter()
        .map(|(original, next)| {
            Ok(ProfileBatchRow {
                settings_before: before.resolve(original.id).map_err(|e| e.to_string())?,
                settings_after: after.resolve(next.id).map_err(|e| e.to_string())?,
                before: original,
                after: next,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((
        ProfileBatchPlan {
            format: "cshell.profile-batch-plan".into(),
            version: 1,
            revision: draft.revision,
            request_sha256: draft.fingerprint().map_err(str::to_owned)?,
            rows,
        },
        changes,
    ))
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatchPreview {
    pub plan: ProfileBatchPlan,
    pub token: [u8; 32],
    pub expires_unix: u64,
}
impl std::fmt::Debug for ProfileBatchPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileBatchPreview")
            .field("revision", &self.plan.revision)
            .field("rows", &self.plan.rows.len())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatchApplied {
    pub revision: u64,
    pub request_sha256: [u8; 32],
    pub profiles: Vec<ProfileRecord>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSettingsReport {
    pub revision: u64,
    pub profile: ProfileRecord,
    pub settings: ResolvedTerminalSettings,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileBatchAuthority {
    pub revision: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProfileBatchReply {
    Preview(ProfileBatchPreview),
    Applied(ProfileBatchApplied),
    Settings(ProfileSettingsReport),
    Authority(ProfileBatchAuthority),
}
pub enum ProfileBatchExpectation<'a> {
    Preview {
        draft: &'a ProfileBatchDraft,
        snapshot: Option<&'a CatalogSnapshot>,
    },
    Applied(&'a ProfileBatchPreview),
    Settings {
        profile: ProfileId,
        revision: Option<u64>,
    },
    Authority,
}
pub fn decode_profile_batch_reply(
    bytes: &[u8],
    expect: ProfileBatchExpectation<'_>,
) -> Result<ProfileBatchReply, String> {
    if bytes.len() > MAX_PROFILE_BATCH_REPLY {
        return Err("profile batch reply limit".into());
    }
    let reply: ProfileBatchReply =
        serde_json::from_slice(bytes).map_err(|_| "invalid profile batch reply")?;
    match (&reply, expect) {
        (ProfileBatchReply::Authority(_), ProfileBatchExpectation::Authority) => {}
        (
            ProfileBatchReply::Preview(preview),
            ProfileBatchExpectation::Preview { draft, snapshot },
        ) => {
            let p = &preview.plan;
            let ids = p.rows.iter().map(|r| r.before.id).collect::<BTreeSet<_>>();
            if p.format != "cshell.profile-batch-plan"
                || p.version != 1
                || p.revision != draft.revision
                || p.request_sha256 != draft.fingerprint().map_err(str::to_owned)?
                || ids != draft.profiles
                || p.rows.len() != ids.len()
                || preview.expires_unix == 0
                || preview.token == [0; 32]
                || p.rows
                    .iter()
                    .any(|r| r.after != edited_profile(&r.before, &draft.patch))
            {
                return Err("profile batch preview scope/effect mismatch".into());
            }
            if let Some(snapshot) = snapshot
                && plan_profile_batch(snapshot, draft)?.0 != *p
            {
                return Err("profile batch preview differs from reviewed catalog".into());
            }
        }
        (ProfileBatchReply::Applied(applied), ProfileBatchExpectation::Applied(preview)) => {
            if Some(applied.revision) != preview.plan.revision.checked_add(1)
                || applied.request_sha256 != preview.plan.request_sha256
                || applied.profiles
                    != preview
                        .plan
                        .rows
                        .iter()
                        .map(|r| r.after.clone())
                        .collect::<Vec<_>>()
            {
                return Err("profile batch apply effect mismatch".into());
            }
        }
        (
            ProfileBatchReply::Settings(report),
            ProfileBatchExpectation::Settings { profile, revision },
        ) => {
            if report.profile.id != profile || revision.is_some_and(|r| r != report.revision) {
                return Err("settings inspection scope mismatch".into());
            }
            for source in [
                report.settings.terminal_type.source,
                report.settings.theme.source,
                report.settings.logging.source,
            ] {
                if matches!(source,SettingSource::Profile(id) if id!=profile) {
                    return Err("settings source mismatch".into());
                }
            }
        }
        _ => return Err("profile batch reply type mismatch".into()),
    }
    Ok(reply)
}
