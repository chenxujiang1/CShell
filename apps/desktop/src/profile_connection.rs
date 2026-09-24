use crate::daemon_connection::DesktopConnectionConfig;
use cshell_domain::{
    ProfileFolder, ProfileId, ProfileRecord, SshConnectionRecord, TerminalDefaults,
};
use cshell_ipc::{
    Envelope, Handshake, ProfileCatalogData, ProfileChange, ProfileImportPolicy,
    ProfileImportPreviewData, ProfileOperation, ProfileRequest, ProfileResponse, ProfileStatus,
    client_handshake, envelope, features, read_envelope, transport, write_envelope,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_IMPORT_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct DesktopProfileView {
    pub generation: u64,
    pub catalog: Option<DesktopProfileCatalog>,
    pub preview: Option<ProfileImportPreviewData>,
    pub preview_source: Option<(String, ProfileImportPolicy)>,
    pub error: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct DesktopProfileCatalog {
    pub revision: u64,
    pub defaults: TerminalDefaults,
    pub folders: Vec<ProfileFolder>,
    pub profiles: Vec<ProfileRecord>,
    pub ssh_connections: Vec<SshConnectionRecord>,
}

#[derive(Debug)]
pub enum ProfileClientCommand {
    Refresh,
    OpenProfile(ProfileId),
    SetPassword {
        profile_id: ProfileId,
        expected_revision: u64,
        password: Vec<u8>,
    },
    DeletePassword {
        profile_id: ProfileId,
        expected_revision: u64,
    },
    Apply {
        expected_revision: u64,
        changes: Vec<ProfileChange>,
    },
    PreviewImport {
        path: String,
        policy: ProfileImportPolicy,
    },
    CommitImport {
        expected_revision: u64,
    },
}

#[derive(Debug)]
pub struct DesktopProfileConnection {
    shared: Arc<Mutex<DesktopProfileView>>,
    sender: Option<tokio::sync::mpsc::Sender<ProfileClientCommand>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DesktopProfileConnection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(DesktopProfileView {
            status: "Profiles loading".into(),
            ..DesktopProfileView::default()
        }));
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        let worker_shared = Arc::clone(&shared);
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
        })
    }

    pub fn view(&self) -> DesktopProfileView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn request(&self, command: ProfileClientCommand) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(command).is_ok())
    }
}

impl Drop for DesktopProfileConnection {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn profile_worker(
    config: DesktopConnectionConfig,
    shared: Arc<Mutex<DesktopProfileView>>,
    mut receiver: tokio::sync::mpsc::Receiver<ProfileClientCommand>,
) {
    let mut pending_import: Option<(Vec<u8>, ProfileImportPolicy, u64)> = None;
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = refresh.tick() => {
                if shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).catalog.is_none() {
                    let result = send(&config, request(ProfileOperation::List)).await.and_then(catalog_from_response);
                    publish_catalog(&shared, result);
                }
            }
            next = receiver.recv() => {
                let Some(command) = next else { break };
                match command {
                    ProfileClientCommand::OpenProfile(_) => {}
                    ProfileClientCommand::SetPassword { profile_id, expected_revision, password } => {
                        let mut outgoing = request(ProfileOperation::SetPassword);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        outgoing.credential_secret = password;
                        let result = send(&config, outgoing).await.and_then(|response| check_response(&response));
                        publish_credential_result(&shared, result, "Password saved in system keychain");
                    }
                    ProfileClientCommand::DeletePassword { profile_id, expected_revision } => {
                        let mut outgoing = request(ProfileOperation::DeletePassword);
                        outgoing.expected_revision = expected_revision;
                        outgoing.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
                        let result = send(&config, outgoing).await.and_then(|response| check_response(&response));
                        publish_credential_result(&shared, result, "Password removed from system keychain");
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
                    ProfileClientCommand::PreviewImport { path, policy } => {
                        let result: Result<(Vec<u8>, ProfileImportPreviewData), String> = async {
                            let bytes = read_import_file(&path).await?;
                            let mut outgoing = request(ProfileOperation::PreviewImport);
                            outgoing.import_json = bytes.clone();
                            outgoing.import_policy = policy as i32;
                            let response = send(&config, outgoing).await?;
                            check_response(&response)?;
                            let preview = response.preview.ok_or("daemon omitted import preview")?;
                            Ok((bytes, preview))
                        }.await;
                        match result {
                            Ok((bytes, preview)) => {
                                pending_import = Some((bytes, policy, preview.base_revision));
                                update(&shared, |view| {
                                    view.preview_source = Some((path, policy));
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
                    ProfileClientCommand::CommitImport { expected_revision } => {
                        let result = if let Some((bytes, policy, preview_revision)) = &pending_import {
                            if *preview_revision != expected_revision {
                                Err("import preview is stale; preview again".into())
                            } else {
                                let mut outgoing = request(ProfileOperation::CommitImport);
                                outgoing.expected_revision = expected_revision;
                                outgoing.import_json = bytes.clone();
                                outgoing.import_policy = *policy as i32;
                                send(&config, outgoing).await.and_then(catalog_from_response)
                            }
                        } else {
                            Err("preview an import file first".into())
                        };
                        if result.is_ok() { pending_import = None; }
                        publish_catalog(&shared, result);
                        update(&shared, |view| { if view.error.is_none() { view.preview = None; } });
                    }
                }
            }
        }
    }
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

fn request(operation: ProfileOperation) -> ProfileRequest {
    ProfileRequest {
        operation: operation as i32,
        expected_revision: 0,
        changes: Vec::new(),
        import_json: Vec::new(),
        import_policy: ProfileImportPolicy::Fail as i32,
        credential_profile_id: Vec::new(),
        credential_secret: Vec::new(),
    }
}

async fn read_import_file(path: &str) -> Result<Vec<u8>, String> {
    let path = PathBuf::from(path.trim());
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| error.to_string())?;
    if metadata.len() > MAX_IMPORT_FILE_BYTES {
        return Err("Profile import file exceeds 1 MiB".into());
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_IMPORT_FILE_BYTES {
        return Err("Profile import file exceeds 1 MiB".into());
    }
    Ok(bytes)
}

async fn send(
    config: &DesktopConnectionConfig,
    request: ProfileRequest,
) -> Result<ProfileResponse, String> {
    let timeout = if matches!(
        ProfileOperation::try_from(request.operation),
        Ok(ProfileOperation::SetPassword | ProfileOperation::DeletePassword)
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
    handshake.feature_bits =
        features::PROFILE_CONTROL | features::SSH_PROFILE_TARGET | features::SSH_PROFILE_SESSION;
    let negotiated = client_handshake(&mut stream, 1, handshake)
        .await
        .map_err(|error| error.to_string())?;
    if negotiated.feature_bits & features::PROFILE_CONTROL == 0 {
        return Err("daemon does not support Profile control".into());
    }
    if negotiated.feature_bits & features::SSH_PROFILE_TARGET == 0
        || negotiated.feature_bits & features::SSH_PROFILE_SESSION == 0
    {
        return Err("daemon does not support SSH Profile sessions; restart the daemon".into());
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
    Ok(DesktopProfileCatalog {
        revision: data.revision,
        defaults,
        folders,
        profiles,
        ssh_connections,
    })
}

fn publish_catalog(
    shared: &Arc<Mutex<DesktopProfileView>>,
    result: Result<DesktopProfileCatalog, String>,
) {
    update(shared, |view| match result {
        Ok(catalog) => {
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
