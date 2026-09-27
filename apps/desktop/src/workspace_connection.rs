//! Background workspace load and serialized compare-and-swap autosaves.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::{WorkspaceSnapshot, decode_workspace, encode_workspace};
use cshell_domain::WorkspaceDocument;
use cshell_ipc::{
    Envelope, Handshake, WorkspaceOperation, WorkspaceRequest, WorkspaceResponse, WorkspaceStatus,
    client_handshake, envelope, features, read_envelope, transport, write_envelope,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct WorkspaceView {
    pub ready: bool,
    pub snapshot: Option<WorkspaceSnapshot>,
    pub error: Option<String>,
    pub status: String,
}

#[derive(Debug)]
pub struct WorkspaceConnection {
    shared: Arc<Mutex<WorkspaceView>>,
    sender: Option<tokio::sync::watch::Sender<Option<WorkspaceDocument>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WorkspaceConnection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(WorkspaceView {
            status: "Loading workspace".into(),
            ..Default::default()
        }));
        let (sender, mut receiver) = tokio::sync::watch::channel::<Option<WorkspaceDocument>>(None);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("cshell-workspace-ipc".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .error = Some(error.to_string());
                        worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .ready = true;
                        return;
                    }
                };
                runtime.block_on(async move {
                    // Starting a second companion is harmless: the existing single-instance guard owns the runtime.
                    if config.resolve().is_err() {
                        let _result = config.start_companion_daemon();
                    }
                    let mut last_start = std::time::Instant::now();
                    let loaded = loop {
                        let result = send(&config, WorkspaceRequest::default()).await;
                        if result.is_ok() {
                            break result;
                        }
                        if last_start.elapsed() >= Duration::from_secs(5) {
                            let _result = config.start_companion_daemon();
                            last_start = std::time::Instant::now();
                        }
                        worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .status = "Waiting for workspace storage".into();
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                            changed = receiver.changed() => { if changed.is_err() { return; } }
                        }
                        // IPC capability/storage failures are returned as responses and stop retries below.
                    };
                    let mut revision = 0;
                    let mut blocked = false;
                    {
                        let mut view = worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        view.ready = true;
                        match loaded.and_then(decode_response) {
                            Ok(snapshot) => {
                                revision = snapshot.as_ref().map_or(0, |s| s.revision);
                                view.status = if snapshot.is_some() {
                                    "Workspace restored; terminals wait for explicit open"
                                } else {
                                    "Workspace autosave ready"
                                }
                                .into();
                                view.snapshot = snapshot;
                            }
                            Err(error) => {
                                view.error = Some(error);
                                blocked = true;
                            }
                        }
                    }
                    while receiver.changed().await.is_ok() {
                        let document = receiver.borrow_and_update().clone();
                        let Some(document) = document else {
                            continue;
                        };
                        if blocked {
                            continue;
                        }
                        let json = match encode_workspace(&document) {
                            Ok(json) => json,
                            Err(error) => {
                                worker_shared
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .error = Some(error.to_string());
                                continue;
                            }
                        };
                        let result = send(
                            &config,
                            WorkspaceRequest {
                                operation: WorkspaceOperation::Save as i32,
                                expected_revision: revision,
                                document_json: json,
                            },
                        )
                        .await
                        .and_then(decode_response);
                        let mut view = worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match result {
                            Ok(Some(snapshot)) => {
                                revision = snapshot.revision;
                                view.snapshot = Some(snapshot);
                                view.status = "Workspace saved".into();
                                view.error = None;
                            }
                            Ok(None) => {
                                blocked = true;
                                view.error = Some("Workspace save returned no document".into());
                            }
                            Err(error) => {
                                // A lost reply or competing editor must not be replayed or overwritten automatically.
                                blocked = true;
                                view.error = Some(format!(
                                    "{error}; autosave paused. Reopen the application to reload."
                                ));
                            }
                        }
                    }
                });
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> WorkspaceView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn save(&self, document: &WorkspaceDocument) {
        if let Some(sender) = &self.sender {
            sender.send_if_modified(|current| {
                if current.as_ref() == Some(document) {
                    return false;
                }
                *current = Some(document.clone());
                true
            });
        }
    }
}

impl Drop for WorkspaceConnection {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}

fn decode_response(response: WorkspaceResponse) -> Result<Option<WorkspaceSnapshot>, String> {
    if response.status != WorkspaceStatus::Ok as i32 {
        return Err(response.detail);
    }
    if response.document_json.is_empty() {
        return if response.revision == 0 {
            Ok(None)
        } else {
            Err("Workspace revision has no document".into())
        };
    }
    if response.revision == 0 {
        return Err("Workspace document has no revision".into());
    }
    Ok(Some(WorkspaceSnapshot {
        revision: response.revision,
        document: decode_workspace(&response.document_json).map_err(|error| error.to_string())?,
    }))
}

async fn send(
    config: &DesktopConnectionConfig,
    request: WorkspaceRequest,
) -> Result<WorkspaceResponse, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let resolved = config.resolve().map_err(|e| e.to_string())?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|e| e.to_string())?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        handshake.feature_bits = features::WORKSPACE_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|e| e.to_string())?;
        if negotiated.feature_bits & features::WORKSPACE_CONTROL == 0 {
            return Ok(WorkspaceResponse {
                status: WorkspaceStatus::Unsupported as i32,
                detail: "Workspace control requires the current daemon".into(),
                ..Default::default()
            });
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::WorkspaceRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|e| e.to_string())?;
        if response.request_id != 2 {
            return Err("Mismatched workspace reply".into());
        }
        let Some(envelope::Payload::WorkspaceResponse(response)) = response.payload else {
            return Err("Unexpected workspace reply".into());
        };
        Ok(response)
    })
    .await
    .map_err(|_| "Workspace request timed out; result may be unknown".to_owned())?
}
