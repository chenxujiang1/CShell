use crate::daemon_connection::DesktopConnectionConfig;
use cshell_domain::{
    LocalConnectionRecord, ProfileFolder, ProfileId, ProfileRecord, SshConnectionRecord,
    TerminalDefaults,
};
use cshell_ipc::{
    Envelope, Handshake, HostKeyPreviewData, ProfileCatalogData, ProfileChange,
    ProfileImportPolicy, ProfileImportPreviewData, ProfileOperation, ProfileRequest,
    ProfileResponse, ProfileStatus, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zeroize::{Zeroize, Zeroizing};

const MAX_IMPORT_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProfileImportFormat {
    #[default]
    Catalog,
    OpenSsh,
    PuttyRegistry,
    SecureCrtIni,
    XshellSession,
    MobaXtermUnicode,
    MobaXtermWindows1252,
    SecureCrtDirectory,
    XshellDirectory,
    ForeignManifest,
    Theme,
}
impl ProfileImportFormat {
    pub fn reviewed_metadata(self) -> bool {
        matches!(
            self,
            Self::PuttyRegistry
                | Self::SecureCrtIni
                | Self::XshellSession
                | Self::MobaXtermUnicode
                | Self::MobaXtermWindows1252
                | Self::SecureCrtDirectory
                | Self::XshellDirectory
                | Self::ForeignManifest
        )
    }
    pub fn batch_input(self) -> Option<cshell_import::ForeignBatchInput> {
        match self {
            Self::SecureCrtDirectory => Some(cshell_import::ForeignBatchInput::SecureCrtDirectory),
            Self::XshellDirectory => Some(cshell_import::ForeignBatchInput::XshellDirectory),
            Self::ForeignManifest => Some(cshell_import::ForeignBatchInput::Manifest),
            _ => None,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProfileImportSource {
    pub path: String,
    pub policy: ProfileImportPolicy,
    pub format: ProfileImportFormat,
    pub default_username: String,
}

impl std::fmt::Debug for ProfileImportSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileImportSource")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default)]
pub struct DesktopProfileView {
    pub generation: u64,
    pub credential_pending: bool,
    pub(crate) vault_mutations_pending: usize,
    pub host_key_pending: bool,
    pub host_key_existing_entry: bool,
    pub host_key_scope: Option<(ProfileId, u64)>,
    pub host_key_confirmed: Option<ProfileId>,
    pub catalog: Option<DesktopProfileCatalog>,
    pub preview: Option<ProfileImportPreviewData>,
    pub preview_source: Option<ProfileImportSource>,
    pub import_notices: Vec<String>,
    pub import_digest: Option<[u8; 32]>,
    pub host_key_preview: Option<HostKeyPreviewData>,
    pub local_shells: Vec<cshell_ipc::LocalShellData>,
    pub error: Option<String>,
    pub status: String,
    pub vault: Option<cshell_ipc::VaultResponse>,
    pub vault_error: Option<String>,
    pub vault_notice: String,
    pub package_generation: u64,
    pub package_preview: Option<cshell_ipc::CredentialPackagePreview>,
    pub package_status: String,
    pub package_source: Option<(String, i32)>,
}

#[derive(Clone)]
pub struct DesktopProfileCatalog {
    pub revision: u64,
    pub defaults: TerminalDefaults,
    pub folders: Vec<ProfileFolder>,
    pub profiles: Vec<ProfileRecord>,
    pub ssh_connections: Vec<SshConnectionRecord>,
    pub local_connections: Vec<LocalConnectionRecord>,
    pub themes: Vec<cshell_domain::ThemeRecord>,
}

impl std::fmt::Debug for DesktopProfileView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DesktopProfileView")
            .field("generation", &self.generation)
            .field("catalog", &self.catalog)
            .field("has_preview", &self.preview.is_some())
            .field("host_key_preview", &self.host_key_preview)
            .field("has_error", &self.error.is_some())
            .field("local_shell_count", &self.local_shells.len())
            .finish_non_exhaustive()
    }
}

impl DesktopProfileCatalog {
    pub(super) fn export_snapshot(&self) -> cshell_application::CatalogSnapshot {
        cshell_application::CatalogSnapshot {
            revision: self.revision,
            defaults: self.defaults.clone(),
            folders: self.folders.clone(),
            profiles: self.profiles.clone(),
            ssh_connections: self.ssh_connections.clone(),
            local_connections: self.local_connections.clone(),
            themes: self.themes.clone(),
        }
    }
}

impl std::fmt::Debug for DesktopProfileCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DesktopProfileCatalog")
            .field("revision", &self.revision)
            .field("profile_count", &self.profiles.len())
            .field("folder_count", &self.folders.len())
            .field("ssh_connection_count", &self.ssh_connections.len())
            .field("local_connection_count", &self.local_connections.len())
            .finish_non_exhaustive()
    }
}

pub enum ProfileClientCommand {
    SaveTheme {
        expected_revision: u64,
        theme: cshell_domain::ThemeRecord,
    },
    Vault(cshell_ipc::VaultRequest),
    CredentialPackage {
        path: String,
        request: cshell_ipc::VaultRequest,
    },
    Refresh,
    DiscoverLocalShells,
    OpenProfile(ProfileId),
    OpenLocal(ProfileId, cshell_ipc::LocalLaunchOptions),
    SetPassword {
        profile_id: ProfileId,
        expected_revision: u64,
        password: Zeroizing<Vec<u8>>,
    },
    DeletePassword {
        profile_id: ProfileId,
        expected_revision: u64,
    },
    SetKeyPassphrase {
        profile_id: ProfileId,
        expected_revision: u64,
        passphrase: Zeroizing<Vec<u8>>,
    },
    DeleteKeyPassphrase {
        profile_id: ProfileId,
        expected_revision: u64,
    },
    Apply {
        expected_revision: u64,
        changes: Vec<ProfileChange>,
    },
    ApplyWithPassword {
        expected_revision: u64,
        changes: Vec<ProfileChange>,
        profile_id: ProfileId,
        password: Zeroizing<Vec<u8>>,
    },
    ExportOpenSsh {
        path: String,
        expected_revision: u64,
        selected: Vec<ProfileId>,
    },
    ExportThemes {
        path: String,
        expected_revision: u64,
    },
    ExportCatalog {
        path: String,
        expected_revision: u64,
        include_local_context: bool,
    },
    PreviewImport(ProfileImportSource),
    CommitImport {
        expected_revision: u64,
    },
    CommitReviewedImport {
        expected_revision: u64,
        document_sha256: [u8; 32],
    },
    PreviewHostKey {
        profile_id: ProfileId,
        expected_revision: u64,
    },
    ConfirmHostKey {
        profile_id: ProfileId,
        expected_revision: u64,
        token: Zeroizing<Vec<u8>>,
        fingerprint: String,
    },
}

impl ProfileClientCommand {
    fn host_key_scope(&self) -> Option<(ProfileId, u64)> {
        match self {
            Self::PreviewHostKey { profile_id, expected_revision }
            | Self::ConfirmHostKey { profile_id, expected_revision, .. } => Some((*profile_id, *expected_revision)),
            _ => None,
        }
    }
    fn is_vault_mutation(&self) -> bool {
        matches!(self, Self::Vault(request)
            if request.operation != cshell_ipc::VaultOperation::Status as i32)
    }

    pub(crate) fn is_credential_mutation(&self) -> bool {
        matches!(self, Self::Apply { .. } | Self::ApplyWithPassword { .. }
            | Self::SetPassword { .. } | Self::DeletePassword { .. }
            | Self::SetKeyPassphrase { .. } | Self::DeleteKeyPassphrase { .. })
    }
}

