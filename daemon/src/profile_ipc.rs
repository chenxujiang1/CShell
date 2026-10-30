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
use cshell_ssh::import_confirmed_host_key;
use cshell_storage::SqliteProfileRepository;
use cshell_vault::{Secret, VaultCredentialKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroize;

pub(crate) enum SavedSessionPlan {
    Ssh(Box<crate::ssh_route::SshConnectionPlan>),
    Local(cshell_local::LocalProfile, cshell_domain::LocalClosePolicy),
}

#[derive(Debug)]
pub struct ProfileIpcService {
    pub(crate) service: ProfileService<SqliteProfileRepository>,
    pub(crate) vault: std::sync::Arc<crate::vault_control::VaultController>,
    known_hosts_path: Option<PathBuf>,
    pub(crate) catalog_mutation: std::sync::Arc<tokio::sync::Mutex<()>>,
    pub(crate) packages: Mutex<crate::credential_package::PackageLeases>,
    pub(crate) package_derivations: std::sync::Arc<tokio::sync::Semaphore>,
    pending_host_keys: Mutex<HashMap<[u8; 16], PendingHostKey>>,
}

struct PendingHostKey {
    replacement: Option<cshell_ssh::HostKeyUpdate>,
    target: SshConnectionRecord,
    revision: u64,
    public_key_line: String,
    fingerprint: String,
    token: [u8; 32],
    expires: Instant,
}

impl std::fmt::Debug for PendingHostKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingHostKey")
            .field("revision", &self.revision)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl Drop for PendingHostKey {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

impl ProfileIpcService {
    pub(crate) async fn load_clipboard_policy(
        &self,
    ) -> Result<cshell_application::ClipboardPolicySnapshot, cshell_application::ClipboardPolicyError>
    {
        use cshell_application::ClipboardPolicyRepository;
        self.service.repository().load_clipboard_policy().await
    }
    pub(crate) async fn save_clipboard_policy(
        &self,
        revision: u64,
        document: &cshell_application::ClipboardPolicyDocument,
    ) -> Result<cshell_application::ClipboardPolicySnapshot, cshell_application::ClipboardPolicyError>
    {
        use cshell_application::ClipboardPolicyRepository;
        self.service
            .repository()
            .save_clipboard_policy(revision, document)
            .await
    }
    pub fn new(repository: SqliteProfileRepository) -> Self {
        let path = repository
            .path()
            .with_file_name("credentials")
            .join("master.vault");
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir().map_or_else(|_| path.clone(), |root| root.join(&path))
        };
        let vault = crate::vault_control::VaultController::new(path);
        Self {
            vault,
            service: ProfileService::new(repository),
            known_hosts_path: None,
            catalog_mutation: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            packages: Mutex::new(crate::credential_package::PackageLeases::default()),
            package_derivations: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            pending_host_keys: Mutex::new(HashMap::new()),
        }
    }

    /// Own this guard for the lifetime of the production daemon. Test services do
    /// not attach to the user's native session implicitly.
    pub fn start_session_lock_monitor(&self) -> std::io::Result<crate::SessionLockMonitor> {
        crate::session_lock::SessionLockMonitor::start(&self.vault)
    }

    #[must_use]
    pub fn with_known_hosts_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts_path = Some(path.into());
        self
    }

    pub(crate) fn known_hosts_path(&self) -> Result<PathBuf, (ProfileStatus, String)> {
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
    ) -> Result<crate::ssh_route::SshConnectionPlan, (ProfileStatus, String)> {
        let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
        if snapshot.revision != revision {
            return Err((
                ProfileStatus::Conflict,
                "Profile catalog changed; refresh and preview again".into(),
            ));
        }
        plan_from_snapshot(&snapshot, id).map_err(invalid)
    }

    pub(crate) async fn session_plan(&self, id: ProfileId) -> Result<SavedSessionPlan, String> {
        let snapshot = self
            .service
            .load()
            .await
            .map_err(|error| error.to_string())?
            .snapshot();
        Self::session_plan_from_snapshot(&snapshot, id)
    }

    /// Capture terminal context and connection configuration together before any
    /// credential read or network/PTY launch. Later edits do not replace this plan.
    pub(crate) async fn session_plan_context(
        &self,
        id: ProfileId,
        review: Option<&cshell_application::TerminalResolutionReport>,
    ) -> Result<
        (
            SavedSessionPlan,
            cshell_application::TerminalResolutionReport,
        ),
        String,
    > {
        use cshell_application::{
            HostCatalogSnapshot, HostRepository, TerminalResolutionContext,
            TerminalResolutionReport, resolve_terminal_context,
        };
        self.vault
            .ensure_owned()
            .map_err(|_| "credential storage is owned by another daemon")?;
        let _gate = self.catalog_mutation.lock().await;
        let snapshot = self
            .service
            .load()
            .await
            .map_err(|_| "Profile storage unavailable")?
            .snapshot();
        let hosts = self
            .service
            .repository()
            .load_hosts()
            .await
            .map_err(|_| "Host storage unavailable")?
            .unwrap_or(HostCatalogSnapshot {
                revision: 0,
                catalog: Default::default(),
            });
        cshell_application::validate_hosts(&hosts.catalog).map_err(|_| "Host catalog invalid")?;
        let policies = crate::terminal_policy_store::read().await?;
        let context = review.map_or(
            TerminalResolutionContext {
                profile_id: id,
                host_id: None,
                temporary: Default::default(),
            },
            |r| r.context.clone(),
        );
        if context.profile_id != id
            || review.is_some_and(|r| {
                r.profile_revision != snapshot.revision
                    || r.host_revision != hosts.revision
                    || r.policy_revision != policies.revision
            })
        {
            return Err("terminal context revision changed; resolve again before launching".into());
        }
        let profile = snapshot
            .profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or("Profile context missing")?
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
                    .ok_or("Host context missing")
            })
            .transpose()?;
        let captured = snapshot.clone();
        let resolution_context = context.clone();
        let selected_host = host.clone();
        let captured_policies = policies.clone();
        let (settings, matched_tags) = tokio::task::spawn_blocking(move || {
            resolve_terminal_context(
                &captured,
                &captured_policies,
                &resolution_context,
                selected_host.as_ref(),
            )
        })
        .await
        .map_err(|_| "terminal context worker failed")??;
        if !matches!(settings.theme.value.as_str(), "default" | "dark" | "light")
            && !snapshot
                .themes
                .iter()
                .any(|t| t.name == settings.theme.value)
        {
            return Err("terminal context references an unknown theme".into());
        }
        let report = TerminalResolutionReport {
            profile_revision: snapshot.revision,
            host_revision: hosts.revision,
            policy_revision: policies.revision,
            context,
            profile,
            host,
            matched_tags,
            settings,
        };
        if review.is_some_and(|review| review != &report) {
            return Err(
                "reviewed terminal context no longer matches the saved configuration".into(),
            );
        }
        // Bound the receipt before starting anything; it cannot fail encoding
        // for size only after a process has been created.
        let bytes = serde_json::to_vec(&report).map_err(|_| "terminal context encoding failed")?;
        if bytes.len() > cshell_application::MAX_TERMINAL_POLICY_REPLY - 128 {
            return Err("terminal context receipt limit".into());
        }
        Ok((Self::session_plan_from_snapshot(&snapshot, id)?, report))
    }

    fn session_plan_from_snapshot(
        snapshot: &CatalogSnapshot,
        id: ProfileId,
    ) -> Result<SavedSessionPlan, String> {
        let record = snapshot
            .profiles
            .iter()
            .find(|record| record.id == id)
            .ok_or("Profile does not exist")?;
        match record.kind {
            cshell_domain::ProfileKind::Ssh => Ok(SavedSessionPlan::Ssh(Box::new(
                plan_from_snapshot(snapshot, id)?,
            ))),
            cshell_domain::ProfileKind::Local => {
                let target = snapshot
                    .local_connections
                    .iter()
                    .find(|connection| connection.profile_id == id)
                    .ok_or("Local Profile program is not configured; edit and save it first")?;
                let cwd_policy = match &target.cwd {
                    cshell_domain::LocalWorkingDirectory::Inherit => {
                        cshell_local::WorkingDirectoryPolicy::Inherit
                    }
                    cshell_domain::LocalWorkingDirectory::Home => {
                        cshell_local::WorkingDirectoryPolicy::Home
                    }
                    cshell_domain::LocalWorkingDirectory::Explicit { path } => {
                        cshell_local::WorkingDirectoryPolicy::Explicit(path.into())
                    }
                };
                Ok(SavedSessionPlan::Local(
                    cshell_local::LocalProfile {
                        name: record.name.clone(),
                        program: target.program.clone().into(),
                        args: target.args.clone(),
                        cwd_policy,
                        env_overrides: target.env_overrides.clone(),
                    },
                    target.close_policy,
                ))
            }
        }
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
                local_shells: Vec::new(),
                export_json: Vec::new(),
            },
        }
    }

    async fn handle_inner(
        &self,
        mut request: ProfileRequest,
    ) -> Result<ProfileResponse, (ProfileStatus, String)> {
        let operation = ProfileOperation::try_from(request.operation)
            .map_err(|_| invalid("unknown Profile operation"))?;
        if !request.export_profile_ids.is_empty() && operation != ProfileOperation::ExportOpenSsh {
            return Err(invalid("Profile selection requires OpenSSH export"));
        }
        if request.include_local_context && operation != ProfileOperation::ExportCatalog {
            return Err(invalid("local export options require ExportCatalog"));
        }
        match operation {
            ProfileOperation::PreviewThemes
            | ProfileOperation::CommitThemes
            | ProfileOperation::ExportThemes => {
                if !request.changes.is_empty()
                    || !request.credential_profile_id.is_empty()
                    || !request.credential_secret.is_empty()
                    || !request.host_key_token.is_empty()
                    || !request.host_key_fingerprint.is_empty()
                {
                    return Err(invalid(
                        "Theme transfer must not contain mutation or credential fields",
                    ));
                }
                if operation == ProfileOperation::ExportThemes {
                    if !request.import_json.is_empty() || request.import_policy != 0 {
                        return Err(invalid("Theme export must not contain import fields"));
                    }
                    let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                    if snapshot.revision != request.expected_revision {
                        return Err((
                            ProfileStatus::Conflict,
                            "Catalog changed; refresh before Theme export".into(),
                        ));
                    }
                    let export_json =
                        cshell_application::export_themes(&snapshot).map_err(theme_error)?;
                    return Ok(ProfileResponse {
                        status: ProfileStatus::Ok as i32,
                        revision: snapshot.revision,
                        export_json,
                        ..Default::default()
                    });
                }
                let policy = decode_policy(request.import_policy)?;
                if operation == ProfileOperation::PreviewThemes {
                    let preview = self
                        .service
                        .preview_themes(&request.import_json, policy)
                        .await
                        .map_err(theme_error)?;
                    return Ok(ProfileResponse {
                        status: ProfileStatus::Ok as i32,
                        revision: preview.base_revision,
                        preview: Some(preview_data(preview)),
                        ..Default::default()
                    });
                }
                let _mutation = self.catalog_mutation.lock().await;
                self.service
                    .commit_themes(request.expected_revision, &request.import_json, policy)
                    .await
                    .map_err(theme_error)?;
                Ok(catalog_response(
                    self.service.load().await.map_err(service_error)?.snapshot(),
                ))
            }
            ProfileOperation::ExportCatalog | ProfileOperation::ExportOpenSsh => {
                if !request.changes.is_empty()
                    || !request.import_json.is_empty()
                    || !request.credential_profile_id.is_empty()
                    || !request.credential_secret.is_empty()
                    || !request.host_key_token.is_empty()
                    || !request.host_key_fingerprint.is_empty()
                    || request.import_policy != 0
                {
                    return Err(invalid(
                        "export must not contain mutation or credential fields",
                    ));
                }
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                if request.expected_revision != snapshot.revision {
                    return Err((
                        ProfileStatus::Conflict,
                        "Profile catalog changed; refresh before export".into(),
                    ));
                }
                let export_json = if operation == ProfileOperation::ExportOpenSsh {
                    if request.export_profile_ids.len()
                        > cshell_application::MAX_OPENSSH_EXPORT_HOSTS
                    {
                        return Err(invalid("too many OpenSSH export Profiles"));
                    }
                    let selected = request
                        .export_profile_ids
                        .iter()
                        .map(|id| {
                            decode_id(id)
                                .map(ProfileId::from_bytes)
                                .map_err(|_| invalid("invalid OpenSSH export selection"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    cshell_application::export_openssh_config(&snapshot, &selected)
                        .map_err(|error| invalid(error.to_string()))?
                } else {
                    cshell_application::export_profile_catalog(
                        &snapshot,
                        cshell_application::ProfileExportOptions {
                            include_local_context: request.include_local_context,
                        },
                    )
                    .map_err(import_error)?
                };
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: snapshot.revision,
                    export_json,
                    ..Default::default()
                })
            }
            ProfileOperation::DiscoverLocalShells => {
                let shells = tokio::task::spawn_blocking(cshell_local::discover_shells)
                    .await
                    .map_err(|_| {
                        (
                            ProfileStatus::Unavailable,
                            "local shell discovery failed".into(),
                        )
                    })?;
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    local_shells: shells
                        .into_iter()
                        .map(|shell| cshell_ipc::LocalShellData {
                            name: shell.name,
                            program: shell.program.to_string_lossy().into_owned(),
                            args: shell.args,
                        })
                        .collect(),
                    ..ProfileResponse::default()
                })
            }
            ProfileOperation::List => {
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                Ok(catalog_response(snapshot))
            }
            ProfileOperation::ApplyChanges => {
                let _mutation = self.catalog_mutation.lock().await;
                if request.changes.len() > MAX_PROFILE_CONTROL_CHANGES {
                    return Err(invalid("too many Profile changes"));
                }
                let changes = std::mem::take(&mut request.changes)
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
                    local_shells: Vec::new(),
                    export_json: Vec::new(),
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
                let target = target.clone();
                let vault = self.vault.clone();
                let result = if operation == ProfileOperation::SetPassword {
                    if request.credential_secret.is_empty()
                        || request.credential_secret.len() > 4096
                    {
                        return Err(invalid("password must contain 1 to 4096 bytes"));
                    }
                    let secret = Secret::new(std::mem::take(&mut request.credential_secret));
                    tokio::task::spawn_blocking(move || {
                        vault.edit(&target, VaultCredentialKind::Password, Some(&secret))
                    })
                    .await
                } else {
                    if !request.credential_secret.is_empty() {
                        return Err(invalid("delete password must not contain a secret"));
                    }
                    tokio::task::spawn_blocking(move || {
                        vault.edit(&target, VaultCredentialKind::Password, None)
                    })
                    .await
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
                            format!("credential store: {error}"),
                        )
                    })?;
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: snapshot.revision,
                    catalog: None,
                    preview: None,
                    detail: String::new(),
                    host_key_preview: None,
                    local_shells: Vec::new(),
                    export_json: Vec::new(),
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
                if !target.uses_private_key() {
                    return Err(invalid("SSH Profile does not use a private key"));
                }
                let _key_path = target
                    .private_key_path
                    .clone()
                    .ok_or_else(|| invalid("SSH Profile has no private key reference"))?;
                let target = target.clone();
                let vault = self.vault.clone();
                let result = if operation == ProfileOperation::SetKeyPassphrase {
                    if request.credential_secret.is_empty()
                        || request.credential_secret.len() > 4096
                    {
                        return Err(invalid("key passphrase must contain 1 to 4096 bytes"));
                    }
                    let secret = Secret::new(std::mem::take(&mut request.credential_secret));
                    tokio::task::spawn_blocking(move || {
                        vault.edit(&target, VaultCredentialKind::KeyPassphrase, Some(&secret))
                    })
                    .await
                } else {
                    if !request.credential_secret.is_empty() {
                        return Err(invalid("delete key passphrase must not contain a secret"));
                    }
                    tokio::task::spawn_blocking(move || {
                        vault.edit(&target, VaultCredentialKind::KeyPassphrase, None)
                    })
                    .await
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
                            format!("credential store: {error}"),
                        )
                    })?;
                Ok(ProfileResponse {
                    status: ProfileStatus::Ok as i32,
                    revision: snapshot.revision,
                    catalog: None,
                    preview: None,
                    detail: String::new(),
                    host_key_preview: None,
                    local_shells: Vec::new(),
                    export_json: Vec::new(),
                })
            }
            ProfileOperation::CommitImport => {
                let _mutation = self.catalog_mutation.lock().await;
                let policy = decode_policy(request.import_policy)?;
                self.service
                    .commit_import(request.expected_revision, &request.import_json, policy)
                    .await
                    .map_err(import_error)?;
                let snapshot = self.service.load().await.map_err(service_error)?.snapshot();
                Ok(catalog_response(snapshot))
            }
            ProfileOperation::PreviewHostKey | ProfileOperation::PreviewHostKeyUpdate => {
                let replacing = operation == ProfileOperation::PreviewHostKeyUpdate;
                let id_bytes = decode_id(&request.credential_profile_id)
                    .map_err(|error| invalid(error.to_string()))?;
                let id = ProfileId::from_bytes(id_bytes);
                let plan = self.host_key_target(id, request.expected_revision).await?;
                let target = plan.target.clone();
                let path = self.known_hosts_path()?;
                let verifier = crate::host_ca_store::verifier(&path, &target.host, target.port)
                    .await
                    .map_err(|error| (ProfileStatus::Corrupt, error.into()))?;
                if !replacing && verifier.has_matching_entries() {
                    return Err((
                        ProfileStatus::Conflict,
                        "host already has a known_hosts entry; first-key import is blocked".into(),
                    ));
                }
                let scanned = crate::ssh_route::scan_profile(&plan, &path, &self.vault)
                    .await
                    .map_err(|error| (ProfileStatus::Unavailable, error))?;
                let replacement = if replacing {
                    if verifier.has_authority_entries() || verifier.public_key_revoked(&scanned.public_key_line)
                        .map_err(|error| (ProfileStatus::Corrupt, error.into()))? {
                        return Err((ProfileStatus::Conflict, "CA-managed or revoked host trust cannot be replaced".into()));
                    }
                    Some(cshell_ssh::preview_host_key_update(&path, &target.host, target.port, &scanned.public_key_line)
                        .map_err(|error| (ProfileStatus::Conflict, error))?)
                } else { None };
                let token: [u8; 32] = rand::random();
                let expires_unix_seconds = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| (ProfileStatus::Unavailable, error.to_string()))?
                    .as_secs()
                    + 300;
                let preview = HostKeyPreviewData {
                    previous_fingerprints: replacement.as_ref().map_or_else(Vec::new, |r| r.previous_fingerprints.clone()),
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
                    replacement,
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
                    local_shells: Vec::new(),
                    export_json: Vec::new(),
                    host_key_preview: Some(Box::new(preview)),
                    ..ProfileResponse::default()
                })
            }
            ProfileOperation::ConfirmHostKey | ProfileOperation::ConfirmHostKeyUpdate => {
                let replacing = operation == ProfileOperation::ConfirmHostKeyUpdate;
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
                        || replacing != preview.replacement.is_some()
                    {
                        return Err(invalid(
                            "host-key preview expired or fingerprint confirmation did not match",
                        ));
                    }
                    previews
                        .remove(&id_bytes)
                        .ok_or_else(|| invalid("preview the host key again"))?
                };
                let plan = self.host_key_target(id, request.expected_revision).await?;
                let target = plan.target.clone();
                if target != pending.target {
                    return Err((
                        ProfileStatus::Conflict,
                        "SSH target changed; preview again".into(),
                    ));
                }
                let path = self.known_hosts_path()?;
                let scanned = crate::ssh_route::scan_profile(&plan, &path, &self.vault)
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
                let profile_id = id.as_uuid().to_string();
                let _mutation = self.catalog_mutation.lock().await;
                self.host_key_target(id, request.expected_revision).await?;
                let _trust = crate::host_ca_store::coordination().read_owned().await;
                let verifier = crate::host_ca_store::verifier(&path, &target.host, target.port)
                    .await
                    .map_err(|error| (ProfileStatus::Corrupt, error.into()))?;
                if (!replacing && verifier.has_matching_entries())
                    || (replacing && verifier.has_authority_entries())
                    || verifier
                        .public_key_revoked(&scanned.public_key_line)
                        .map_err(|error| (ProfileStatus::Corrupt, error.into()))?
                {
                    return Err((
                        ProfileStatus::Conflict,
                        "Effective host trust changed or this key is revoked; import blocked"
                            .into(),
                    ));
                }
                tokio::task::spawn_blocking(move || {
                    if let Some(replacement) = &pending.replacement {
                        cshell_ssh::update_confirmed_host_key(&path, &target.host, target.port,
                            &scanned.public_key_line, &pending.fingerprint, &profile_id, replacement)
                    } else { import_confirmed_host_key(
                        &path,
                        &target.host,
                        target.port,
                        &scanned.public_key_line,
                        &pending.fingerprint,
                        &profile_id,
                    ) }
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
                    detail: if replacing { "Host key updated in known_hosts with an audit record" } else { "Host key imported into known_hosts with an audit record" }.into(),
                    ..ProfileResponse::default()
                })
            }
        }
    }
}

