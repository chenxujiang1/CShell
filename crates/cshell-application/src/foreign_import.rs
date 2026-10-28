use crate::{
    MobaXtermImportSummary, PROFILE_CONNECTION_IMPORT_VERSION, ProfileImportDocument,
    PuttyImportResult, PuttyImportSummary, SecureCrtImportSummary, XshellImportSummary,
};
use serde::{Deserialize, Serialize};
/// Untagged only to retain the existing PuTTY v1 review representation. Each
/// variant has a closed, distinct schema; source identity is checked separately.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ForeignProfileImportSummary {
    Putty(PuttyImportSummary),
    SecureCrt(SecureCrtImportSummary),
    Xshell(XshellImportSummary),
    MobaXterm(MobaXtermImportSummary),
    Batch(crate::ForeignBatchSummary),
}
impl ForeignProfileImportSummary {
    pub fn source(&self) -> &'static str {
        match self {
            Self::Putty(_) => "putty-registry",
            Self::SecureCrt(_) => "securecrt-ini",
            Self::Xshell(_) => "xshell-session",
            Self::MobaXterm(_) => "mobaxterm-bookmarks",
            Self::Batch(_) => "foreign-batch",
        }
    }
    pub fn notices(&self) -> Vec<String> {
        match self {
            Self::Putty(summary) => summary.notices(),
            Self::SecureCrt(summary) => summary.notices(),
            Self::Xshell(summary) => summary.notices(),
            Self::MobaXterm(summary) => summary.notices(),
            Self::Batch(summary) => summary.notices(),
        }
    }
    pub fn validate(&self, document: &ProfileImportDocument) -> bool {
        let count = document.profiles.len();
        if document.version != PROFILE_CONNECTION_IMPORT_VERSION
            || count == 0
            || count > 512
            || count != document.ssh_connections.len()
            || (!document.folders.is_empty()
                && !matches!(self, Self::MobaXterm(_) | Self::Batch(_)))
            || !document.local_connections.is_empty()
        {
            return false;
        }
        if document.profiles.iter().any(|p| {
            p.kind != cshell_domain::ProfileKind::Ssh
                || (p.folder_id.is_some() && !matches!(self, Self::MobaXterm(_) | Self::Batch(_)))
                || p.id.as_uuid().is_nil()
                || p.terminal.logging != Some(false)
        }) || document.ssh_connections.iter().any(|c| {
            c.auth_method != cshell_domain::SshAuthMethod::Password
                || c.private_key_path.is_some()
                || c.certificate_path.is_some()
                || c.agent_identity.is_some()
                || c.route != cshell_domain::SshRoute::Direct
        }) {
            return false;
        }
        match self {
            Self::Batch(s) => s.validate_document(document),
            Self::Putty(s) => {
                s.sessions_read <= 512
                    && s.imported == count
                    && [
                        s.defaults_skipped,
                        s.other_protocols_skipped,
                        s.empty_hosts_skipped,
                        s.trust_overrides_omitted,
                        s.logging_settings_omitted,
                    ]
                    .iter()
                    .all(|n| *n <= 512)
                    && s.values_omitted <= 32768
                    && s.key_references_omitted <= 1024
                    && s.imported
                        + s.defaults_skipped
                        + s.other_protocols_skipped
                        + s.empty_hosts_skipped
                        == s.sessions_read
            }
            Self::SecureCrt(s) => {
                count == 1
                    && s.validate()
                    && document
                        .profiles
                        .iter()
                        .all(|p| p.terminal.terminal_type.is_none())
            }
            Self::MobaXterm(s) => s.validate_document(document),
            Self::Xshell(s) => {
                count == 1
                    && s.validate()
                    && document.profiles.iter().all(|p| {
                        p.terminal.terminal_type.is_none()
                            && p.terminal.theme.is_none()
                            && !p.favorite
                            && p.tags.len() == 1
                            && p.tags.contains("import:xshell")
                    })
                    && document
                        .ssh_connections
                        .iter()
                        .all(|c| c.agent_backend == cshell_domain::SshAgentBackend::Auto)
            }
        }
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignProfileImportResult {
    pub document: ProfileImportDocument,
    pub summary: ForeignProfileImportSummary,
}
impl From<PuttyImportResult> for ForeignProfileImportResult {
    fn from(result: PuttyImportResult) -> Self {
        Self {
            document: result.document,
            summary: ForeignProfileImportSummary::Putty(result.summary),
        }
    }
}
impl std::fmt::Debug for ForeignProfileImportResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignProfileImportResult")
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}