impl std::fmt::Debug for ProfileClientCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::SaveTheme { .. } => "SaveTheme",
            Self::Vault(_) => "Vault([REDACTED])",
            Self::CredentialPackage { .. } => "CredentialPackage([REDACTED])",
            Self::Refresh => "Refresh",
            Self::DiscoverLocalShells => "DiscoverLocalShells",
            Self::OpenProfile(_) => "OpenProfile",
            Self::OpenLocal(..) => "OpenLocal([REDACTED])",
            Self::SetPassword { .. } => "SetPassword([REDACTED])",
            Self::DeletePassword { .. } => "DeletePassword",
            Self::SetKeyPassphrase { .. } => "SetKeyPassphrase([REDACTED])",
            Self::DeleteKeyPassphrase { .. } => "DeleteKeyPassphrase",
            Self::Apply { .. } => "Apply",
            Self::ApplyWithPassword { .. } => "ApplyWithPassword([REDACTED])",
            Self::ExportCatalog { .. } => "ExportCatalog",
            Self::ExportThemes { .. } => "ExportThemes",
            Self::ExportOpenSsh { .. } => "ExportOpenSsh",
            Self::PreviewImport(_) => "PreviewImport",
            Self::CommitImport { .. } => "CommitImport",
            Self::CommitReviewedImport { .. } => "CommitReviewedImport",
            Self::PreviewHostKey { .. } => "PreviewHostKey",
            Self::ConfirmHostKey { .. } => "ConfirmHostKey",
        };
        formatter.write_str(name)
    }
}

#[derive(Debug)]
pub struct DesktopProfileConnection {
    shared: Arc<Mutex<DesktopProfileView>>,
    sender: Option<tokio::sync::mpsc::Sender<ProfileClientCommand>>,
    worker: Option<std::thread::JoinHandle<()>>,
    lock_sender: Option<tokio::sync::mpsc::Sender<()>>,
    lock_worker: Option<std::thread::JoinHandle<()>>,
}

impl DesktopProfileConnection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(DesktopProfileView {
            status: "Profiles loading".into(),
            ..DesktopProfileView::default()
        }));
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        let worker_shared = Arc::clone(&shared);
        // Lock uses a separate authenticated connection, including while Argon2 is running.
        let (lock_sender, mut lock_receiver) = tokio::sync::mpsc::channel(1);
        let lock_config = config.clone();
        let lock_shared = Arc::clone(&shared);
        let lock_worker = std::thread::Builder::new()
            .name("cshell-vault-lock".into())
            .spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(async move {
                        while lock_receiver.recv().await.is_some() {
                            let result = send_vault(
                                &lock_config,
                                cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::LockTransient),
                            )
                            .await;
                            publish_vault(&lock_shared, result, false);
                        }
                    });
                }
            })?;
        let worker = std::thread::Builder::new()
            .name("cshell-profile-ipc".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        runtime.block_on(profile_worker(config, worker_shared, receiver))
                    }
                    Err(error) => {
                        update(&worker_shared, |view| view.error = Some(error.to_string()))
                    }
                }
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            worker: Some(worker),
            lock_sender: Some(lock_sender),
            lock_worker: Some(lock_worker),
        })
    }

    pub fn view(&self) -> DesktopProfileView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn catalog_revision(&self) -> Option<u64> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog
            .as_ref()
            .map(|c| c.revision)
    }

    pub fn request(&self, command: ProfileClientCommand) -> bool {
        let tracked = command.is_credential_mutation();
        let vault_mutation = command.is_vault_mutation();
        let host_key_scope = command.host_key_scope();
        let mut view = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if tracked && view.credential_pending || vault_mutation && view.vault_mutations_pending>0 {
            if vault_mutation {view.vault_error=Some("凭据操作正在进行，请等待完成后重试。".into());view.generation=view.generation.wrapping_add(1);}
            return false;
        }
        if host_key_scope.is_some() && view.host_key_pending { return false; }
        let sent = self.sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(command).is_ok());
        if let Some(scope) = host_key_scope {
            view.host_key_scope = Some(scope);
            view.host_key_pending = sent;
            view.host_key_confirmed = None;
            view.host_key_preview = None;
            view.host_key_existing_entry = false;
            if sent { view.error = None; }
        }
        if sent && vault_mutation {
            view.vault_mutations_pending += 1;
        }
        if sent && tracked {
            view.credential_pending = true;
            view.error = None;
            view.status = "正在保存或移除凭据，请稍候…".into();
        } else if !sent {
            view.error = Some("请求未提交：后台队列已满或连接已关闭，请稍后重试。".into());
            if vault_mutation {view.vault_error=view.error.clone();}
        }
        view.generation = view.generation.wrapping_add(1);
        sent
    }
    fn lock_vault_on_exit(&self) {
        if let Some(sender) = &self.lock_sender {
            let _ = sender.try_send(());
        }
    }


}

impl Drop for DesktopProfileConnection {
    fn drop(&mut self) {
        self.lock_vault_on_exit();
        self.sender.take();
        self.lock_sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.lock_worker.take() {
            let _ = worker.join();
        }
    }
}

