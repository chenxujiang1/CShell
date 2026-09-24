use cshell_application::{
    CatalogChange, CatalogError, CatalogSnapshot, ImportAction, ImportConflictPolicy,
    ImportItemKind, ProfileImportError, ProfileImportPreview, ProfileRepositoryError,
    ProfileService, ProfileServiceError,
};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileRecord, SshAuthMethod, SshConnectionRecord,
};
use cshell_ipc::{
    HostKeyPreviewData, MAX_PROFILE_CONTROL_CHANGES, ProfileCatalogData, ProfileChange,
    ProfileCodecError, ProfileDefaultsData, ProfileFolderData, ProfileImportAction,
    ProfileImportItemData, ProfileImportItemKind, ProfileImportPolicy, ProfileImportPreviewData,
    ProfileOperation, ProfileRecordData, ProfileRequest, ProfileResponse, ProfileStatus,
    SshConnectionData, decode_id, profile_change,
};
use cshell_ssh::{KnownHostsVerifier, import_confirmed_host_key, scan_host_key};
use cshell_storage::SqliteProfileRepository;
use cshell_vault::{
    ProfileKeyPassphraseRef, ProfilePasswordBinding, ProfilePasswordRef, Secret,
    SystemProfileKeyPassphraseVault, SystemProfilePasswordVault,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct ProfileIpcService {
    service: ProfileService<SqliteProfileRepository>,
    known_hosts_path: Option<PathBuf>,
    pending_host_keys: Mutex<HashMap<[u8; 16], PendingHostKey>>,
}

#[derive(Debug)]
struct PendingHostKey {
    target: SshConnectionRecord,
    revision: u64,
    public_key_line: String,
    fingerprint: String,
    token: [u8; 32],
    expires: Instant,
}

impl ProfileIpcService {
    pub fn new(repository: SqliteProfileRepository) -> Self {
        Self {
            service: ProfileService::new(repository),
            known_hosts_path: None,
            pending_host_keys: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_known_hosts_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts_path = Some(path.into());
        self
    }

    fn known_hosts_path(&self) -> Result<PathBuf, (ProfileStatus, String)> {
        self.known_hosts_path
            .clone()
            .map(Ok)
            .unwrap_or_else(crate::session_ipc::default_known_hosts_path)
            .map_err(|error| (ProfileStatus::Unavailable, error))
    }

    async fn host_key_target(
        &self,
        id: ProfileId,
        revision: u64,
    ) -> Result<SshConnectionRecord, (ProfileStatus, String)> {
        let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
        if snapshot.revision != revision {
            return Err((
                ProfileStatus::Conflict,
                "Profile catalog changed; refresh and preview again".into(),
            ));
        }
        if !snapshot
            .profiles
            .iter()
            .any(|profile| profile.id == id && profile.kind == cshell_domain::ProfileKind::Ssh)
        {
            return Err(invalid("saved SSH Profile does not exist"));
        }
        snapshot
            .ssh_connections
            .into_iter()
            .find(|target| target.profile_id == id)
            .ok_or_else(|| invalid("saved SSH Profile has no target"))
    }

    pub async fn ssh_target(&self, id: ProfileId) -> Result<(String, SshConnectionRecord), String> {
        let snapshot = self
            .service
            .load()
            .await
            .map_err(|error| error.to_string())?
            .snapshot();
        let profile = snapshot
            .profiles
            .iter()
            .find(|profile| profile.id == id && profile.kind == cshell_domain::ProfileKind::Ssh)
            .ok_or("saved SSH Profile does not exist")?;
        let target = snapshot
            .ssh_connections
            .into_iter()
            .find(|connection| connection.profile_id == id)
            .ok_or("saved SSH Profile has no target")?;
        Ok((profile.name.clone(), target))
    }

    pub async fn handle(&self, request: ProfileRequest) -> ProfileResponse {
        match self.handle_inner(request).await {
            Ok(response) => response,
            Err((status, detail)) => ProfileResponse {
                status: status as i32,
                revision: 0,
                catalog: None,
                preview: None,
                detail,
                host_key_preview: None,
            },
        }
    }

    async fn handle_inner(
        &self,
        request: ProfileRequest,
    ) -> Result<ProfileResponse, (ProfileStatus, String)> {
        let operation = ProfileOperation::try_from(request.operation)
            .map_err(|_| invalid("unknown Profile operation"))?;
        match operation {
            ProfileOperation::List => {
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                Ok(catalog_response(snapshot))
            }
            ProfileOperation::ApplyChanges => {
                if request.changes.len() > MAX_PROFILE_CONTROL_CHANGES {
                    return Err(invalid("too many Profile changes"));
                }
                let changes = request
                    .changes
                    .into_iter()
                    .map(decode_change)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| invalid(error.to_string()))?;
                self.service
                    .apply_batch(request.expected_revision, &changes)
                    .await
                    .map_err(service_error)?;
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                Ok(catalog_response(snapshot))
            }
            ProfileOperation::PreviewImport => {
                let policy = decode_policy(request.import_policy)?;
                let preview = self
                    .service
                    .preview_import(&request.import_json, policy)
                    .await
                    .map_err(import_error)?;
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: preview.base_revision,
                    catalog: None,
                    preview: Some(preview_data(preview)),
                    detail: String::new(),
                    host_key_preview: None,
                })
            }
            ProfileOperation::SetPassword | ProfileOperation::DeletePassword => {
                let id = ProfileId::from_bytes(
                    decode_id(&request.credential_profile_id)
                        .map_err(|error| invalid(error.to_string()))?,
                );
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                if snapshot.revision != request.expected_revision {
                    return Err((
                        ProfileStatus::Conflict,
                        "Profile catalog changed; refresh before editing credentials".into(),
                    ));
                }
                if !snapshot.profiles.iter().any(|profile| {
                    profile.id == id && profile.kind == cshell_domain::ProfileKind::Ssh
                }) || !snapshot
                    .ssh_connections
                    .iter()
                    .any(|connection| connection.profile_id == id)
                {
                    return Err(invalid("SSH Profile target does not exist"));
                }
                let target = snapshot
                    .ssh_connections
                    .iter()
                    .find(|connection| connection.profile_id == id)
                    .ok_or_else(|| invalid("SSH Profile target does not exist"))?;
                let binding = ProfilePasswordBinding {
                    host: target.host.clone(),
                    port: target.port,
                    username: target.username.clone(),
                };
                let reference = ProfilePasswordRef::from_profile_bytes(*id.as_uuid().as_bytes());
                let vault = SystemProfilePasswordVault::new();
                let result = if operation == ProfileOperation::SetPassword {
                    if request.credential_secret.is_empty()
                        || request.credential_secret.len() > 4096
                    {
                        return Err(invalid("password must contain 1 to 4096 bytes"));
                    }
                    let secret = Secret::new(request.credential_secret);
                    tokio::task::spawn_blocking(move || vault.write(&reference, &binding, &secret))
                        .await
                } else {
                    if !request.credential_secret.is_empty() {
                        return Err(invalid("delete password must not contain a secret"));
                    }
                    tokio::task::spawn_blocking(move || vault.delete(&reference)).await
                };
                result
                    .map_err(|_| {
                        (
                            ProfileStatus::Unavailable,
                            "credential vault task failed".into(),
                        )
                    })?
                    .map_err(|error| {
                        (
                            ProfileStatus::Unavailable,
                            format!("system keychain: {error:?}"),
                        )
                    })?;
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: snapshot.revision,
                    catalog: None,
                    preview: None,
                    detail: String::new(),
                    host_key_preview: None,
                })
            }
            ProfileOperation::SetKeyPassphrase | ProfileOperation::DeleteKeyPassphrase => {
                let id = ProfileId::from_bytes(
                    decode_id(&request.credential_profile_id)
                        .map_err(|error| invalid(error.to_string()))?,
                );
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                if snapshot.revision != request.expected_revision {
                    return Err((
                        ProfileStatus::Conflict,
                        "Profile catalog changed; refresh before editing credentials".into(),
                    ));
                }
                let profile = snapshot
                    .profiles
                    .iter()
                    .find(|profile| {
                        profile.id == id && profile.kind == cshell_domain::ProfileKind::Ssh
                    })
                    .ok_or_else(|| invalid("SSH Profile does not exist"))?;
                let target = snapshot
                    .ssh_connections
                    .iter()
                    .find(|connection| connection.profile_id == profile.id)
                    .ok_or_else(|| invalid("SSH Profile target does not exist"))?;
                if !matches!(
                    target.auth_method,
                    SshAuthMethod::PrivateKey | SshAuthMethod::Certificate
                ) {
                    return Err(invalid("SSH Profile does not use a private key"));
                }
                let key_path = target
                    .private_key_path
                    .clone()
                    .ok_or_else(|| invalid("SSH Profile has no private key reference"))?;
                let reference =
                    ProfileKeyPassphraseRef::from_profile_bytes(*id.as_uuid().as_bytes());
                let vault = SystemProfileKeyPassphraseVault::new();
                let result = if operation == ProfileOperation::SetKeyPassphrase {
                    if request.credential_secret.is_empty()
                        || request.credential_secret.len() > 4096
                    {
                        return Err(invalid("key passphrase must contain 1 to 4096 bytes"));
                    }
                    let secret = Secret::new(request.credential_secret);
                    tokio::task::spawn_blocking(move || vault.write(&reference, &key_path, &secret))
                        .await
                } else {
                    if !request.credential_secret.is_empty() {
                        return Err(invalid("delete key passphrase must not contain a secret"));
                    }
                    tokio::task::spawn_blocking(move || vault.delete(&reference)).await
                };
                result
                    .map_err(|_| {
                        (
                            ProfileStatus::Unavailable,
                            "credential vault task failed".into(),
                        )
                    })?
                    .map_err(|error| {
                        (
                            ProfileStatus::Unavailable,
                            format!("system keychain: {error:?}"),
                        )
                    })?;
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: snapshot.revision,
                    catalog: None,
                    preview: None,
                    detail: String::new(),
                    host_key_preview: None,
                })
            }
            ProfileOperation::CommitImport => {
                let policy = decode_policy(request.import_policy)?;
                self.service
                    .commit_import(request.expected_revision, &request.import_json, policy)
                    .await
                    .map_err(import_error)?;
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                Ok(catalog_response(snapshot))
            }
            ProfileOperation::PreviewHostKey => {
                let id_bytes = decode_id(&request.credential_profile_id)
                    .map_err(|error| invalid(error.to_string()))?;
                let id = ProfileId::from_bytes(id_bytes);
                let target = self.host_key_target(id, request.expected_revision).await?;
                let path = self.known_hosts_path()?;
                let verifier = KnownHostsVerifier::load_or_empty(&path, &target.host, target.port)
                    .map_err(|error| (ProfileStatus::Corrupt, error.to_string()))?;
                if verifier.has_matching_entries() {
                    return Err((
                        ProfileStatus::Conflict,
                        "host already has a known_hosts entry; first-key import is blocked".into(),
                    ));
                }
                let scanned = scan_host_key(&target.host, target.port)
                    .await
                    .map_err(|error| (ProfileStatus::Unavailable, error))?;
                let token: [u8; 32] = rand::random();
                let expires_unix_seconds = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| (ProfileStatus::Unavailable, error.to_string()))?
                    .as_secs()
                    + 300;
                let preview = HostKeyPreviewData {
                    profile_id: id_bytes.to_vec(),
                    host: target.host.clone(),
                    port: target.port.into(),
                    algorithm: scanned.algorithm,
                    fingerprint: scanned.fingerprint.clone(),
                    public_key_line: scanned.public_key_line.clone(),
                    token: token.to_vec(),
                    expires_unix_seconds,
                };
                let pending = PendingHostKey {
                    target,
                    revision: request.expected_revision,
                    public_key_line: scanned.public_key_line,
                    fingerprint: scanned.fingerprint,
                    token,
                    expires: Instant::now() + Duration::from_secs(300),
                };
                self.pending_host_keys
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id_bytes, pending);
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: request.expected_revision,
                    host_key_preview: Some(Box::new(preview)),
                    ..ProfileResponse::default()
                })
            }
            ProfileOperation::ConfirmHostKey => {
                let id_bytes = decode_id(&request.credential_profile_id)
                    .map_err(|error| invalid(error.to_string()))?;
                let id = ProfileId::from_bytes(id_bytes);
                let pending = {
                    let mut previews = self
                        .pending_host_keys
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let preview = previews
                        .get(&id_bytes)
                        .ok_or_else(|| invalid("preview the host key again"))?;
                    if preview.expires <= Instant::now()
                        || preview.revision != request.expected_revision
                        || request.host_key_token.as_slice() != preview.token
                        || request.host_key_fingerprint != preview.fingerprint
                    {
                        return Err(invalid(
                            "host-key preview expired or fingerprint confirmation did not match",
                        ));
                    }
                    previews
                        .remove(&id_bytes)
                        .ok_or_else(|| invalid("preview the host key again"))?
                };
                let target = self.host_key_target(id, request.expected_revision).await?;
                if target.host != pending.target.host || target.port != pending.target.port {
                    return Err((
                        ProfileStatus::Conflict,
                        "SSH target changed; preview again".into(),
                    ));
                }
                let scanned = scan_host_key(&target.host, target.port)
                    .await
                    .map_err(|error| (ProfileStatus::Unavailable, error))?;
                if scanned.public_key_line != pending.public_key_line
                    || scanned.fingerprint != pending.fingerprint
                {
                    return Err((
                        ProfileStatus::Conflict,
                        "server host key changed after preview; import blocked".into(),
                    ));
                }
                let path = self.known_hosts_path()?;
                let profile_id = id.as_uuid().to_string();
                tokio::task::spawn_blocking(move || {
                    import_confirmed_host_key(
                        &path,
                        &target.host,
                        target.port,
                        &scanned.public_key_line,
                        &pending.fingerprint,
                        &profile_id,
                    )
                })
                .await
                .map_err(|_| {
                    (
                        ProfileStatus::Unavailable,
                        "host-key import task failed".into(),
                    )
                })?
                .map_err(|error| (ProfileStatus::Conflict, error))?;
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: request.expected_revision,
                    detail: "Host key imported into known_hosts with an audit record".into(),
                    ..ProfileResponse::default()
                })
            }
        }
    }
}

