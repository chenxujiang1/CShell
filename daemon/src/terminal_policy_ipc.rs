use crate::ProfileIpcService;
use cshell_application::*;
use cshell_domain::*;
use cshell_ipc::{
    TerminalPolicyOperation as Op, TerminalPolicyRequest as Request,
    TerminalPolicyResponse as Response, TerminalPolicyStatus as Status,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
struct Pending {
    preview: TerminalPolicyPreview,
    expires: Instant,
}
#[derive(Clone, Default)]
pub(crate) struct TerminalPolicyService {
    mutation: Arc<tokio::sync::Mutex<()>>,
    pending: Arc<Mutex<BTreeMap<[u8; 32], Pending>>>,
}
impl std::fmt::Debug for TerminalPolicyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalPolicyService")
            .finish_non_exhaustive()
    }
}
impl TerminalPolicyService {
    pub async fn handle(
        &self,
        request: &Request,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Response {
        match self.execute(request, profiles).await {
            Ok(reply) => match serde_json::to_vec(&reply) {
                Ok(document_json) if document_json.len() <= MAX_TERMINAL_POLICY_REPLY => Response {
                    status: Status::Ok as i32,
                    document_json,
                    detail: String::new(),
                },
                _ => failure(Status::Unavailable, "terminal policy reply unavailable"),
            },
            Err((status, detail)) => failure(status, &detail),
        }
    }
    async fn execute(
        &self,
        request: &Request,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Result<TerminalPolicyReply, (Status, String)> {
        let op = Op::try_from(request.operation)
            .map_err(|_| invalid("unknown terminal policy operation"))?;
        let canonical = Request {
            operation: request.operation,
            expected_revision: if op == Op::Get {
                0
            } else {
                request.expected_revision
            },
            profile_revision: if op == Op::Get {
                0
            } else {
                request.profile_revision
            },
            host_revision: if op == Op::Get {
                0
            } else {
                request.host_revision
            },
            document_json: if matches!(op, Op::Preview | Op::Resolve) {
                request.document_json.clone()
            } else {
                Vec::new()
            },
            preview_token: if op == Op::Apply {
                request.preview_token.clone()
            } else {
                Vec::new()
            },
            review_sha256: if op == Op::Apply {
                request.review_sha256.clone()
            } else {
                Vec::new()
            },
            confirmed: op == Op::Apply && request.confirmed,
        };
        if request != &canonical || request.document_json.len() > MAX_TERMINAL_POLICY_BYTES + 1024 {
            return Err(invalid("noncanonical/oversized terminal policy request"));
        }
        let access = profiles.ok_or_else(|| unavailable("profile authority unavailable"))?;
        access
            .vault
            .ensure_owned()
            .map_err(|_| unavailable("credential storage is owned by another daemon"))?;
        let _gate = self.mutation.lock().await;
        let _profiles = access.catalog_mutation.lock().await;
        let profiles = access
            .service
            .load()
            .await
            .map_err(|_| unavailable("profile catalog unavailable"))?
            .snapshot();
        let hosts = access
            .service
            .repository()
            .load_hosts()
            .await
            .map_err(|_| unavailable("host catalog unavailable"))?
            .unwrap_or(HostCatalogSnapshot {
                revision: 0,
                catalog: HostCatalog::default(),
            });
        validate_hosts(&hosts.catalog).map_err(|_| unavailable("host catalog invalid"))?;
        let current = crate::terminal_policy_store::read()
            .await
            .map_err(unavailable)?;
        if op != Op::Get
            && (request.expected_revision != current.revision
                || request.profile_revision != profiles.revision
                || request.host_revision != hosts.revision)
        {
            return Err(conflict());
        }
        match op {
            Op::Get => Ok(TerminalPolicyReply::Catalog(TerminalPolicyView {
                catalog: current,
                profile_revision: profiles.revision,
                host_revision: hosts.revision,
            })),
            Op::Preview => {
                let proposal: TerminalPolicyProposal =
                    serde_json::from_slice(&request.document_json)
                        .map_err(|_| invalid("invalid policy proposal"))?;
                proposal.fingerprint().map_err(invalid)?;
                if proposal.catalog.revision != current.revision
                    || proposal.profile_revision != profiles.revision
                    || proposal.host_revision != hosts.revision
                {
                    return Err(conflict());
                }
                validate_references(&proposal.catalog, &profiles, &hosts.catalog)?;
                if proposal.catalog == current {
                    return Err(invalid("policy proposal has no changes"));
                }
                let matched_profiles =
                    affected(&current, &proposal.catalog, &profiles, &hosts.catalog);
                if matched_profiles.len() > 2048 {
                    return Err(invalid("policy impact preview limit"));
                }
                let mut token: [u8; 32] = rand::random();
                if token == [0; 32] {
                    token[0] = 1;
                }
                let expires_unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| unavailable("policy clock unavailable"))?
                    .as_secs()
                    .saturating_add(300);
                let preview = TerminalPolicyPreview {
                    proposal,
                    token,
                    expires_unix,
                    matched_profiles,
                };
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.retain(|_, p| p.expires > Instant::now());
                if pending.len() >= 16 || pending.contains_key(&token) {
                    return Err(unavailable("terminal policy preview budget"));
                }
                pending.insert(
                    token,
                    Pending {
                        preview: preview.clone(),
                        expires: Instant::now() + Duration::from_secs(300),
                    },
                );
                Ok(TerminalPolicyReply::Preview(preview))
            }
            Op::Apply => {
                let token: [u8; 32] = request
                    .preview_token
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("invalid policy token"))?;
                let pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&token)
                    .ok_or_else(|| invalid("policy preview missing; prepare a new preview"))?;
                let preview = pending.preview;
                if !request.confirmed
                    || pending.expires <= Instant::now()
                    || request.review_sha256.as_slice()
                        != preview.fingerprint().map_err(invalid)?.as_slice()
                {
                    return Err(invalid("policy confirmation/expiry/review mismatch"));
                }
                if preview.proposal.catalog.revision != current.revision
                    || preview.proposal.profile_revision != profiles.revision
                    || preview.proposal.host_revision != hosts.revision
                {
                    return Err(conflict());
                }
                validate_references(&preview.proposal.catalog, &profiles, &hosts.catalog)?;
                let mut next = preview.proposal.catalog;
                next.revision = next
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| unavailable("policy revision exhausted"))?;
                crate::terminal_policy_store::write(&next, current.revision)
                    .await
                    .map_err(|e| {
                        if e.contains("revision") {
                            conflict()
                        } else {
                            unavailable(e)
                        }
                    })?;
                Ok(TerminalPolicyReply::Catalog(TerminalPolicyView {
                    catalog: next,
                    profile_revision: profiles.revision,
                    host_revision: hosts.revision,
                }))
            }
            Op::Resolve => {
                if request.document_json.len() > 8 * 1024 {
                    return Err(invalid("resolution context limit"));
                }
                let context: TerminalResolutionContext =
                    serde_json::from_slice(&request.document_json)
                        .map_err(|_| invalid("invalid terminal context"))?;
                context.validate().map_err(invalid)?;
                let profile = profiles
                    .profiles
                    .iter()
                    .find(|p| p.id == context.profile_id)
                    .ok_or_else(|| invalid("Profile context missing"))?
                    .clone();
                let host = context
                    .host_id
                    .map(|id| {
                        hosts
                            .catalog
                            .hosts
                            .iter()
                            .find(|h| h.id == id)
                            .cloned()
                            .ok_or_else(|| invalid("Host context missing"))
                    })
                    .transpose()?;
                let captured = profiles.clone();
                let policies = current.clone();
                let request_context = context.clone();
                let selected_host = host.clone();
                let (settings, matched_tags) = tokio::task::spawn_blocking(move || {
                    resolve_terminal_context(
                        &captured,
                        &policies,
                        &request_context,
                        selected_host.as_ref(),
                    )
                })
                .await
                .map_err(|_| unavailable("terminal resolution worker failed"))?
                .map_err(|e| (Status::Invalid, e))?;
                if !matches!(settings.theme.value.as_str(), "default" | "dark" | "light")
                    && !profiles
                        .themes
                        .iter()
                        .any(|t| t.name == settings.theme.value)
                {
                    return Err(invalid("resolved context references an unknown theme"));
                }
                Ok(TerminalPolicyReply::Resolved(TerminalResolutionReport {
                    profile_revision: profiles.revision,
                    host_revision: hosts.revision,
                    policy_revision: current.revision,
                    context,
                    profile,
                    host,
                    matched_tags,
                    settings,
                }))
            }
        }
    }
}
fn validate_references(
    policy: &TerminalPolicyCatalog,
    profiles: &CatalogSnapshot,
    hosts: &HostCatalog,
) -> Result<(), (Status, String)> {
    policy.validate().map_err(invalid)?;
    for overrides in policy
        .tags
        .iter()
        .map(|r| &r.terminal)
        .chain(policy.hosts.iter().map(|r| &r.terminal))
    {
        if let Some(theme) = &overrides.theme
            && !matches!(theme.as_str(), "default" | "dark" | "light")
            && !profiles.themes.iter().any(|t| &t.name == theme)
        {
            return Err(invalid("policy references an unknown theme"));
        }
    }
    for rule in &policy.hosts {
        let host = hosts
            .hosts
            .iter()
            .find(|h| h.id == rule.host_id)
            .ok_or_else(|| invalid("policy references an unknown Host"))?;
        if !profiles.profiles.iter().any(|p| p.id == host.profile_id) {
            return Err(invalid("Host policy references a missing Profile"));
        }
    }
    Ok(())
}
fn affected(
    before: &TerminalPolicyCatalog,
    after: &TerminalPolicyCatalog,
    profiles: &CatalogSnapshot,
    hosts: &HostCatalog,
) -> Vec<ProfileId> {
    let tags = before
        .tags
        .iter()
        .chain(after.tags.iter())
        .map(|r| r.tag.to_lowercase())
        .collect::<BTreeSet<_>>();
    let mut ids = profiles
        .profiles
        .iter()
        .filter(|p| p.tags.iter().any(|t| tags.contains(&t.to_lowercase())))
        .map(|p| p.id)
        .collect::<BTreeSet<_>>();
    for rule in before.hosts.iter().chain(after.hosts.iter()) {
        if let Some(host) = hosts.hosts.iter().find(|h| h.id == rule.host_id) {
            ids.insert(host.profile_id);
        }
    }
    ids.into_iter().collect()
}
fn invalid(message: &str) -> (Status, String) {
    (Status::Invalid, message.into())
}
fn unavailable(message: &str) -> (Status, String) {
    (Status::Unavailable, message.into())
}
fn conflict() -> (Status, String) {
    (
        Status::Conflict,
        "policy/Profile/Host revision changed; reload and prepare a new preview".into(),
    )
}
fn failure(status: Status, detail: &str) -> Response {
    Response {
        status: status as i32,
        detail: detail.chars().take(1024).collect(),
        ..Default::default()
    }
}