async fn profile_worker(
    config: DesktopConnectionConfig,
    shared: Arc<Mutex<DesktopProfileView>>,
    mut receiver: tokio::sync::mpsc::Receiver<ProfileClientCommand>,
) {
    let mut pending_import: Option<(Vec<u8>, ProfileImportPolicy, u64, ProfileImportFormat)> = None;
    let mut vault_profile = Vec::new();
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = refresh.tick() => {
                if shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).catalog.is_none() {
                    let result = send(&config, request(ProfileOperation::List)).await.and_then(catalog_from_response);
                    publish_catalog(&shared, result);
                    discover_local_shells(&config, &shared).await;
                }
                let result = send_vault(&config, cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::Status).for_profile(vault_profile.clone())).await;
                publish_vault(&shared, result, false);
            }
            next = receiver.recv() => {
                let Some(command) = next else { break };
                let tracked = command.is_credential_mutation();
                let vault_mutation = command.is_vault_mutation();
                match command {
                    ProfileClientCommand::CredentialPackage { path, request } => {
                        let source = (request.operation == cshell_ipc::VaultOperation::PreviewPackage as i32).then(|| (path.clone(), request.package_policy));
                        let result = transfer_package(&config, &path, request).await;
                        update(&shared, |view| { view.package_generation = view.package_generation.wrapping_add(1); view.package_preview = None; view.package_source = source; match result { Ok((preview, status)) => { view.package_preview = preview; view.package_status = status; }, Err(error) => view.package_status = error } });
                        publish_vault(&shared, send_vault(&config, cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::Status).for_profile(vault_profile.clone())).await, false);
                    }
                    ProfileClientCommand::Vault(request) => {
                        if !request.profile_id.is_empty() { vault_profile = request.profile_id.clone(); }
                        let mutation = request.operation != cshell_ipc::VaultOperation::Status as i32;
                        publish_vault(&shared, send_vault(&config, request).await, mutation);
                    }
                    ProfileClientCommand::OpenProfile(_) | ProfileClientCommand::OpenLocal(..) => {}
                    ProfileClientCommand::DiscoverLocalShells => { discover_local_shells(&config, &shared).await; }
                    ProfileClientCommand::SetPassword { profile_id, expected_revision, mut password } => {
                        let mut outgoing = request(ProfileOperation::SetPassword);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        outgoing.credential_secret = std::mem::take(&mut *password);
                        let result = send(&config, outgoing).await.and_then(|response| check_response(&response));
                        publish_credential_result(&shared, result, "密码已保存到凭据存储");
                    }
                    ProfileClientCommand::DeletePassword { profile_id, expected_revision } => {
                        let mut outgoing = request(ProfileOperation::DeletePassword);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        let result = send(&config, outgoing).await.and_then(|response| check_response(&response));
                        publish_credential_result(&shared, result, "密码已从凭据存储移除");
                    }
                    ProfileClientCommand::SetKeyPassphrase { profile_id, expected_revision, mut passphrase } => {
                        let mut outgoing = request(ProfileOperation::SetKeyPassphrase);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        outgoing.credential_secret = std::mem::take(&mut *passphrase);
                        let result = send(&config, outgoing).await.and_then(|response| check_response(&response));
                        publish_credential_result(&shared, result, "Key passphrase saved in selected credential store");
                    }
                    ProfileClientCommand::DeleteKeyPassphrase { profile_id, expected_revision } => {
                        let mut outgoing = request(ProfileOperation::DeleteKeyPassphrase);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        let result = send(&config, outgoing).await.and_then(|response| check_response(&response));
                        publish_credential_result(&shared, result, "Key passphrase removed from selected credential store");
                    }
                    ProfileClientCommand::SaveTheme{expected_revision,theme}=>{
                        let result=async{
                            cshell_application::validate_themes(std::slice::from_ref(&theme)).map_err(|e|e.to_string())?;
                            let document=cshell_application::ThemeDocument{format:cshell_application::THEME_FORMAT.into(),version:cshell_application::THEME_VERSION,themes:vec![theme.clone()]};
                            let bytes=serde_json::to_vec(&document).map_err(|_|"Theme encoding failed".to_owned())?;
                            let mut preview=request(ProfileOperation::PreviewThemes);preview.import_json=bytes.clone();preview.import_policy=ProfileImportPolicy::Replace as i32;
                            let response=send(&config,preview).await?;check_response(&response)?;
                            let preview=response.preview.ok_or("Theme preview missing")?;
                            if preview.base_revision!=expected_revision || !preview.can_commit || preview.items.len()!=1 || preview.items[0].theme.clone().and_then(|t|cshell_domain::ThemeRecord::try_from(t).ok())!=Some(theme){return Err("Theme preview scope/revision mismatch".into());}
                            let mut commit=request(ProfileOperation::CommitThemes);commit.expected_revision=expected_revision;commit.import_json=bytes;commit.import_policy=ProfileImportPolicy::Replace as i32;
                            let response=send(&config,commit).await?;
                            let catalog=catalog_from_response(response)?;
                            if catalog.revision!=expected_revision.checked_add(1).ok_or("Theme revision exhausted")? {return Err("Theme commit revision mismatch".into());}
                            Ok(catalog)
                        }.await;publish_catalog(&shared,result);
                    }
                    ProfileClientCommand::Refresh => {
                        publish_catalog(&shared, send(&config, request(ProfileOperation::List)).await.and_then(catalog_from_response));
                    }
                    ProfileClientCommand::Apply { expected_revision, changes } => {
                        let mut outgoing = request(ProfileOperation::ApplyChanges);
                        outgoing.expected_revision = expected_revision;
                        outgoing.changes = changes;
                        publish_catalog(&shared, send(&config, outgoing).await.and_then(catalog_from_response));
                    }
                    ProfileClientCommand::ApplyWithPassword { expected_revision, changes, profile_id, mut password } => {
                        let mut outgoing = request(ProfileOperation::ApplyChanges);
                        outgoing.expected_revision = expected_revision;
                        outgoing.changes = changes.clone();
                        match send(&config, outgoing).await.and_then(catalog_from_response) {
                            Ok(catalog) => {
                                let credential = password_request_for_saved_target(&catalog, profile_id, &changes, &mut password);
                                publish_catalog(&shared, Ok(catalog));
                                let result = match credential {
                                    Ok(outgoing) => send(&config, outgoing).await.and_then(|response| check_response(&response)),
                                    Err(error) => Err(error),
                                }.map_err(|error| format!("配置已保存，但密码未保存：{error}。请检查凭据存储状态后重新填写并保存密码。"));
                                publish_credential_result(&shared, result, "配置和密码已保存");
                            }
                            Err(error) => publish_catalog(&shared, Err(error)),
                        }
                    }
                    ProfileClientCommand::ExportOpenSsh { path, expected_revision, selected } => {
                        let catalog = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).catalog.clone();
                        let result = export_openssh(&config, catalog, &path, expected_revision, &selected).await;
                        publish_credential_result(&shared, result, "OpenSSH config saved (credentials and trust excluded)");
                    }
                    ProfileClientCommand::ExportThemes { path, expected_revision } => {
                        let result = async {
                            let mut outgoing = request(ProfileOperation::ExportThemes);
                            outgoing.expected_revision = expected_revision;
                            let response = send(&config, outgoing).await?;
                            check_response(&response)?;
                            if response.revision != expected_revision { return Err("Theme export revision mismatch; refresh before retrying".into()); }
                            let document=cshell_application::ThemeDocument::parse(&response.export_json).map_err(|_| "Daemon returned an invalid Theme document")?;
                            let bytes=if std::path::Path::new(&path).extension().is_some_and(|e|e.eq_ignore_ascii_case("toml")){document.to_toml().map_err(|_|"Theme TOML encoding failed")?}else{response.export_json};
                            crate::profile_export::save_export(&path, &bytes)
                        }.await;
                        publish_credential_result(&shared, result, "Theme export saved");
                    }
                    ProfileClientCommand::ExportCatalog { path, expected_revision, include_local_context } => {
                        let result = async {
                            let mut outgoing = request(ProfileOperation::ExportCatalog);
                            outgoing.expected_revision = expected_revision;
                            outgoing.include_local_context = include_local_context;
                            let mut response = send(&config, outgoing).await?;
                            check_response(&response)?;
                            if response.revision != expected_revision { return Err("Export revision mismatch; refresh before retrying".into()); }
                            let bytes = Zeroizing::new(std::mem::take(&mut response.export_json));
                            let document = cshell_application::ProfileImportDocument::parse(&bytes)
                                .map_err(|_| "Daemon returned an invalid export document")?;
                            if document.version != cshell_application::PROFILE_TRANSFER_VERSION { return Err("Daemon returned an unsupported export document".into()); }
                            crate::profile_export::save_export(&path, &bytes)
                        }.await;
                        publish_credential_result(&shared, result, "Profile export saved (credentials excluded)");
                    }
                    ProfileClientCommand::PreviewImport(source) => {
                        pending_import = None;
                        update(&shared,|view|{view.preview=None;view.preview_source=None;view.import_digest=None;view.import_notices.clear();});
                        let result: Result<(Vec<u8>, ProfileImportPreviewData,Vec<String>,Option<[u8;32]>), String> = async {
                            let (bytes,notices) = prepare_import_source(&source).await?;
                            let digest=if source.format.reviewed_metadata(){Some(cshell_application::ProfileImportDocument::parse(&bytes).and_then(|d|d.fingerprint()).map_err(|_|"Converted session document is invalid")?)}else{None};
                            let mut outgoing = request(if source.format == ProfileImportFormat::Theme { ProfileOperation::PreviewThemes } else { ProfileOperation::PreviewImport });
                            outgoing.import_json = bytes.clone();
                            outgoing.import_policy = source.policy as i32;
                            let response = send(&config, outgoing).await?;
                            check_response(&response)?;
                            let preview = response.preview.ok_or("daemon omitted import preview")?;
                            Ok((bytes, preview,notices,digest))
                        }.await;
                        match result {
                            Ok((bytes, preview,notices,digest)) => {
                                pending_import = Some((bytes, source.policy, preview.base_revision, source.format));
                                update(&shared, |view| {
                                    view.preview_source = Some(source);
                                    view.import_notices=notices;view.import_digest=digest;
                                    view.status = "Import preview ready".into();
                                    view.error = None;
                                    view.preview = Some(preview);
                                });
                            }
                            Err(error) => update(&shared, |view| {
                                view.preview = None;
                                view.preview_source = None;
                                view.error = Some(error);
                            }),
                        }
                    }
                    ProfileClientCommand::CommitReviewedImport{expected_revision,document_sha256} => {
                        let result=if let Some((bytes,policy,revision,format))=pending_import.take(){
                            let actual=cshell_application::ProfileImportDocument::parse(&bytes).and_then(|d|d.fingerprint()).map_err(|_|"Invalid converted import".to_owned());
                            if revision!=expected_revision || !format.reviewed_metadata() || actual.ok()!=Some(document_sha256){Err("Converted import or preview changed; preview and review again".into())}else{
                                let mut outgoing=request(ProfileOperation::CommitImport);outgoing.expected_revision=revision;outgoing.import_json=bytes;outgoing.import_policy=policy as i32;send(&config,outgoing).await.and_then(catalog_from_response)
                            }
                        }else{Err("Preview a converted import first".into())};
                        publish_catalog(&shared,result);update(&shared,|view|{view.preview=None;view.preview_source=None;view.import_digest=None;view.import_notices.clear();});
                    }
                    ProfileClientCommand::CommitImport { expected_revision } => {
                        let result = if let Some((bytes, policy, preview_revision, format)) = pending_import.take() {
                            if preview_revision != expected_revision || format.reviewed_metadata() {
                                Err("import preview is stale; preview again".into())
                            } else {
                                let mut outgoing = request(if format == ProfileImportFormat::Theme { ProfileOperation::CommitThemes } else { ProfileOperation::CommitImport });
                                outgoing.expected_revision = expected_revision;
                                outgoing.import_json = bytes;
                                outgoing.import_policy = policy as i32;
                                send(&config, outgoing).await.and_then(catalog_from_response)
                            }
                        } else {
                            Err("preview an import file first".into())
                        };
                        publish_catalog(&shared, result);
                        update(&shared, |view| { view.preview = None; view.preview_source = None; });
                    }
                    ProfileClientCommand::PreviewHostKey { profile_id, expected_revision } => {
                        let mut outgoing = request(ProfileOperation::PreviewHostKey);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        let result = send(&config, outgoing).await.and_then(|response| {
                            check_response(&response)?;
                            response.host_key_preview.map(|preview| *preview).ok_or_else(|| "daemon omitted host-key preview".into())
                        });
                        update(&shared, |view| publish_host_key_preview(view, result));
                    }
                    ProfileClientCommand::ConfirmHostKey { profile_id, expected_revision, mut token, fingerprint } => {
                        let mut outgoing = request(ProfileOperation::ConfirmHostKey);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        outgoing.host_key_token = std::mem::take(&mut *token);
                        outgoing.host_key_fingerprint = fingerprint;
                        let result = send(&config, outgoing).await.and_then(|response| check_response(&response));
                        update(&shared, |view| { view.host_key_pending = false; match result {
                            Ok(()) => {
                                view.host_key_confirmed = Some(profile_id);
                                view.host_key_preview = None;
                                view.error = None;
                                view.status = "服务器主机密钥已保存".into();
                            }
                            Err(error) => {
                                view.host_key_preview = None;
                                view.error = Some(error);
                            }
                        }});
                    }
                }
                if tracked { update(&shared, |view| view.credential_pending = false); }
                if vault_mutation {
                    update(&shared, |view| view.vault_mutations_pending = view.vault_mutations_pending.saturating_sub(1));
                }
            }
        }
    }
}