fn decode_policy(raw: i32) -> Result<ImportConflictPolicy, (ProfileStatus, String)> {
    match ProfileImportPolicy::try_from(raw)
        .map_err(|_| invalid("unknown import conflict policy"))?
    {
        ProfileImportPolicy::Fail => Ok(ImportConflictPolicy::Fail),
        ProfileImportPolicy::Skip => Ok(ImportConflictPolicy::Skip),
        ProfileImportPolicy::Replace => Ok(ImportConflictPolicy::Replace),
    }
}

fn decode_change(change: ProfileChange) -> Result<CatalogChange, ProfileCodecError> {
    match change.change.ok_or(ProfileCodecError::MissingChange)? {
        profile_change::Change::UpsertFolder(value) => {
            Ok(CatalogChange::UpsertFolder(ProfileFolder::try_from(value)?))
        }
        profile_change::Change::RemoveFolder(value) => Ok(CatalogChange::RemoveFolder(
            FolderId::from_bytes(decode_id(&value)?),
        )),
        profile_change::Change::UpsertProfile(value) => Ok(CatalogChange::UpsertProfile(
            ProfileRecord::try_from(value)?,
        )),
        profile_change::Change::RemoveProfile(value) => Ok(CatalogChange::RemoveProfile(
            ProfileId::from_bytes(decode_id(&value)?),
        )),
        profile_change::Change::UpsertSshConnection(value) => Ok(
            CatalogChange::UpsertSshConnection(SshConnectionRecord::try_from(value)?),
        ),
        profile_change::Change::RemoveSshConnection(value) => Ok(
            CatalogChange::RemoveSshConnection(ProfileId::from_bytes(decode_id(&value)?)),
        ),
    }
}

