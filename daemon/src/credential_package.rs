//! Credential transfer leases retain ciphertext only; confirmations derive again.
use crate::ProfileIpcService;
use crate::vault_control::{binding, reference};
use cshell_application::CatalogSnapshot;
use cshell_domain::{ProfileId, SshConnectionRecord, SshRoute};
use cshell_ipc::{
    CredentialPackageItem, CredentialPackagePreview, MAX_CREDENTIAL_PACKAGE_BYTES,
    MAX_CREDENTIAL_PACKAGE_PROFILES, ProfileImportAction as Action, ProfileImportPolicy as Policy,
    VaultControlStatus as Status, VaultOperation as Op, VaultRequest, VaultResponse, decode_id,
};
use cshell_vault::{
    CredentialPackage, CredentialPackageRecord, Secret, VaultCredentialKind as Kind, VaultError,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

const LEASE_LIFETIME: Duration = Duration::from_secs(120);
#[derive(Default)]
pub(crate) struct PackageLeases(HashMap<[u8; 32], Lease>);
impl std::fmt::Debug for PackageLeases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageLeases")
            .field("count", &self.0.len())
            .finish()
    }
}
struct Lease {
    bytes: Vec<u8>,
    policy: Policy,
    profile_revision: u64,
    vault_revision: u64,
    generation: u64,
    expires: Instant,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    version: u32,
    profiles: Vec<PackageProfile>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageProfile {
    name: String,
    folder: Vec<String>,
    target: SshConnectionRecord,
}
struct Prepared {
    records: Vec<CredentialPackageRecord>,
    items: Vec<CredentialPackageItem>,
}

impl ProfileIpcService {
    pub(crate) async fn handle_package(&self, request: VaultRequest) -> VaultResponse {
        match self.package_inner(request).await {
            Ok(response) => response,
            Err(error) => VaultResponse {
                status: crate::vault_control::status(error) as i32,
                ..Default::default()
            },
        }
    }
    async fn package_inner(&self, mut request: VaultRequest) -> Result<VaultResponse, VaultError> {
        let derivation = self
            .package_derivations
            .clone()
            .try_acquire_owned()
            .map_err(|_| VaultError::Busy)?;
        let operation = Op::try_from(request.operation).map_err(|_| VaultError::InvalidInput)?;
        if !request.profile_id.is_empty()
            || request.portable
            || request.secret.is_empty()
            || request.secret.len() > 1024
        {
            return Err(VaultError::InvalidInput);
        }
        let password = Secret::new(std::mem::take(&mut request.secret));
        let policy =
            Policy::try_from(request.package_policy).map_err(|_| VaultError::InvalidInput)?;
        if operation == Op::ExportPackage {
            if !request.package.is_empty()
                || !request.preview_token.is_empty()
                || policy != Policy::Fail
                || request.selected_profiles.is_empty()
                || request.selected_profiles.len() > MAX_CREDENTIAL_PACKAGE_PROFILES
            {
                return Err(VaultError::InvalidInput);
            }
            let snapshot = self
                .package_snapshot(request.expected_profile_revision)
                .await?;
            let ids = request
                .selected_profiles
                .iter()
                .map(|id| {
                    decode_id(id)
                        .map(ProfileId::from_bytes)
                        .map_err(|_| VaultError::InvalidInput)
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
            if ids.len() != request.selected_profiles.len() {
                return Err(VaultError::InvalidInput);
            }
            let context = export_context(&snapshot, &ids)?;
            let targets: Vec<_> = context
                .profiles
                .iter()
                .filter(|p| ids.contains(&p.target.profile_id))
                .map(|p| p.target.clone())
                .collect();
            let context =
                Zeroizing::new(serde_json::to_vec(&context).map_err(|_| VaultError::InvalidInput)?);
            let controller = self.vault.clone();
            let revision = request.expected_vault_revision;
            let generation = request.expected_lock_generation;
            let include_keys = request.include_private_keys;
            let bytes = tokio::task::spawn_blocking(move || {
                let _derivation = derivation;
                let records =
                    controller.export_records(&targets, include_keys, revision, generation)?;
                let bytes = CredentialPackage::seal(&context, &records, &password)?;
                controller.commit_auth(generation, || bytes)
            })
            .await
            .map_err(|_| VaultError::Io(std::io::ErrorKind::Other))??;
            return Ok(VaultResponse {
                status: Status::Ok as i32,
                package: bytes,
                ..Default::default()
            });
        }
        if !request.selected_profiles.is_empty() || request.include_private_keys {
            return Err(VaultError::InvalidInput);
        }
        let (bytes, policy, profile_revision, vault_revision, generation) =
            if operation == Op::PreviewPackage {
                if !request.preview_token.is_empty()
                    || request.package.is_empty()
                    || request.package.len() > MAX_CREDENTIAL_PACKAGE_BYTES
                {
                    return Err(VaultError::InvalidInput);
                }
                (
                    std::mem::take(&mut request.package),
                    policy,
                    request.expected_profile_revision,
                    request.expected_vault_revision,
                    request.expected_lock_generation,
                )
            } else if operation == Op::CommitPackage {
                if !request.package.is_empty() {
                    return Err(VaultError::InvalidInput);
                }
                let token: [u8; 32] = request
                    .preview_token
                    .as_slice()
                    .try_into()
                    .map_err(|_| VaultError::InvalidInput)?;
                // Consume before any validation/derivation: a failed confirmation is never replayed.
                let lease = self
                    .packages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
                    .remove(&token)
                    .ok_or(VaultError::AlreadyExists)?;
                request.preview_token.zeroize();
                if lease.expires <= Instant::now()
                    || policy != lease.policy
                    || request.expected_profile_revision != lease.profile_revision
                    || request.expected_vault_revision != lease.vault_revision
                    || request.expected_lock_generation != lease.generation
                {
                    return Err(VaultError::AlreadyExists);
                }
                (
                    lease.bytes,
                    lease.policy,
                    lease.profile_revision,
                    lease.vault_revision,
                    lease.generation,
                )
            } else {
                return Err(VaultError::InvalidInput);
            };
        let retained = (operation == Op::PreviewPackage).then(|| bytes.clone());
        let (package, derivation) = tokio::task::spawn_blocking(move || {
            let package = CredentialPackage::open(&bytes, &password)?;
            Ok::<_, VaultError>((package, derivation))
        })
        .await
        .map_err(|_| VaultError::Io(std::io::ErrorKind::Other))??;
        // Hold the catalog mutation gate through the final Vault transaction.
        let mutation = self.catalog_mutation.clone().lock_owned().await;
        let snapshot = self.package_snapshot(profile_revision).await?;
        let prepared = tokio::task::spawn_blocking(move || {
            let _derivation = derivation;
            prepare(&package, &snapshot)
        })
        .await
        .map_err(|_| VaultError::Io(std::io::ErrorKind::Other))??;
        let controller = self.vault.clone();
        let ids: Vec<_> = prepared
            .items
            .iter()
            .map(|item| {
                decode_id(&item.profile_id)
                    .map(ProfileId::from_bytes)
                    .map_err(|_| VaultError::InvalidInput)
            })
            .collect::<Result<_, _>>()?;
        let actions = tokio::task::spawn_blocking(move || {
            controller.package_actions(&ids, policy, vault_revision, generation)
        })
        .await
        .map_err(|_| VaultError::Io(std::io::ErrorKind::Other))??;
        if operation == Op::CommitPackage {
            let controller = self.vault.clone();
            tokio::task::spawn_blocking(move || {
                let _mutation = mutation;
                controller.restore_package(&prepared.records, policy, vault_revision, generation)
            })
            .await
            .map_err(|_| VaultError::Io(std::io::ErrorKind::Other))??;
            return Ok(VaultResponse {
                status: Status::Ok as i32,
                ..Default::default()
            });
        }
        let mut items = prepared.items;
        for (item, action) in items.iter_mut().zip(&actions) {
            item.action = *action as i32;
            item.reason = match action {
                Action::Conflict => {
                    "Destination already has a credential group; choose Skip or Replace"
                }
                Action::Skip => "Keep the entire destination credential group",
                Action::Replace => "Replace the entire credential group and select encrypted Vault",
                _ => "Restore into encrypted Vault",
            }
            .into();
        }
        let can_commit = !actions.contains(&Action::Conflict);
        let mut preview = CredentialPackagePreview {
            token: Vec::new(),
            profile_revision,
            vault_revision,
            lock_generation: generation,
            can_commit,
            items,
        };
        if can_commit {
            let token: [u8; 32] = rand::random();
            let mut leases = self
                .packages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            leases.0.retain(|_, lease| {
                lease.expires > Instant::now()
                    && lease.generation == generation
                    && lease.profile_revision == profile_revision
                    && lease.vault_revision == vault_revision
            });
            if leases.0.len() >= 4 {
                return Err(VaultError::Busy);
            }
            leases.0.insert(
                token,
                Lease {
                    bytes: retained.ok_or(VaultError::InvalidInput)?,
                    policy,
                    profile_revision,
                    vault_revision,
                    generation,
                    expires: Instant::now() + LEASE_LIFETIME,
                },
            );
            preview.token = token.to_vec();
        }
        Ok(VaultResponse {
            status: Status::Ok as i32,
            package_preview: Some(preview),
            ..Default::default()
        })
    }
    async fn package_snapshot(&self, revision: u64) -> Result<CatalogSnapshot, VaultError> {
        let snapshot = self
            .service
            .load()
            .await
            .map_err(|_| VaultError::Io(std::io::ErrorKind::Other))?
            .snapshot();
        if snapshot.revision != revision {
            return Err(VaultError::AlreadyExists);
        }
        Ok(snapshot)
    }
}

fn folder_path(snapshot: &CatalogSnapshot, id: ProfileId) -> Result<Vec<String>, VaultError> {
    let profile = snapshot
        .profiles
        .iter()
        .find(|p| p.id == id)
        .ok_or(VaultError::InvalidInput)?;
    let mut next = profile.folder_id;
    let mut path = Vec::new();
    while let Some(id) = next {
        if path.len() >= 32 {
            return Err(VaultError::Capacity);
        }
        let folder = snapshot
            .folders
            .iter()
            .find(|f| f.id == id)
            .ok_or(VaultError::InvalidInput)?;
        path.push(folder.name.clone());
        next = folder.parent_id;
    }
    path.reverse();
    Ok(path)
}
fn export_context(
    snapshot: &CatalogSnapshot,
    selected: &BTreeSet<ProfileId>,
) -> Result<Context, VaultError> {
    let mut needed = selected.clone();
    let mut pending: Vec<_> = selected.iter().copied().collect();
    while let Some(id) = pending.pop() {
        let target = snapshot
            .ssh_connections
            .iter()
            .find(|t| t.profile_id == id)
            .ok_or(VaultError::InvalidInput)?;
        if let SshRoute::Jump { profile_id } = target.route
            && needed.insert(profile_id)
        {
            pending.push(profile_id);
        }
        if needed.len() > 64 {
            return Err(VaultError::Capacity);
        }
    }
    let profiles = needed
        .into_iter()
        .map(|id| {
            let profile = snapshot
                .profiles
                .iter()
                .find(|p| p.id == id)
                .ok_or(VaultError::InvalidInput)?;
            let target = snapshot
                .ssh_connections
                .iter()
                .find(|t| t.profile_id == id)
                .ok_or(VaultError::InvalidInput)?
                .clone();
            Ok(PackageProfile {
                name: profile.name.clone(),
                folder: folder_path(snapshot, id)?,
                target,
            })
        })
        .collect::<Result<_, VaultError>>()?;
    Ok(Context {
        version: 1,
        profiles,
    })
}
fn prepare(
    package: &CredentialPackage,
    snapshot: &CatalogSnapshot,
) -> Result<Prepared, VaultError> {
    prepare_records(package.context(), package.records(), snapshot)
}
fn prepare_records(
    context: &[u8],
    credentials: &[CredentialPackageRecord],
    snapshot: &CatalogSnapshot,
) -> Result<Prepared, VaultError> {
    let context: Context = serde_json::from_slice(context).map_err(|_| VaultError::InvalidInput)?;
    if context.version != 1 || context.profiles.is_empty() || context.profiles.len() > 64 {
        return Err(VaultError::InvalidInput);
    }
    let mut mapping = BTreeMap::new();
    let mut destinations = BTreeSet::new();
    for source in &context.profiles {
        if source.name.is_empty()
            || source.name.len() > 256
            || source.folder.len() > 32
            || source
                .folder
                .iter()
                .any(|name| name.is_empty() || name.len() > 256)
        {
            return Err(VaultError::InvalidInput);
        }
        let id = if snapshot
            .ssh_connections
            .iter()
            .any(|t| t.profile_id == source.target.profile_id)
        {
            source.target.profile_id
        } else {
            let matches: Vec<_> = snapshot
                .profiles
                .iter()
                .filter(|p| {
                    p.name == source.name
                        && folder_path(snapshot, p.id).is_ok_and(|path| path == source.folder)
                        && snapshot
                            .ssh_connections
                            .iter()
                            .any(|t| t.profile_id == p.id)
                })
                .collect();
            if matches.len() != 1 {
                return Err(VaultError::AlreadyExists);
            }
            matches[0].id
        };
        if mapping.insert(source.target.profile_id, id).is_some() || !destinations.insert(id) {
            return Err(VaultError::InvalidInput);
        }
    }
    for source in &context.profiles {
        let mut target = source.target.clone();
        target.profile_id = mapping[&target.profile_id];
        if let SshRoute::Jump { profile_id } = &mut target.route {
            *profile_id = *mapping.get(profile_id).ok_or(VaultError::InvalidInput)?;
        }
        if !snapshot.ssh_connections.contains(&target) {
            return Err(VaultError::AlreadyExists);
        }
    }
    let mut records = Vec::new();
    let mut items: BTreeMap<ProfileId, CredentialPackageItem> = BTreeMap::new();
    for record in credentials {
        let source_id = ProfileId::from_bytes(record.reference.profile_id);
        let source = context
            .profiles
            .iter()
            .find(|p| p.target.profile_id == source_id)
            .ok_or(VaultError::InvalidInput)?;
        if binding(&source.target, record.reference.kind)? != record.binding() {
            return Err(VaultError::InvalidInput);
        }
        let id = mapping[&source_id];
        let target = snapshot
            .ssh_connections
            .iter()
            .find(|t| t.profile_id == id)
            .ok_or(VaultError::InvalidInput)?;
        let valid_kind = match record.reference.kind {
            Kind::Password => target.uses_password(),
            Kind::PrivateKey | Kind::KeyPassphrase => target.uses_private_key(),
            Kind::ProfileMode | Kind::ManagedKey | Kind::VaultSettings => false,
        };
        if !valid_kind {
            return Err(VaultError::InvalidInput);
        }
        if record.reference.kind == Kind::PrivateKey {
            let encoded = Zeroizing::new(
                std::str::from_utf8(record.secret.expose())
                    .map_err(|_| VaultError::InvalidInput)?
                    .to_owned(),
            );
            let phrase = credentials
                .iter()
                .find(|r| r.reference == reference(source_id, Kind::KeyPassphrase))
                .map(|r| {
                    std::str::from_utf8(r.secret.expose()).map_err(|_| VaultError::InvalidInput)
                })
                .transpose()?;
            cshell_ssh::SshPrivateKey::decode_private_key(&encoded, phrase)
                .map_err(|_| VaultError::InvalidInput)?;
        }
        records.push(CredentialPackageRecord::new(
            reference(id, record.reference.kind),
            binding(target, record.reference.kind)?,
            Secret::new(record.secret.expose().to_vec()),
        ));
        let item = items.entry(id).or_insert_with(|| CredentialPackageItem {
            profile_id: id.as_uuid().as_bytes().to_vec(),
            name: snapshot
                .profiles
                .iter()
                .find(|p| p.id == id)
                .map_or_else(String::new, |p| p.name.clone()),
            target: format!("{}@{}:{}", target.username, target.host, target.port),
            ..Default::default()
        });
        item.kinds.push(record.reference.kind as u32);
    }
    if items.is_empty() || items.len() > MAX_CREDENTIAL_PACKAGE_PROFILES {
        return Err(VaultError::Capacity);
    }
    Ok(Prepared {
        records,
        items: items.into_values().collect(),
    })
}

#[cfg(test)]
mod tests;