fn password_request_for_saved_target(
    catalog: &DesktopProfileCatalog,
    profile_id: ProfileId,
    changes: &[ProfileChange],
    password: &mut Zeroizing<Vec<u8>>,
) -> Result<ProfileRequest, String> {
    let expected = changes.iter().find_map(|change| match &change.change {
        Some(cshell_ipc::profile_change::Change::UpsertSshConnection(target))
            if target.profile_id == profile_id.as_uuid().as_bytes() => Some(target),
        _ => None,
    }).ok_or("保存结果缺少 SSH 目标")?;
    let actual = catalog.ssh_connections.iter().find(|target| target.profile_id == profile_id)
        .ok_or("保存结果缺少 SSH 目标")?;
    if !actual.uses_password()
        || cshell_ipc::SshConnectionData::from(actual) != *expected
    {
        return Err("保存后的 SSH 目标与草稿不一致".into());
    }
    let mut outgoing = request(ProfileOperation::SetPassword);
    outgoing.expected_revision = catalog.revision;
    outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
    outgoing.credential_secret = std::mem::take(&mut **password);
    Ok(outgoing)
}

fn publish_credential_result(
    shared: &Arc<Mutex<DesktopProfileView>>,
    result: Result<(), String>,
    message: &str,
) {
    update(shared, |view| match result {
        Ok(()) => {
            view.error = None;
            view.status = message.to_owned();
        }
        Err(error) => view.error = Some(error),
    });
}

fn publish_vault(
    shared: &Arc<Mutex<DesktopProfileView>>,
    result: Result<cshell_ipc::VaultResponse, String>,
    mutation: bool,
) {
    update(shared, |view| match result {
        Ok(response) => {
            let status = cshell_ipc::VaultControlStatus::try_from(response.status)
                .unwrap_or(cshell_ipc::VaultControlStatus::Unavailable);
            if mutation || status != cshell_ipc::VaultControlStatus::Ok {
                view.vault_error = (status != cshell_ipc::VaultControlStatus::Ok)
                    .then(|| if !response.key_notice.is_empty() { response.key_notice.clone() } else { match status {
                        cshell_ipc::VaultControlStatus::RootKeyMissing => "系统解锁密钥已丢失。可尝试恢复系统凭据，或保留旧库并重建。".into(),
                        cshell_ipc::VaultControlStatus::KeychainUnavailable => "系统钥匙串暂时不可用或访问被拒绝，请先检查系统登录状态与权限后重试。".into(),
                        cshell_ipc::VaultControlStatus::AuthenticationFailed => "密码不正确，或系统密钥与凭据库不匹配。请重试；无法恢复时可保留旧库并重建。".into(),
                        _ => format!("Vault: {status:?}；刷新状态后再操作"),
                    }});
                view.vault_notice = if status == cshell_ipc::VaultControlStatus::Ok { response.key_notice.clone() } else { String::new() };
            }
            view.vault = Some(response);
        }
        Err(error) => {
            view.vault = None;
            view.vault_error = Some(error);
        }
    });
}