fn catalog_response(snapshot: CatalogSnapshot) -> ProfileResponse {
    ProfileResponse {
        status: ProfileStatus::Ok as i32,
        revision: snapshot.revision,
        catalog: Some(ProfileCatalogData {
            revision: snapshot.revision,
            defaults: Some(ProfileDefaultsData::from(&snapshot.defaults)),
            folders: snapshot
                .folders
                .iter()
                .map(ProfileFolderData::from)
                .collect(),
            profiles: snapshot
                .profiles
                .iter()
                .map(ProfileRecordData::from)
                .collect(),
            ssh_connections: snapshot
                .ssh_connections
                .iter()
                .map(SshConnectionData::from)
                .collect(),
        }),
        preview: None,
        detail: String::new(),
        host_key_preview: None,
    }
}

fn preview_data(preview: ProfileImportPreview) -> ProfileImportPreviewData {
    ProfileImportPreviewData {
        base_revision: preview.base_revision,
        items: preview
            .items
            .into_iter()
            .map(|item| ProfileImportItemData {
                kind: match item.kind {
                    ImportItemKind::Folder => ProfileImportItemKind::Folder,
                    ImportItemKind::Profile => ProfileImportItemKind::Profile,
                } as i32,
                name: item.name,
                action: match item.action {
                    ImportAction::Create => ProfileImportAction::Create,
                    ImportAction::Skip => ProfileImportAction::Skip,
                    ImportAction::Replace => ProfileImportAction::Replace,
                    ImportAction::Conflict => ProfileImportAction::Conflict,
                } as i32,
            })
            .collect(),
        can_commit: preview.can_commit,
        change_count: u32::try_from(preview.change_count).unwrap_or(u32::MAX),
    }
}

