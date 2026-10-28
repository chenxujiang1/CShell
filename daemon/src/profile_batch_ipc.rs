use crate::ProfileIpcService;
use cshell_application::*;
use cshell_domain::ProfileId;
use cshell_ipc::{
    ProfileBatchOperation as Op, ProfileBatchRequest as Request, ProfileBatchResponse as Response,
    ProfileBatchStatus as Status,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
struct Pending {
    draft: ProfileBatchDraft,
    preview: ProfileBatchPreview,
    expires: Instant,
}
#[derive(Clone, Default)]
pub(crate) struct ProfileBatchService {
    pending: Arc<Mutex<BTreeMap<[u8; 32], Pending>>>,
}
impl std::fmt::Debug for ProfileBatchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileBatchService")
            .finish_non_exhaustive()
    }
}
impl ProfileBatchService {
    pub async fn handle(
        &self,
        request: &Request,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Response {
        let result = self.execute(request, profiles).await;
        match result {
            Ok(reply) => match serde_json::to_vec(&reply) {
                Ok(document_json) if document_json.len() <= MAX_PROFILE_BATCH_REPLY => Response {
                    status: Status::Ok as i32,
                    document_json,
                    detail: String::new(),
                },
                _ => failure(Status::Unavailable, "profile batch reply unavailable"),
            },
            Err((status, detail)) => failure(status, &detail),
        }
    }
    async fn execute(
        &self,
        request: &Request,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Result<ProfileBatchReply, (Status, String)> {
        let op = Op::try_from(request.operation).map_err(|_| invalid("invalid batch operation"))?;
        let canonical = Request {
            operation: request.operation,
            expected_revision: if op == Op::Reload {
                0
            } else {
                request.expected_revision
            },
            document_json: if op == Op::Preview {
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
            profile_id: if op == Op::Inspect {
                request.profile_id.clone()
            } else {
                Vec::new()
            },
            confirmed: op == Op::Apply && request.confirmed,
        };
        if request != &canonical || request.document_json.len() > MAX_PROFILE_BATCH_BYTES {
            return Err(invalid("noncanonical or oversized batch request"));
        }
        let access = profiles.ok_or_else(|| unavailable("profile authority unavailable"))?;
        access
            .vault
            .ensure_owned()
            .map_err(|_| unavailable("credential storage is owned by another daemon"))?;
        // All batch writers share the Profile mutation lock and the repository's
        // existing transaction/CAS. Pending lock is never held across an await.
        let _mutation = access.catalog_mutation.lock().await;
        let catalog = access
            .service
            .load()
            .await
            .map_err(|_| unavailable("profile catalog unavailable"))?;
        let snapshot = catalog.snapshot();
        match op {
            Op::Reload => Ok(ProfileBatchReply::Authority(ProfileBatchAuthority {
                revision: snapshot.revision,
            })),
            Op::Inspect => {
                if request.expected_revision != 0 && request.expected_revision != snapshot.revision
                {
                    return Err(conflict());
                }
                let id = ProfileId::from_bytes(
                    request
                        .profile_id
                        .as_slice()
                        .try_into()
                        .map_err(|_| invalid("invalid Profile UUID"))?,
                );
                let record = snapshot
                    .profiles
                    .iter()
                    .find(|p| p.id == id && !id.as_uuid().is_nil())
                    .ok_or_else(|| invalid("unknown Profile"))?
                    .clone();
                let settings = catalog
                    .resolve(id)
                    .map_err(|_| invalid("Profile inheritance could not be resolved"))?;
                Ok(ProfileBatchReply::Settings(ProfileSettingsReport {
                    revision: snapshot.revision,
                    profile: record,
                    settings,
                }))
            }
            Op::Preview => {
                let draft = decode_profile_batch(&request.document_json).map_err(invalid)?;
                if draft.revision != request.expected_revision
                    || snapshot.revision != draft.revision
                {
                    return Err(conflict());
                }
                let captured = snapshot.clone();
                let proposal = draft.clone();
                let (plan, _) =
                    tokio::task::spawn_blocking(move || plan_profile_batch(&captured, &proposal))
                        .await
                        .map_err(|_| unavailable("profile batch planning worker failed"))?
                        .map_err(|e| (Status::Invalid, e))?;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| unavailable("preview clock unavailable"))?
                    .as_secs();
                let mut token: [u8; 32] = rand::random();
                if token == [0; 32] {
                    token[0] = 1;
                }
                let preview = ProfileBatchPreview {
                    plan,
                    token,
                    expires_unix: now.saturating_add(300),
                };
                let body = serde_json::to_vec(&ProfileBatchReply::Preview(preview.clone()))
                    .map_err(|_| unavailable("preview encoding failed"))?;
                if body.len() > MAX_PROFILE_BATCH_REPLY {
                    return Err(invalid("profile batch preview size limit"));
                }
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.retain(|_, p| p.expires > Instant::now());
                if pending.len() >= 16 {
                    return Err(unavailable("profile batch preview budget"));
                }
                if pending.contains_key(&token) {
                    return Err(unavailable("preview token collision"));
                }
                pending.insert(
                    token,
                    Pending {
                        draft,
                        preview: preview.clone(),
                        expires: Instant::now() + Duration::from_secs(300),
                    },
                );
                Ok(ProfileBatchReply::Preview(preview))
            }
            Op::Apply => {
                let token: [u8; 32] = request
                    .preview_token
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("invalid batch preview token"))?;
                // Consume even a refused attempt. Never resurrect a preview after
                // a failed/uncertain commit or another daemon/catalog revision.
                let pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&token)
                    .ok_or_else(|| invalid("batch preview unavailable; prepare a new preview"))?;
                if !request.confirmed || pending.expires <= Instant::now() {
                    return Err(invalid("explicit unexpired batch confirmation required"));
                }
                if request.review_sha256.as_slice()
                    != pending
                        .preview
                        .plan
                        .fingerprint()
                        .map_err(|_| invalid("invalid stored preview"))?
                        .as_slice()
                {
                    return Err(invalid("confirmed batch review content mismatch"));
                }
                if pending.preview.plan.revision != request.expected_revision
                    || snapshot.revision != request.expected_revision
                {
                    return Err(conflict());
                }
                let expected = pending.preview.plan;
                let draft = pending.draft;
                let (plan, changes) =
                    tokio::task::spawn_blocking(move || plan_profile_batch(&snapshot, &draft))
                        .await
                        .map_err(|_| unavailable("profile batch planning worker failed"))?
                        .map_err(|e| (Status::Invalid, e))?;
                if plan != expected {
                    return Err(conflict());
                }
                access
                    .vault
                    .ensure_owned()
                    .map_err(|_| unavailable("credential storage is owned by another daemon"))?;
                access
                    .service
                    .apply_batch(request.expected_revision, &changes)
                    .await
                    .map_err(|error| match error {
                        ProfileServiceError::Repository(ProfileRepositoryError::Conflict) => {
                            conflict()
                        }
                        ProfileServiceError::Catalog(CatalogError::StaleRevision { .. }) => {
                            conflict()
                        }
                        _ => unavailable(
                            "profile batch commit unavailable; reload before a new preview",
                        ),
                    })?;
                Ok(ProfileBatchReply::Applied(ProfileBatchApplied {
                    revision: request
                        .expected_revision
                        .checked_add(1)
                        .ok_or_else(|| unavailable("profile revision exhausted"))?,
                    request_sha256: plan.request_sha256,
                    profiles: plan.rows.into_iter().map(|r| r.after).collect(),
                }))
            }
        }
    }
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
        "profile batch revision/effect conflict; reload and prepare a new preview".into(),
    )
}
fn failure(status: Status, detail: &str) -> Response {
    Response {
        status: status as i32,
        detail: detail.chars().take(1024).collect(),
        ..Default::default()
    }
}
