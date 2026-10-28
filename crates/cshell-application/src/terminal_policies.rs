//! Scalar terminal settings policies; never inherit credentials or transport.
use crate::{CatalogSnapshot, ProfileCatalog, profiles::merge_settings};
use cshell_domain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
pub const MAX_TERMINAL_POLICY_BYTES: usize = 256 * 1024;
pub const MAX_TERMINAL_POLICY_REPLY: usize = 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagTerminalPolicy {
    pub id: TerminalPolicyId,
    pub tag: String,
    pub priority: u16,
    pub terminal: TerminalOverrides,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostTerminalPolicy {
    pub host_id: HostId,
    pub terminal: TerminalOverrides,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalPolicyCatalog {
    pub format: String,
    pub version: u32,
    pub revision: u64,
    pub tags: Vec<TagTerminalPolicy>,
    pub hosts: Vec<HostTerminalPolicy>,
}
impl Default for TerminalPolicyCatalog {
    fn default() -> Self {
        Self {
            format: "cshell.terminal-policies".into(),
            version: 1,
            revision: 1,
            tags: Vec::new(),
            hosts: Vec::new(),
        }
    }
}
pub fn validate_terminal_context_overrides(value: &TerminalOverrides) -> Result<(), &'static str> {
    for text in [&value.terminal_type, &value.theme].into_iter().flatten() {
        if text.trim().is_empty() || text.len() > 128 || text.chars().any(char::is_control) {
            return Err("invalid terminal policy override");
        }
    }
    Ok(())
}
impl TerminalPolicyCatalog {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "cshell.terminal-policies"
            || self.version != 1
            || self.revision == 0
            || self.tags.len() > 64
            || self.hosts.len() > 256
        {
            return Err("terminal policy format/count limit");
        }
        let mut ids = BTreeSet::new();
        let mut priorities = BTreeSet::new();
        for rule in &self.tags {
            if rule.id.as_uuid().is_nil()
                || !ids.insert(rule.id)
                || rule.tag.trim().is_empty()
                || rule.tag.len() > 64
                || rule.tag.chars().any(char::is_control)
                || rule.priority > 1000
                || !priorities.insert((rule.tag.to_lowercase(), rule.priority))
            {
                return Err("invalid/ambiguous tag policy");
            }
            validate_terminal_context_overrides(&rule.terminal)?;
        }
        let mut hosts = BTreeSet::new();
        for rule in &self.hosts {
            if rule.host_id.as_uuid().is_nil() || !hosts.insert(rule.host_id) {
                return Err("invalid/duplicate host policy");
            }
            validate_terminal_context_overrides(&rule.terminal)?;
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<[u8; 32], &'static str> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| "policy encoding failed")?;
        if bytes.len() > MAX_TERMINAL_POLICY_BYTES {
            return Err("terminal policy byte limit");
        }
        Ok(Sha256::digest(bytes).into())
    }
}
pub fn decode_terminal_policies(bytes: &[u8]) -> Result<TerminalPolicyCatalog, &'static str> {
    if bytes.len() > MAX_TERMINAL_POLICY_BYTES {
        return Err("terminal policy byte limit");
    }
    let catalog: TerminalPolicyCatalog =
        serde_json::from_slice(bytes).map_err(|_| "invalid terminal policies")?;
    catalog.validate()?;
    Ok(catalog)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalResolutionContext {
    pub profile_id: ProfileId,
    #[serde(default)]
    pub host_id: Option<HostId>,
    #[serde(default)]
    pub temporary: TerminalOverrides,
}
impl TerminalResolutionContext {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.profile_id.as_uuid().is_nil()
            || self.host_id.is_some_and(|id| id.as_uuid().is_nil())
        {
            return Err("invalid terminal context");
        }
        validate_terminal_context_overrides(&self.temporary)
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalPolicyView {
    pub catalog: TerminalPolicyCatalog,
    pub profile_revision: u64,
    pub host_revision: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalResolutionReport {
    pub profile_revision: u64,
    pub host_revision: u64,
    pub policy_revision: u64,
    pub context: TerminalResolutionContext,
    pub profile: ProfileRecord,
    pub host: Option<HostRecord>,
    pub matched_tags: Vec<TerminalPolicyId>,
    pub settings: ResolvedTerminalSettings,
}
/// Review artifact for one explicit launch. It is a captured resolution, not a
/// credential or network override, and must be recomputed by the launch authority.
pub fn decode_terminal_resolution_report(bytes: &[u8]) -> Result<TerminalResolutionReport, String> {
    if bytes.len() > MAX_TERMINAL_POLICY_REPLY {
        return Err("terminal resolution report limit".into());
    }
    let report: TerminalResolutionReport =
        serde_json::from_slice(bytes).map_err(|_| "invalid terminal resolution report")?;
    let encoded = serde_json::to_vec(&TerminalPolicyReply::Resolved(report.clone()))
        .map_err(|_| "resolution encoding failed")?;
    decode_terminal_policy_reply(
        &encoded,
        TerminalPolicyExpectation::Resolved {
            context: &report.context,
            profile_revision: report.profile_revision,
            host_revision: report.host_revision,
            policy_revision: report.policy_revision,
        },
    )?;
    Ok(report)
}
pub fn resolve_terminal_context(
    snapshot: &CatalogSnapshot,
    policies: &TerminalPolicyCatalog,
    context: &TerminalResolutionContext,
    host: Option<&HostRecord>,
) -> Result<(ResolvedTerminalSettings, Vec<TerminalPolicyId>), String> {
    policies.validate().map_err(str::to_owned)?;
    context.validate().map_err(str::to_owned)?;
    if host.map(|h| h.id) != context.host_id
        || host.is_some_and(|h| h.profile_id != context.profile_id)
    {
        return Err("Host does not belong to the selected Profile context".into());
    }
    let catalog = ProfileCatalog::from_snapshot(snapshot.clone()).map_err(|e| e.to_string())?;
    resolve_validated_terminal_context(&catalog, policies, context, host)
}
pub fn resolve_validated_terminal_context(
    catalog: &ProfileCatalog,
    policies: &TerminalPolicyCatalog,
    context: &TerminalResolutionContext,
    host: Option<&HostRecord>,
) -> Result<(ResolvedTerminalSettings, Vec<TerminalPolicyId>), String> {
    policies.validate().map_err(str::to_owned)?;
    context.validate().map_err(str::to_owned)?;
    if host.map(|h| h.id) != context.host_id
        || host.is_some_and(|h| h.profile_id != context.profile_id)
    {
        return Err("Host context mismatch".into());
    }
    let snapshot = catalog.snapshot_ref();
    let profile = snapshot
        .profiles
        .iter()
        .find(|p| p.id == context.profile_id)
        .ok_or("Profile context missing")?;
    let mut settings = catalog
        .resolve_folder_defaults(profile.id)
        .map_err(|e| e.to_string())?;
    let profile_tags = profile
        .tags
        .iter()
        .map(|t| t.to_lowercase())
        .collect::<BTreeSet<_>>();
    let mut matching = policies
        .tags
        .iter()
        .filter(|p| profile_tags.contains(&p.tag.to_lowercase()))
        .collect::<Vec<_>>();
    matching.sort_by_key(|r| (r.priority, r.id));
    let matched = matching.iter().map(|r| r.id).collect();
    for rule in matching {
        merge_settings(&mut settings, &rule.terminal, SettingSource::Tag(rule.id));
    }
    merge_settings(
        &mut settings,
        &profile.terminal,
        SettingSource::Profile(profile.id),
    );
    if let Some(host) = host
        && let Some(rule) = policies.hosts.iter().find(|r| r.host_id == host.id)
    {
        merge_settings(&mut settings, &rule.terminal, SettingSource::Host(host.id));
    }
    merge_settings(&mut settings, &context.temporary, SettingSource::Temporary);
    Ok((settings, matched))
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalPolicyProposal {
    pub catalog: TerminalPolicyCatalog,
    pub profile_revision: u64,
    pub host_revision: u64,
}
impl TerminalPolicyProposal {
    pub fn fingerprint(&self) -> Result<[u8; 32], &'static str> {
        self.catalog.fingerprint()?;
        let bytes = serde_json::to_vec(self).map_err(|_| "policy proposal encoding failed")?;
        Ok(Sha256::digest(bytes).into())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalPolicyPreview {
    pub proposal: TerminalPolicyProposal,
    pub token: [u8; 32],
    pub expires_unix: u64,
    pub matched_profiles: Vec<ProfileId>,
}
impl std::fmt::Debug for TerminalPolicyPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalPolicyPreview")
            .field("revision", &self.proposal.catalog.revision)
            .finish_non_exhaustive()
    }
}
impl TerminalPolicyPreview {
    pub fn fingerprint(&self) -> Result<[u8; 32], &'static str> {
        self.proposal.fingerprint()?;
        if self.token == [0; 32]
            || self.expires_unix == 0
            || self.matched_profiles.len() > 2048
            || self.matched_profiles.iter().any(|id| id.as_uuid().is_nil())
            || self
                .matched_profiles
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != self.matched_profiles.len()
        {
            return Err("invalid policy preview scope");
        }
        let bytes = serde_json::to_vec(&(self.proposal.clone(), &self.matched_profiles))
            .map_err(|_| "policy review encoding failed")?;
        if bytes.len() > MAX_TERMINAL_POLICY_REPLY {
            return Err("policy review limit");
        }
        Ok(Sha256::digest(bytes).into())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TerminalPolicyReply {
    Catalog(TerminalPolicyView),
    Preview(TerminalPolicyPreview),
    Resolved(TerminalResolutionReport),
}
pub enum TerminalPolicyExpectation<'a> {
    Catalog,
    Preview(&'a TerminalPolicyProposal),
    Saved(&'a TerminalPolicyPreview),
    Resolved {
        context: &'a TerminalResolutionContext,
        profile_revision: u64,
        host_revision: u64,
        policy_revision: u64,
    },
}
pub fn decode_terminal_policy_reply(
    bytes: &[u8],
    expect: TerminalPolicyExpectation<'_>,
) -> Result<TerminalPolicyReply, String> {
    if bytes.len() > MAX_TERMINAL_POLICY_REPLY {
        return Err("terminal policy reply limit".into());
    }
    let reply: TerminalPolicyReply =
        serde_json::from_slice(bytes).map_err(|_| "invalid terminal policy reply")?;
    match (&reply, expect) {
        (TerminalPolicyReply::Catalog(view), TerminalPolicyExpectation::Catalog) => {
            view.catalog.fingerprint().map_err(str::to_owned)?;
        }
        (TerminalPolicyReply::Preview(preview), TerminalPolicyExpectation::Preview(proposal)) => {
            preview.fingerprint().map_err(str::to_owned)?;
            proposal.fingerprint().map_err(str::to_owned)?;
            if preview.proposal != *proposal
                || preview.token == [0; 32]
                || preview.expires_unix == 0
                || preview.matched_profiles.len() > 2048
                || preview
                    .matched_profiles
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != preview.matched_profiles.len()
            {
                return Err("terminal policy preview binding mismatch".into());
            }
        }
        (TerminalPolicyReply::Catalog(view), TerminalPolicyExpectation::Saved(preview)) => {
            preview.fingerprint().map_err(str::to_owned)?;
            let mut expected = preview.proposal.catalog.clone();
            expected.revision = expected
                .revision
                .checked_add(1)
                .ok_or("policy revision exhausted")?;
            if view.catalog != expected
                || view.profile_revision != preview.proposal.profile_revision
                || view.host_revision != preview.proposal.host_revision
            {
                return Err("terminal policy save effect mismatch".into());
            }
        }
        (
            TerminalPolicyReply::Resolved(report),
            TerminalPolicyExpectation::Resolved {
                context,
                profile_revision,
                host_revision,
                policy_revision,
            },
        ) => {
            context.validate().map_err(str::to_owned)?;
            if report.context != *context
                || report.profile.id != context.profile_id
                || report.profile_revision != profile_revision
                || report.host_revision != host_revision
                || report.policy_revision != policy_revision
                || report.host.as_ref().map(|h| h.id) != context.host_id
                || report
                    .host
                    .as_ref()
                    .is_some_and(|h| h.profile_id != context.profile_id)
                || report.matched_tags.len() > 64
                || report.matched_tags.iter().any(|id| id.as_uuid().is_nil())
                || report
                    .matched_tags
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != report.matched_tags.len()
                || [
                    &report.settings.terminal_type.value,
                    &report.settings.theme.value,
                ]
                .iter()
                .any(|text| {
                    text.trim().is_empty() || text.len() > 128 || text.chars().any(char::is_control)
                })
            {
                return Err("terminal resolution context/version mismatch".into());
            }
            for source in [
                report.settings.terminal_type.source,
                report.settings.theme.source,
                report.settings.logging.source,
            ] {
                match source {
                    SettingSource::Folder(id) if id.as_uuid().is_nil() => {
                        return Err("resolution folder source invalid".into());
                    }
                    SettingSource::Profile(id) if id != context.profile_id => {
                        return Err("resolution Profile source mismatch".into());
                    }
                    SettingSource::Host(id) if Some(id) != context.host_id => {
                        return Err("resolution Host source mismatch".into());
                    }
                    SettingSource::Tag(id) if !report.matched_tags.contains(&id) => {
                        return Err("resolution tag source mismatch".into());
                    }
                    _ => {}
                }
            }
        }
        _ => return Err("terminal policy reply type mismatch".into()),
    }
    Ok(reply)
}