fn plan_from_snapshot(
    snapshot: &CatalogSnapshot,
    id: ProfileId,
) -> Result<crate::ssh_route::SshConnectionPlan, String> {
    let profile = snapshot
        .profiles
        .iter()
        .find(|profile| profile.id == id && profile.kind == cshell_domain::ProfileKind::Ssh)
        .ok_or("saved SSH Profile does not exist")?;
    let target = snapshot
        .ssh_connections
        .iter()
        .find(|item| item.profile_id == id)
        .ok_or("saved SSH Profile has no target")?
        .clone();
    crate::ssh_route::SshConnectionPlan::new(
        profile.name.clone(),
        target,
        &snapshot.ssh_connections,
    )
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
        profile_change::Change::UpsertLocalConnection(value) => {
            Ok(CatalogChange::UpsertLocalConnection(
                cshell_domain::LocalConnectionRecord::try_from(value)?,
            ))
        }
        profile_change::Change::RemoveLocalConnection(value) => Ok(
            CatalogChange::RemoveLocalConnection(ProfileId::from_bytes(decode_id(&value)?)),
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
            themes: snapshot
                .themes
                .iter()
                .map(cshell_ipc::ThemeData::from)
                .collect(),
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
            local_connections: snapshot
                .local_connections
                .iter()
                .map(cshell_ipc::LocalConnectionData::from)
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
        local_shells: Vec::new(),
        export_json: Vec::new(),
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
                    ImportItemKind::Theme => ProfileImportItemKind::Theme,
                } as i32,
                name: item.name,
                details: item.details,
                theme: item.theme.as_ref().map(cshell_ipc::ThemeData::from),
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

fn theme_error(error: cshell_application::ThemeTransferError) -> (ProfileStatus, String) {
    use cshell_application::ThemeTransferError;
    let status = match &error {
        ThemeTransferError::UnsupportedFormat => ProfileStatus::Unsupported,
        ThemeTransferError::Conflicts
        | ThemeTransferError::Catalog(CatalogError::StaleRevision { .. }) => {
            ProfileStatus::Conflict
        }
        ThemeTransferError::Service(error) => {
            return service_error(match error {
                ProfileServiceError::Catalog(error) => ProfileServiceError::Catalog(error.clone()),
                ProfileServiceError::Repository(error) => ProfileServiceError::Repository(*error),
            });
        }
        _ => ProfileStatus::Invalid,
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

impl ProfileIpcService {
    pub(crate) async fn handle_workspace(
        &self,
        request: cshell_ipc::WorkspaceRequest,
    ) -> cshell_ipc::WorkspaceResponse {
        use cshell_application::{WorkspaceRepository, WorkspaceRepositoryError, decode_workspace};
        use cshell_ipc::{WorkspaceOperation, WorkspaceResponse, WorkspaceStatus};
        let result = match WorkspaceOperation::try_from(request.operation) {
            Ok(WorkspaceOperation::Load)
                if request.document_json.is_empty() && request.expected_revision == 0 =>
            {
                self.service.repository().load_workspace().await
            }
            Ok(WorkspaceOperation::Save) => {
                let document = match decode_workspace(&request.document_json) {
                    Ok(document) => document,
                    Err(error) => {
                        return WorkspaceResponse {
                            status: WorkspaceStatus::Invalid as i32,
                            detail: error.to_string(),
                            ..Default::default()
                        };
                    }
                };
                self.service
                    .repository()
                    .save_workspace(request.expected_revision, &document)
                    .await
                    .map(Some)
            }
            _ => {
                return WorkspaceResponse {
                    status: WorkspaceStatus::Invalid as i32,
                    detail: "Invalid workspace operation".into(),
                    ..Default::default()
                };
            }
        };
        match result {
            Ok(snapshot) => {
                let revision = snapshot.as_ref().map_or(0, |s| s.revision);
                let json = snapshot.map_or(Ok(Vec::new()), |s| {
                    cshell_application::encode_workspace(&s.document)
                });
                match json {
                    Ok(document_json) => WorkspaceResponse {
                        status: WorkspaceStatus::Ok as i32,
                        revision,
                        document_json,
                        detail: String::new(),
                    },
                    Err(error) => WorkspaceResponse {
                        status: WorkspaceStatus::Corrupt as i32,
                        detail: error.to_string(),
                        ..Default::default()
                    },
                }
            }
            Err(error) => WorkspaceResponse {
                status: match error {
                    WorkspaceRepositoryError::Conflict { .. } => WorkspaceStatus::Conflict,
                    WorkspaceRepositoryError::Unavailable => WorkspaceStatus::Unavailable,
                    WorkspaceRepositoryError::Corrupt => WorkspaceStatus::Corrupt,
                } as i32,
                detail: error.to_string(),
                ..Default::default()
            },
        }
    }
}