async fn transfer_package(
    config: &DesktopConnectionConfig,
    path: &str,
    mut request: cshell_ipc::VaultRequest,
) -> Result<(Option<cshell_ipc::CredentialPackagePreview>, String), String> {
    use cshell_ipc::{VaultControlStatus as Status, VaultOperation as Op};
    let operation = Op::try_from(request.operation).map_err(|_| "Unknown package operation")?;
    if operation == Op::PreviewPackage {
        request.package = read_import_file(path).await?;
    }
    let mut response = send_vault(config, request).await?;
    if response.status != Status::Ok as i32 {
        return Err(match Status::try_from(response.status) {
            Ok(Status::AuthenticationFailed) => {
                "Package authentication failed; check the password and file"
            }
            Ok(Status::Conflict) => {
                "Saved state or targets changed; import the matching Profile JSON and preview again"
            }
            Ok(Status::Locked) => "Unlock encrypted Vault before transferring credentials",
            Ok(Status::Busy) => "Too many pending previews; wait 2 minutes and preview again",
            Ok(Status::ReloadRequired) => {
                "Vault result uncertain; reload and inspect before retrying"
            }
            _ => "Credential package unavailable or invalid; no automatic retry",
        }
        .into());
    }
    match operation {
        Op::ExportPackage => {
            if response.package.is_empty()
                || response.package.len() > cshell_ipc::MAX_CREDENTIAL_PACKAGE_BYTES
            {
                return Err("Invalid encrypted package response".into());
            }
            crate::profile_export::save_export(path, &response.package)?;
            Ok((None, "Encrypted credential package saved".into()))
        }
        Op::PreviewPackage => {
            let preview = response
                .package_preview
                .take()
                .ok_or("Package preview unavailable")?;
            if preview.items.is_empty()
                || preview.items.len() > cshell_ipc::MAX_CREDENTIAL_PACKAGE_PROFILES
                || (preview.can_commit && preview.token.len() != 32)
            {
                return Err("Invalid package preview".into());
            }
            Ok((
                Some(preview),
                "Review the target and credential groups; re-enter the package password to confirm"
                    .into(),
            ))
        }
        Op::CommitPackage => Ok((
            None,
            "Credential import finished; refresh Vault status before connecting".into(),
        )),
        _ => Err("Unknown package operation".into()),
    }
}

async fn send_vault(
    config: &DesktopConnectionConfig,
    mut request: cshell_ipc::VaultRequest,
) -> Result<cshell_ipc::VaultResponse, String> {
    tokio::time::timeout(std::time::Duration::from_secs(40), async {
        let resolved = config
            .resolve()
            .map_err(|_| "Vault discovery unavailable")?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| "Vault daemon unavailable")?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        let package = matches!(
            cshell_ipc::VaultOperation::try_from(request.operation),
            Ok(cshell_ipc::VaultOperation::ExportPackage
                | cshell_ipc::VaultOperation::PreviewPackage
                | cshell_ipc::VaultOperation::CommitPackage)
        );
        handshake.feature_bits =
            features::VAULT_CONTROL | features::CREDENTIAL_PACKAGE | features::VAULT_SESSION_STATE;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "Vault handshake failed")?;
        if negotiated.feature_bits & features::VAULT_CONTROL == 0 {
            return Err("daemon 不支持 Vault；请更新后重启 daemon".into());
        }
        if request.operation==cshell_ipc::VaultOperation::LockTransient as i32 && negotiated.protocol_minor<70 {request.operation=cshell_ipc::VaultOperation::Lock as i32;}
        if request.operation>=cshell_ipc::VaultOperation::ProtectWithSystem as i32 && negotiated.protocol_minor<70 {return Err("请一起更新 GUI 和后台，以支持凭据库保护方式无损切换".into());}
        if request.managed_key.is_some() && negotiated.protocol_minor < 67 {
            return Err("后台版本不支持密钥对管理，请更新并重启后台".into());
        }
        if request.confirm_rebuild && negotiated.protocol_minor < 68 {
            return Err("后台版本不支持安全重建凭据库，请更新并重启后台".into());
        }
        if package && negotiated.feature_bits & features::CREDENTIAL_PACKAGE == 0 {
            return Err(
                "daemon does not support encrypted credential packages; update and restart it"
                    .into(),
            );
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::VaultRequest(request)),
            },
        )
        .await
        .map_err(|_| "Vault write failed; refresh before retrying")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "Vault result unknown; refresh before retrying")?;
        if response.request_id != 2 {
            return Err("Vault response mismatch".into());
        }
        match response.payload {
            Some(envelope::Payload::VaultResponse(response)) => Ok(response),
            _ => Err("Vault response unavailable".into()),
        }
    })
    .await
    .map_err(|_| "Vault result unknown; refresh before retrying".to_owned())?
}

fn request(operation: ProfileOperation) -> ProfileRequest {
    ProfileRequest {
        operation: operation as i32,
        expected_revision: 0,
        changes: Vec::new(),
        import_json: Vec::new(),
        import_policy: ProfileImportPolicy::Fail as i32,
        credential_profile_id: Vec::new(),
        credential_secret: Vec::new(),
        host_key_token: Vec::new(),
        host_key_fingerprint: String::new(),
        include_local_context: false,
        export_profile_ids: Vec::new(),
    }
}

/// A cancelled wait keeps its blocking reader's permit until that reader exits.
async fn prepare_import_source(
    source: &ProfileImportSource,
) -> Result<(Vec<u8>, Vec<String>), String> {
    let Some(input) = source.format.batch_input() else {
        return prepare_import_details(read_import_file(&source.path).await?, source);
    };
    static READERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let permit = READERS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Migration readers are still occupied; wait for them to finish")?;
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct CancelRead(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for CancelRead {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let _cancel = CancelRead(cancelled.clone());
    let source = source.clone();
    let owner = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let converted = cshell_import::load_foreign_batch(
            std::path::Path::new(source.path.trim()),
            input,
            &source.default_username,
            &cancelled,
        )
        .map_err(|e| e.to_string())?;
        let notices = converted.summary.notices();
        let bytes = serde_json::to_vec(&converted.document)
            .map_err(|_| "Converted migration document could not be encoded".to_owned())?;
        Ok::<_, String>((bytes, notices))
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), owner)
        .await
        .map_err(|_| "Migration read timed out; any remaining reader retains its slot".to_owned())?
        .map_err(|_| "Migration reader could not complete".to_owned())?
}

async fn read_import_file(path: &str) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let path = PathBuf::from(path.trim());
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| "Import file cannot be opened")?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| "Import file metadata is unavailable")?;
    if !metadata.is_file() || metadata.len() > MAX_IMPORT_FILE_BYTES {
        return Err("Profile import file exceeds 1 MiB".into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_IMPORT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "Import file cannot be read")?;
    if bytes.len() as u64 > MAX_IMPORT_FILE_BYTES {
        return Err("Profile import file exceeds 1 MiB".into());
    }
    Ok(std::mem::take(&mut *bytes))
}