fn invalid(detail: impl Into<String>) -> (ProfileStatus, String) {
    (ProfileStatus::Invalid, detail.into())
}

fn service_error(error: ProfileServiceError) -> (ProfileStatus, String) {
    let status = match &error {
        ProfileServiceError::Catalog(CatalogError::StaleRevision { .. })
        | ProfileServiceError::Repository(ProfileRepositoryError::Conflict) => {
            ProfileStatus::Conflict
        }
        ProfileServiceError::Catalog(_) => ProfileStatus::Invalid,
        ProfileServiceError::Repository(ProfileRepositoryError::Corrupt) => ProfileStatus::Corrupt,
        ProfileServiceError::Repository(ProfileRepositoryError::Unavailable) => {
            ProfileStatus::Unavailable
        }
    };
    (status, error.to_string())
}

fn import_error(error: ProfileImportError) -> (ProfileStatus, String) {
    let status = match &error {
        ProfileImportError::UnsupportedFormat => ProfileStatus::Unsupported,
        ProfileImportError::Conflicts
        | ProfileImportError::Catalog(CatalogError::StaleRevision { .. }) => {
            ProfileStatus::Conflict
        }
        ProfileImportError::Service(service) => {
            return service_error(match service {
                ProfileServiceError::Catalog(catalog) => {
                    ProfileServiceError::Catalog(catalog.clone())
                }
                ProfileServiceError::Repository(repository) => {
                    ProfileServiceError::Repository(*repository)
                }
            });
        }
        _ => ProfileStatus::Invalid,
    };
    (status, error.to_string())
}
