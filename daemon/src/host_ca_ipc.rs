//! Public CA changes require a one-use, version-bound preview and typed fingerprint.
use crate::{ProfileIpcService, host_ca_store as store};
use cshell_application::*;
use cshell_domain::{ClipboardHost, HostAuthorityId};
use cshell_ipc::{CaOperation, CaRequest, CaResponse, CaStatus};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, &'static str>;
#[derive(Clone)]
pub(crate) struct HostCaIpcService {
    catalog: Arc<tokio::sync::Mutex<()>>,
    previews: Arc<Mutex<BTreeMap<[u8; 32], Pending>>>,
    inspectors: Arc<tokio::sync::Semaphore>,
}
struct Pending {
    preview: HostCaPreview,
    expires: Instant,
}
impl Default for HostCaIpcService {
    fn default() -> Self {
        Self {
            catalog: Default::default(),
            previews: Default::default(),
            inspectors: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}
impl std::fmt::Debug for HostCaIpcService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCaIpcService").finish_non_exhaustive()
    }
}
fn id(bytes: &[u8]) -> Result<HostAuthorityId> {
    let id = HostAuthorityId::from_bytes(bytes.try_into().map_err(|_| "invalid host CA identity")?);
    if id.as_uuid().is_nil() {
        return Err("nil host CA identity");
    }
    Ok(id)
}
fn effective_rules(catalog: &HostCaCatalog) -> Vec<(String, String, HostCaState)> {
    let mut rules: Vec<_> = catalog
        .items
        .iter()
        .filter(|r| r.definition.state != HostCaState::Disabled)
        .map(|r| {
            (
                r.fingerprint.clone(),
                if r.definition.state == HostCaState::Revoked {
                    String::new()
                } else {
                    r.definition.patterns.clone()
                },
                r.definition.state,
            )
        })
        .collect();
    rules.sort_by(|a, b| a.0.cmp(&b.0));
    rules
}
impl HostCaIpcService {
    pub(crate) async fn handle(
        &self,
        request: CaRequest,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> CaResponse {
        match self.inner(&request, profiles).await {
            Ok(reply) => match serde_json::to_vec(&reply) {
                Ok(bytes) if bytes.len() <= MAX_HOST_CA_DOCUMENT => CaResponse {
                    document_json: bytes,
                    ..Default::default()
                },
                _ => CaResponse {
                    status: CaStatus::Unavailable as i32,
                    detail: "Host CA reply unavailable; reload before another mutation".into(),
                    ..Default::default()
                },
            },
            Err(detail) => CaResponse {
                status: CaStatus::Conflict as i32,
                detail: detail.into(),
                ..Default::default()
            },
        }
    }
    async fn inner(
        &self,
        request: &CaRequest,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Result<HostCaReply> {
        let op =
            CaOperation::try_from(request.operation).map_err(|_| "unknown host CA operation")?;
        let apply = matches!(op, CaOperation::ApplySave | CaOperation::ApplyRemove);
        let canonical = CaRequest {
            operation: op as i32,
            authority_id: if matches!(op, CaOperation::Get | CaOperation::InspectCertificate) {
                Vec::new()
            } else {
                request.authority_id.clone()
            },
            expected_revision: if matches!(op, CaOperation::Get | CaOperation::InspectCertificate) {
                0
            } else {
                request.expected_revision
            },
            document_json: if matches!(
                op,
                CaOperation::PreviewSave | CaOperation::InspectCertificate
            ) {
                request.document_json.clone()
            } else {
                Vec::new()
            },
            preview_token: if apply {
                request.preview_token.clone()
            } else {
                Vec::new()
            },
            confirmed_fingerprint: if apply {
                request.confirmed_fingerprint.clone()
            } else {
                String::new()
            },
            confirmed: apply,
        };
        if canonical != *request
            || request.document_json.len() > 70 * 1024
            || !matches!(op, CaOperation::Get | CaOperation::InspectCertificate)
                && request.expected_revision == 0
        {
            return Err("invalid host CA request shape");
        }
        if op == CaOperation::InspectCertificate {
            return self.inspect(request, profiles).await;
        }
        let _catalog = self.catalog.lock().await;
        let mut catalog = store::read().await?;
        if op == CaOperation::Get {
            return Ok(HostCaReply::Catalog(catalog));
        }
        let id = id(&request.authority_id)?;
        if matches!(op, CaOperation::PreviewSave | CaOperation::PreviewRemove) {
            if catalog.revision != request.expected_revision {
                return Err("host CA catalog changed; reload before previewing");
            }
            let (record, fingerprint) = if op == CaOperation::PreviewSave {
                if request.document_json.len() > 24 * 1024 {
                    return Err("host CA draft limit");
                }
                let mut draft: HostCaDraft = serde_json::from_slice(&request.document_json)
                    .map_err(|_| "invalid host CA draft")?;
                draft.public_key = draft.public_key.trim().into();
                draft.validate()?;
                let patterns = draft.patterns.clone();
                let key = draft.public_key.clone();
                let (canonical, fingerprint) = tokio::task::spawn_blocking(move || {
                    cshell_ssh::normalize_host_authority(&patterns, &key)
                })
                .await
                .map_err(|_| "host CA normalization worker unavailable")??;
                draft.public_key = canonical;
                if catalog
                    .items
                    .iter()
                    .any(|r| r.id == id && r.fingerprint != fingerprint)
                {
                    return Err(
                        "a host CA key identity is immutable; create a new record for rotation",
                    );
                }
                if catalog
                    .items
                    .iter()
                    .any(|r| r.id != id && r.fingerprint == fingerprint)
                {
                    return Err("host CA key already exists; edit its current record");
                }
                let record = HostCaRecord {
                    id,
                    definition: draft,
                    fingerprint: fingerprint.clone(),
                };
                if let Some(saved) = catalog.items.iter_mut().find(|r| r.id == id) {
                    *saved = record.clone();
                } else {
                    catalog.items.push(record.clone());
                }
                catalog.changed()?;
                if serde_json::to_vec(&catalog)
                    .map_err(|_| "host CA encoding failed")?
                    .len()
                    > MAX_HOST_CA_DOCUMENT
                {
                    return Err("host CA catalog byte limit");
                }
                (Some(record), fingerprint)
            } else {
                let row = catalog
                    .items
                    .iter()
                    .find(|r| r.id == id)
                    .ok_or("host CA does not exist")?;
                if row.definition.state == HostCaState::Revoked {
                    return Err(
                        "explicitly review clearing the revocation before removing its record",
                    );
                }
                (None, row.fingerprint.clone())
            };
            let token: [u8; 32] = rand::random();
            let now = Instant::now();
            let preview = HostCaPreview {
                format: "cshell.host-ca-preview".into(),
                version: 1,
                catalog_revision: request.expected_revision,
                id,
                record,
                fingerprint,
                token,
                expires_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| "preview clock unavailable")?
                    .as_secs()
                    + 300,
            };
            let mut pending = self
                .previews
                .lock()
                .map_err(|_| "host CA previews unavailable")?;
            pending.retain(|_, p| p.expires > now);
            if pending.len() >= 16 {
                return Err("host CA preview capacity reached");
            }
            pending.insert(
                token,
                Pending {
                    preview: preview.clone(),
                    expires: now + Duration::from_secs(300),
                },
            );
            return Ok(HostCaReply::Preview(preview));
        }
        let token: [u8; 32] = request
            .preview_token
            .as_slice()
            .try_into()
            .map_err(|_| "invalid host CA preview token")?;
        let pending = self
            .previews
            .lock()
            .map_err(|_| "host CA previews unavailable")?
            .remove(&token)
            .ok_or("preview consumed or unavailable; create a new review")?;
        let preview = pending.preview;
        if pending.expires <= Instant::now()
            || preview.id != id
            || preview.catalog_revision != request.expected_revision
            || catalog.revision != request.expected_revision
            || request.confirmed_fingerprint != preview.fingerprint
            || (op == CaOperation::ApplySave) != preview.record.is_some()
        {
            return Err("host CA review expired or changed; inspect and preview again");
        }
        let profiles = profiles.ok_or("Profile trust controller unavailable")?;
        profiles
            .vault
            .ensure_owned()
            .map_err(|_| "credential storage is owned by another daemon")?;
        let old_rules = effective_rules(&catalog);
        if let Some(record) = preview.record {
            if let Some(saved) = catalog.items.iter_mut().find(|r| r.id == id) {
                *saved = record;
            } else {
                catalog.items.push(record);
            }
        } else {
            let index = catalog
                .items
                .iter()
                .position(|r| r.id == id)
                .ok_or("host CA disappeared")?;
            if catalog.items[index].definition.state == HostCaState::Revoked {
                return Err("revoked record must remain until explicitly reviewed");
            }
            catalog.items.remove(index);
        }
        let previous = catalog.revision;
        catalog.changed()?;
        if old_rules == effective_rules(&catalog) {
            store::write(&catalog, previous).await?;
            return Ok(HostCaReply::Catalog(catalog));
        }
        // Retire in-flight auth, then prevent new verifier snapshots during publication.
        profiles.vault.invalidate();
        let _trust = store::coordination().write_owned().await;
        profiles.vault.invalidate();
        let result = store::write(&catalog, previous).await;
        profiles.vault.invalidate();
        result?;
        Ok(HostCaReply::Catalog(catalog))
    }
    async fn inspect(
        &self,
        request: &CaRequest,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Result<HostCaReply> {
        let input: HostCertificateRequest = serde_json::from_slice(&request.document_json)
            .map_err(|_| "invalid certificate request")?;
        if input.certificate.len() > 65536 {
            return Err("certificate input limit");
        }
        if input
            .target
            .as_ref()
            .is_some_and(|t| ClipboardHost::new(&t.host, t.port).as_ref() != Some(t))
        {
            return Err("certificate target must be canonical");
        }
        let _permit = self
            .inspectors
            .clone()
            .try_acquire_owned()
            .map_err(|_| "certificate inspector capacity reached")?;
        let _trust = store::coordination().read_owned().await;
        let catalog_revision = if input.target.is_some() {
            Some(store::read().await?.revision)
        } else {
            None
        };
        let verifier = if let Some(target) = &input.target {
            let path = profiles
                .ok_or("Profile service unavailable")?
                .known_hosts_path()
                .map_err(|_| "known_hosts path unavailable")?;
            Some(store::verifier(&path, &target.host, target.port).await?)
        } else {
            None
        };
        let target = input.target;
        let certificate = input.certificate;
        use sha2::Digest;
        let certificate_sha256 = sha2::Sha256::digest(certificate.trim().as_bytes()).into();
        let facts = tokio::task::spawn_blocking(move || {
            cshell_ssh::inspect_openssh_certificate(&certificate, verifier.as_ref())
        })
        .await
        .map_err(|_| "certificate worker unavailable")??;
        let host_decision = facts.host_decision.map(|d| match d {
            cshell_ssh::HostKeyCheck::Trusted => CertificateHostDecision::Trusted,
            cshell_ssh::HostKeyCheck::Unknown => CertificateHostDecision::Unknown,
            cshell_ssh::HostKeyCheck::Changed => CertificateHostDecision::Changed,
            cshell_ssh::HostKeyCheck::Revoked => CertificateHostDecision::Revoked,
        });
        Ok(HostCaReply::Certificate(HostCertificateReport {
            certificate_sha256,
            catalog_revision,
            host_certificate: facts.host_certificate,
            serial: facts.serial,
            key_id: facts.key_id,
            principals: facts.principals,
            valid_after: facts.valid_after,
            valid_before: facts.valid_before,
            checked_unix: facts.checked_unix,
            signature_valid: facts.signature_valid,
            time_valid: facts.time_valid,
            subject_fingerprint: facts.subject_fingerprint,
            signer_fingerprint: facts.signer_fingerprint,
            critical_options: facts.critical_options,
            extensions: facts.extensions,
            target,
            host_decision,
        }))
    }
}