fn prepare_import_details(
    mut bytes: Vec<u8>,
    source: &ProfileImportSource,
) -> Result<(Vec<u8>, Vec<String>), String> {
    if source.format.reviewed_metadata() {
        let converted: Result<cshell_application::ForeignProfileImportResult, String> = match source
            .format
        {
            ProfileImportFormat::PuttyRegistry => cshell_application::import_putty_registry(
                &bytes,
                cshell_application::PuttyImportOptions {
                    default_username: source.default_username.trim(),
                },
            )
            .map(Into::into)
            .map_err(|e| e.to_string()),
            ProfileImportFormat::MobaXtermUnicode | ProfileImportFormat::MobaXtermWindows1252 => {
                cshell_application::import_mobaxterm_sessions(
                    &bytes,
                    cshell_application::MobaXtermImportOptions {
                        default_username: source.default_username.trim(),
                        encoding: if source.format == ProfileImportFormat::MobaXtermWindows1252 {
                            cshell_application::MobaXtermEncoding::Windows1252
                        } else {
                            cshell_application::MobaXtermEncoding::Unicode
                        },
                    },
                )
                .map_err(|e| e.to_string())
            }
            ProfileImportFormat::SecureCrtIni | ProfileImportFormat::XshellSession => {
                let name = std::path::Path::new(source.path.trim())
                    .file_stem()
                    .and_then(|name| name.to_str());
                match name {
                    Some(session_name) if source.format == ProfileImportFormat::XshellSession => {
                        cshell_application::import_xshell_session(
                            &bytes,
                            cshell_application::XshellImportOptions {
                                session_name,
                                default_username: source.default_username.trim(),
                            },
                        )
                        .map_err(|e| e.to_string())
                    }
                    Some(session_name) => cshell_application::import_securecrt_ini(
                        &bytes,
                        cshell_application::SecureCrtImportOptions {
                            session_name,
                            default_username: source.default_username.trim(),
                        },
                    )
                    .map_err(|e| e.to_string()),
                    None => Err("Session filename must be UTF-8".into()),
                }
            }
            _ => Err("Unsupported metadata format".into()),
        };
        bytes.zeroize();
        let converted = converted?;
        let notices = converted.summary.notices();
        let bytes = serde_json::to_vec(&converted.document)
            .map_err(|_| "Converted session document could not be encoded")?;
        return Ok((bytes, notices));
    }
    Ok((prepare_standard_import_bytes(bytes, source)?, Vec::new()))
}
#[cfg(test)]
fn prepare_import_bytes(bytes: Vec<u8>, source: &ProfileImportSource) -> Result<Vec<u8>, String> {
    prepare_import_details(bytes, source).map(|(bytes, _)| bytes)
}
fn prepare_standard_import_bytes(
    bytes: Vec<u8>,
    source: &ProfileImportSource,
) -> Result<Vec<u8>, String> {
    if source.format == ProfileImportFormat::Theme
        && std::path::Path::new(&source.path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
    {
        let document =
            cshell_application::ThemeDocument::from_toml(&bytes).map_err(|e| e.to_string())?;
        return serde_json::to_vec(&document)
            .map_err(|_| "TOML conversion could not be encoded".into());
    }
    if source.format == ProfileImportFormat::Theme
        && std::path::Path::new(&source.path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("xcs"))
    {
        let document =
            cshell_application::ThemeDocument::from_xcs(&bytes).map_err(|e| e.to_string())?;
        return serde_json::to_vec(&document)
            .map_err(|_| "XCS conversion could not be encoded".into());
    }
    if source.format != ProfileImportFormat::OpenSsh {
        return Ok(bytes);
    }
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let document = cshell_application::import_openssh_config(
        &bytes,
        cshell_application::OpenSshImportOptions {
            default_username: source.default_username.trim(),
            home_directory: home.as_deref(),
        },
    )
    .map_err(|error| error.to_string())?;
    let bytes =
        serde_json::to_vec(&document).map_err(|_| "Converted import could not be encoded")?;
    if bytes.len() as u64 > MAX_IMPORT_FILE_BYTES {
        return Err("Converted import exceeds 1 MiB".into());
    }
    Ok(bytes)
}

async fn send(
    config: &DesktopConnectionConfig,
    request: ProfileRequest,
) -> Result<ProfileResponse, String> {
    let timeout = if matches!(
        ProfileOperation::try_from(request.operation),
        Ok(ProfileOperation::PreviewHostKey | ProfileOperation::ConfirmHostKey)
    ) {
        std::time::Duration::from_secs(40)
    } else if matches!(
        ProfileOperation::try_from(request.operation),
        Ok(ProfileOperation::SetPassword
            | ProfileOperation::DeletePassword
            | ProfileOperation::SetKeyPassphrase
            | ProfileOperation::DeleteKeyPassphrase)
    ) {
        std::time::Duration::from_secs(30)
    } else {
        std::time::Duration::from_secs(5)
    };
    tokio::time::timeout(timeout, send_inner(config, request))
        .await
        .map_err(|_| "Profile request timed out".to_owned())?
}

async fn send_inner(
    config: &DesktopConnectionConfig,
    request: ProfileRequest,
) -> Result<ProfileResponse, String> {
    let resolved = config.resolve().map_err(|error| error.to_string())?;
    let mut stream = transport::connect(&resolved.endpoint)
        .await
        .map_err(|error| error.to_string())?;
    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::VAULT_CONTROL
        | features::VAULT_SESSION_STATE
        | features::PROFILE_CONTROL
        | features::SSH_PROFILE_TARGET
        | features::SSH_PROFILE_SESSION
        | features::SSH_PROFILE_AUTH
        | features::SSH_HOST_KEY_IMPORT
        | features::SSH_PROFILE_ROUTE
        | features::LOCAL_PROFILE
        | features::LOCAL_LAUNCH_OPTIONS
        | features::PROFILE_CONNECTION_IMPORT
        | features::PROFILE_TRANSFER
        | features::THEME_TRANSFER
        | features::OPENSSH_EXPORT;
    let negotiated = client_handshake(&mut stream, 1, handshake)
        .await
        .map_err(|error| error.to_string())?;
    if negotiated.feature_bits & features::PROFILE_CONTROL == 0 {
        return Err("daemon does not support Profile control".into());
    }
    if negotiated.protocol_minor<65 && request.changes.iter().any(|change|matches!(&change.change,Some(cshell_ipc::profile_change::Change::UpsertSshConnection(target)) if target.options.as_ref().is_some_and(|options|options.sftp_verification.is_some()))) {
        return Err("请先一起更新 GUI 和后台，以保存会话的 SFTP 校验策略".into());
    }
    if matches!(
        ProfileOperation::try_from(request.operation),
        Ok(ProfileOperation::PreviewImport | ProfileOperation::CommitImport)
    ) && cshell_application::ProfileImportDocument::parse(&request.import_json).is_ok_and(
        |document| document.version >= cshell_application::PROFILE_CONNECTION_IMPORT_VERSION,
    ) && negotiated.feature_bits & features::PROFILE_CONNECTION_IMPORT == 0
    {
        return Err(
            "daemon does not support Profile connection import; update and restart it".into(),
        );
    }
    let transfer = request.operation == ProfileOperation::ExportCatalog as i32
        || (matches!(
            ProfileOperation::try_from(request.operation),
            Ok(ProfileOperation::PreviewImport | ProfileOperation::CommitImport)
        ) && cshell_application::ProfileImportDocument::parse(&request.import_json).is_ok_and(
            |document| document.version == cshell_application::PROFILE_TRANSFER_VERSION,
        ));
    if transfer && negotiated.feature_bits & features::PROFILE_TRANSFER == 0 {
        return Err(
            "daemon does not support Profile export or local import; update and restart it".into(),
        );
    }
    let openssh_request = request.operation == ProfileOperation::ExportOpenSsh as i32;
    if openssh_request && negotiated.feature_bits & features::OPENSSH_EXPORT == 0 {
        return Err("daemon does not support OpenSSH export; update and restart it".into());
    }
    let theme_request = matches!(
        ProfileOperation::try_from(request.operation),
        Ok(ProfileOperation::PreviewThemes
            | ProfileOperation::CommitThemes
            | ProfileOperation::ExportThemes)
    );
    if theme_request && negotiated.feature_bits & features::THEME_TRANSFER == 0 {
        return Err("daemon does not support Theme transfer; update and restart it".into());
    }
    if !theme_request
        && !openssh_request
        && (negotiated.feature_bits & features::SSH_PROFILE_TARGET == 0
            || negotiated.feature_bits & features::SSH_PROFILE_SESSION == 0
            || negotiated.feature_bits & features::SSH_PROFILE_AUTH == 0
            || negotiated.feature_bits & features::SSH_HOST_KEY_IMPORT == 0
            || negotiated.feature_bits & features::SSH_PROFILE_ROUTE == 0
            || negotiated.feature_bits & features::LOCAL_PROFILE == 0
            || negotiated.feature_bits & features::LOCAL_LAUNCH_OPTIONS == 0)
    {
        return Err(
            "daemon does not support current Profile configuration; restart the daemon".into(),
        );
    }
    write_envelope(
        &mut stream,
        &Envelope {
            request_id: 2,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::ProfileRequest(request)),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let response = read_envelope(&mut stream)
        .await
        .map_err(|error| error.to_string())?;
    if response.request_id != 2 {
        return Err("daemon returned mismatched Profile response".into());
    }
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("daemon returned unexpected Profile response".into());
    };
    Ok(response)
}

async fn discover_local_shells(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<DesktopProfileView>>,
) {
    let result = send(config, request(ProfileOperation::DiscoverLocalShells))
        .await
        .and_then(|response| {
            check_response(&response)?;
            Ok(response.local_shells)
        });
    update(shared, |view| match result {
        Ok(shells) => {
            view.local_shells = shells;
            view.error = None;
        }
        Err(error) => view.error = Some(error),
    });
}

fn check_response(response: &ProfileResponse) -> Result<(), String> {
    let status = ProfileStatus::try_from(response.status)
        .map_err(|_| "daemon returned unknown Profile status")?;
    if status == ProfileStatus::Ok {
        Ok(())
    } else if response.detail.is_empty() {
        Err(format!("Profile operation failed: {status:?}"))
    } else {
        Err(response.detail.clone())
    }
}

fn catalog_from_response(response: ProfileResponse) -> Result<DesktopProfileCatalog, String> {
    check_response(&response)?;
    let data = response.catalog.ok_or("daemon omitted Profile catalog")?;
    decode_catalog(data)
}

fn decode_catalog(data: ProfileCatalogData) -> Result<DesktopProfileCatalog, String> {
    let defaults = data
        .defaults
        .ok_or("daemon omitted Profile defaults")?
        .into();
    let folders = data
        .folders
        .into_iter()
        .map(|item| {
            item.try_into()
                .map_err(|error: cshell_ipc::ProfileCodecError| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    let profiles = data
        .profiles
        .into_iter()
        .map(|item| {
            item.try_into()
                .map_err(|error: cshell_ipc::ProfileCodecError| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    let ssh_connections = data
        .ssh_connections
        .into_iter()
        .map(|item| {
            item.try_into()
                .map_err(|error: cshell_ipc::ProfileCodecError| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    let local_connections = data
        .local_connections
        .into_iter()
        .map(|item| {
            item.try_into()
                .map_err(|error: cshell_ipc::ProfileCodecError| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    let themes: Vec<cshell_domain::ThemeRecord> = data
        .themes
        .into_iter()
        .map(|theme| {
            theme
                .try_into()
                .map_err(|error: cshell_ipc::ProfileCodecError| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    cshell_application::validate_themes(&themes).map_err(|error| error.to_string())?;
    Ok(DesktopProfileCatalog {
        revision: data.revision,
        defaults,
        folders,
        profiles,
        ssh_connections,
        local_connections,
        themes,
    })
}

fn publish_catalog(
    shared: &Arc<Mutex<DesktopProfileView>>,
    result: Result<DesktopProfileCatalog, String>,
) {
    update(shared, |view| match result {
        Ok(catalog) => {
            if view
                .catalog
                .as_ref()
                .is_some_and(|old| old.revision != catalog.revision)
            {
                view.host_key_preview = None;
            }
            view.status = format!("Profiles revision {}", catalog.revision);
            view.catalog = Some(catalog);
            view.preview = None;
            view.preview_source = None;
            view.error = None;
        }
        Err(error) => {
            view.error = Some(error);
        }
    });
}

fn update(shared: &Arc<Mutex<DesktopProfileView>>, change: impl FnOnce(&mut DesktopProfileView)) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    change(&mut view);
    view.generation = view.generation.wrapping_add(1);
}

async fn export_openssh(
    config: &DesktopConnectionConfig,
    catalog: Option<DesktopProfileCatalog>,
    path: &str,
    expected_revision: u64,
    selected: &[ProfileId],
) -> Result<(), String> {
    let catalog = catalog.ok_or("Saved catalog unavailable")?;
    if catalog.revision != expected_revision {
        return Err("Catalog changed; refresh before export".into());
    }
    let expected = cshell_application::export_openssh_config(&catalog.export_snapshot(), selected)
        .map_err(|error| error.to_string())?;
    let mut outgoing = request(ProfileOperation::ExportOpenSsh);
    outgoing.expected_revision = expected_revision;
    outgoing.export_profile_ids = selected
        .iter()
        .map(|id| id.as_uuid().as_bytes().to_vec())
        .collect();
    let response = send(config, outgoing).await?;
    save_openssh_response(&response, expected_revision, &expected, path)
}

fn save_openssh_response(
    response: &ProfileResponse,
    expected_revision: u64,
    expected: &[u8],
    path: &str,
) -> Result<(), String> {
    check_response(response)?;
    if response.revision != expected_revision || response.export_json != expected {
        return Err(
            "OpenSSH export did not match the saved catalog; refresh before retrying".into(),
        );
    }
    crate::profile_export::save_export(path, &response.export_json)
}

fn publish_host_key_preview(view: &mut DesktopProfileView, result: Result<HostKeyPreviewData, String>) {
    view.host_key_pending = false;
    view.host_key_existing_entry = false;
    match result {
        Ok(preview) => {
            view.host_key_preview = Some(preview);
            view.error = None;
            view.status = "请核对服务器指纹并确认信任".into();
        }
        Err(error) if error.contains("host already has a known_hosts entry; first-key import is blocked") => {
            // Existing daemons report this as a first-import conflict. It is a
            // read-only record notice, never evidence that the peer was verified.
            view.host_key_preview = None;
            view.host_key_existing_entry = true;
            view.error = None;
            view.status = "此主机已有密钥记录，无需再次导入；连接时将自动校验。".into();
        }
        Err(error) => {
            view.host_key_preview = None;
            view.error = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_host_key_preview_is_a_notice_but_security_failures_remain_errors() {
        let mut view = DesktopProfileView { host_key_pending: true, ..Default::default() };
        publish_host_key_preview(&mut view, Err("Conflict: host already has a known_hosts entry; first-key import is blocked".into()));
        assert!(view.host_key_existing_entry);
        assert!(!view.host_key_pending);
        assert!(view.error.is_none());
        assert!(view.host_key_preview.is_none());
        assert!(view.status.contains("无需再次导入"));
        for error in ["HostKeyChanged", "HostKeyRevoked", "VaultLocked"] {
            publish_host_key_preview(&mut view, Err(error.into()));
            assert!(!view.host_key_existing_entry);
            assert_eq!(view.error.as_deref(), Some(error));
        }
    }

    #[test]
    fn profile_connection_exit_locks_vault_even_when_desktop_status_is_unknown() {
        for state in [None, Some(cshell_ipc::VaultSessionState::Active),
            Some(cshell_ipc::VaultSessionState::Locked), Some(cshell_ipc::VaultSessionState::Unavailable)] {
            let (lock_sender, mut locks) = tokio::sync::mpsc::channel(1);
            let connection = DesktopProfileConnection {
                shared: Arc::new(Mutex::new(DesktopProfileView {
                    vault: state.map(|state| cshell_ipc::VaultResponse {
                        exists: true, unlocked: true, session_state: state as i32, ..Default::default()
                    }), ..Default::default()
                })),
                sender: None, worker: None, lock_sender: Some(lock_sender), lock_worker: None,
            };
            assert!(locks.try_recv().is_err());
            drop(connection);
            assert!(locks.try_recv().is_ok(), "program exit must clear the unlocked Vault");
        }
    }

    #[test]
    fn repeated_password_removal_is_bounded_and_queue_failure_is_visible() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let connection = DesktopProfileConnection {
            shared: Arc::new(Mutex::new(DesktopProfileView::default())),
            sender: Some(sender), worker: None, lock_sender: None, lock_worker: None,
        };
        let id = ProfileId::new();
        let remove = || ProfileClientCommand::DeletePassword { profile_id: id, expected_revision: 1 };
        assert!(connection.request(remove()));
        for _ in 0..200 { assert!(!connection.request(remove())); }
        assert!(connection.view().credential_pending);
        assert!(matches!(receiver.try_recv().unwrap(), ProfileClientCommand::DeletePassword { .. }));
        assert!(receiver.try_recv().is_err());
        publish_credential_result(&connection.shared, Ok(()), "密码已从凭据存储移除");
        update(&connection.shared, |view| view.credential_pending = false);
        assert!(connection.request(remove()), "completion must allow another deliberate operation");
        receiver.close();
        update(&connection.shared, |view| view.credential_pending = false);
        assert!(!connection.request(remove()));
        assert!(!connection.view().credential_pending);
        assert!(connection.view().error.is_some());
    }

    #[test]
    fn password_save_uses_committed_revision_and_rejects_a_changed_target() {
        let id = ProfileId::new();
        let target = SshConnectionRecord {
            options: Default::default(),
            profile_id: id, host: "host.example.test".into(), port: 22,
            username: "operator".into(), auth_method: cshell_domain::SshAuthMethod::Password,
            private_key_path: None, certificate_path: None,
            agent_backend: cshell_domain::SshAgentBackend::Auto, agent_identity: None,
            route: cshell_domain::SshRoute::Direct,
        };
        let changes = vec![ProfileChange { change: Some(cshell_ipc::profile_change::Change::UpsertSshConnection(cshell_ipc::SshConnectionData::from(&target))) }];
        let mut catalog = DesktopProfileCatalog {
            revision: 42, defaults: Default::default(), folders: vec![], profiles: vec![],
            ssh_connections: vec![target], local_connections: vec![], themes: vec![],
        };
        let mut password = Zeroizing::new(b"test-only-secret".to_vec());
        catalog.ssh_connections[0].host = "different.example.test".into();
        assert!(password_request_for_saved_target(&catalog, id, &changes, &mut password).is_err());
        assert_eq!(&*password, b"test-only-secret");
        catalog.ssh_connections[0].host = "host.example.test".into();
        let mut request = password_request_for_saved_target(&catalog, id, &changes, &mut password).unwrap();
        assert_eq!(request.expected_revision, 42);
        assert_eq!(request.credential_profile_id, id.as_uuid().as_bytes());
        assert_eq!(request.credential_secret, b"test-only-secret");
        assert!(password.is_empty());
        request.credential_secret.zeroize();
    }

    #[test]
    fn openssh_response_validation_precedes_publication_and_never_overwrites()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp
            .path()
            .join("ssh-config")
            .to_string_lossy()
            .into_owned();
        let expected = b"Host saved\n";
        let mut response = ProfileResponse {
            status: ProfileStatus::Ok as i32,
            revision: 7,
            export_json: expected.to_vec(),
            ..Default::default()
        };
        assert!(save_openssh_response(&response, 8, expected, &path).is_err());
        response.export_json = b"Host unexpected\n".to_vec();
        assert!(save_openssh_response(&response, 7, expected, &path).is_err());
        response.export_json = expected.to_vec();
        response.status = ProfileStatus::Conflict as i32;
        assert!(save_openssh_response(&response, 7, expected, &path).is_err());
        assert!(std::fs::read_dir(temp.path())?.next().is_none());
        response.status = ProfileStatus::Ok as i32;
        save_openssh_response(&response, 7, expected, &path)?;
        assert_eq!(std::fs::read(&path)?, expected);
        assert!(save_openssh_response(&response, 7, expected, &path).is_err());
        assert_eq!(std::fs::read(&path)?, expected);
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn openssh_desktop_file_conversion_is_bounded_and_does_not_send_comments()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("config");
        let marker = "source-comment-not-for-ipc";
        std::fs::write(
            &path,
            format!("# {marker}\nHost app\nHostName app.test\nPort 2222\n"),
        )?;
        let source = ProfileImportSource {
            path: path.to_string_lossy().into_owned(),
            policy: ProfileImportPolicy::Fail,
            format: ProfileImportFormat::OpenSsh,
            default_username: "operator".into(),
        };
        let converted = prepare_import_bytes(read_import_file(&source.path).await?, &source)?;
        assert!(!String::from_utf8_lossy(&converted).contains(marker));
        std::fs::write(&path, "Host changed\n")?;
        let document = cshell_application::ProfileImportDocument::parse(&converted)?;
        assert_eq!(document.ssh_connections[0].host, "app.test");
        assert_eq!(document.ssh_connections[0].username, "operator");
        assert_eq!(document.ssh_connections[0].port, 2222);
        std::fs::write(&path, vec![b'x'; MAX_IMPORT_FILE_BYTES as usize + 1])?;
        assert!(read_import_file(&source.path).await.is_err());
        assert!(
            read_import_file(temp.path().to_str().ok_or("test path must be UTF-8")?)
                .await
                .is_err()
        );
        assert!(!format!("{source:?}").contains("operator"));
        Ok(())
    }

    #[test]
    fn desktop_profile_diagnostics_hide_tokens_environment_and_error_text() {
        let marker = "SECRET-DESKTOP-MARKER";
        let view = DesktopProfileView {
            generation: 7,
            catalog: Some(DesktopProfileCatalog {
                themes: Vec::new(),
                revision: 3,
                defaults: Default::default(),
                folders: vec![],
                profiles: vec![],
                ssh_connections: vec![],
                local_connections: vec![LocalConnectionRecord {
                    profile_id: ProfileId::new(),
                    program: marker.into(),
                    args: vec![marker.into()],
                    cwd: cshell_domain::LocalWorkingDirectory::Home,
                    close_policy: Default::default(),
                    env_overrides: [("API_TOKEN".into(), marker.into())].into(),
                }],
            }),
            error: Some(marker.into()),
            status: marker.into(),
            preview_source: Some(ProfileImportSource {
                path: marker.into(),
                policy: ProfileImportPolicy::Fail,
                format: ProfileImportFormat::Catalog,
                default_username: marker.into(),
            }),
            host_key_preview: Some(HostKeyPreviewData {
                token: marker.as_bytes().to_vec(),
                profile_id: vec![],
                host: String::new(),
                port: 22,
                algorithm: String::new(),
                fingerprint: String::new(),
                public_key_line: String::new(),
                expires_unix_seconds: 0,
            }),
            ..Default::default()
        };
        let diagnostic = format!("{view:?}");
        assert!(diagnostic.contains("generation: 7"));
        assert!(diagnostic.contains("revision: 3"));
        assert!(!diagnostic.contains(marker));
        assert!(!diagnostic.contains("83, 69, 67"));
        let command = ProfileClientCommand::ConfirmHostKey {
            profile_id: ProfileId::new(),
            expected_revision: 3,
            token: Zeroizing::new(marker.as_bytes().to_vec()),
            fingerprint: marker.into(),
        };
        assert!(!format!("{command:?}").contains(marker));
    }
}
