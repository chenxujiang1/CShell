use cshell_application::{
    CatalogChange, CatalogError, CatalogSnapshot, ImportAction, ImportConflictPolicy,
    ImportItemKind, ProfileImportError, ProfileImportPreview, ProfileRepositoryError,
    ProfileService, ProfileServiceError,
};
use cshell_domain::{FolderId, ProfileFolder, ProfileId, ProfileRecord, SshConnectionRecord};
use cshell_ipc::{
    MAX_PROFILE_CONTROL_CHANGES, ProfileCatalogData, ProfileChange, ProfileCodecError,
    ProfileDefaultsData, ProfileFolderData, ProfileImportAction, ProfileImportItemData,
    ProfileImportItemKind, ProfileImportPolicy, ProfileImportPreviewData, ProfileOperation,
    ProfileRecordData, ProfileRequest, ProfileResponse, ProfileStatus, SshConnectionData,
    decode_id, profile_change,
};
use cshell_storage::SqliteProfileRepository;
use cshell_vault::{
    ProfilePasswordBinding, ProfilePasswordRef, Secret, SystemProfilePasswordVault,
};

#[derive(Debug)]
pub struct ProfileIpcService {
    service: ProfileService<SqliteProfileRepository>,
}

impl ProfileIpcService {
    pub fn new(repository: SqliteProfileRepository) -> Self {
        Self {
            service: ProfileService::new(repository),
        }
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
