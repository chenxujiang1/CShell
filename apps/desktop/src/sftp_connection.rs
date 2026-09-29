use crate::daemon_connection::DesktopConnectionConfig;
use cshell_domain::SessionId;
use cshell_ipc::{
    Envelope, Handshake, SftpDirectoryEntry, SftpOperation, SftpRequest, SftpResponse, SftpStatus,
    SftpTransfer, SftpTransferState, client_handshake, envelope, features, read_envelope,
    transport, write_envelope,
};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub struct SftpView {
    pub generation: u64,
    pub session_id: Option<SessionId>,
    pub entries: Vec<SftpDirectoryEntry>,
    pub truncated: bool,
    pub transfer: Option<SftpTransfer>,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum SftpClientCommand {
    List(SessionId, String),
    Recover(SessionId),
    Upload(SessionId, String, String),
    Download(SessionId, String, String),
    Cancel(SessionId, Vec<u8>),
}

#[derive(Debug)]
pub struct DesktopSftpConnection {
    shared: Arc<Mutex<SftpView>>,
    sender: Option<tokio::sync::mpsc::Sender<SftpClientCommand>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DesktopSftpConnection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(SftpView::default()));
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let (shutdown, shutdown_receiver) = tokio::sync::watch::channel(false);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("cshell-sftp-ipc".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        runtime.block_on(run(config, worker_shared, receiver, shutdown_receiver))
                    }
                    Err(error) => {
                        worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .error = Some(error.to_string());
                    }
                }
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn view(&self) -> SftpView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn generation(&self) -> u64 {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }

    pub fn request(&self, command: SftpClientCommand) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(command).is_ok())
    }
}

impl Drop for DesktopSftpConnection {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn run(
    config: DesktopConnectionConfig,
    shared: Arc<Mutex<SftpView>>,
    mut receiver: tokio::sync::mpsc::Receiver<SftpClientCommand>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = poll.tick() => {
                let active = {
                    let view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.session_id.zip(view.transfer.as_ref().filter(|transfer| {
                        transfer.state == SftpTransferState::Running as i32
                    }).map(|transfer| transfer.transfer_id.clone()))
                };
                if let Some((session_id, transfer_id)) = active {
                    let request = request(SftpOperation::TransferStatus, session_id, String::new(), String::new(), transfer_id);
                    let result = tokio::select! {
                        result = send(&config, request) => result,
                        _ = shutdown.changed() => break,
                    };
                    publish(&shared, session_id, result, false);
                }
            }
            command = receiver.recv() => {
                let Some(command) = command else { break };
                let (session_id, request, listing) = match command {
                    SftpClientCommand::List(id, path) => (id, request(SftpOperation::List, id, path, String::new(), Vec::new()), true),
                    SftpClientCommand::Recover(id) => (id, request(SftpOperation::CurrentTransfer, id, String::new(), String::new(), Vec::new()), false),
                    SftpClientCommand::Upload(id, local, remote) => (id, request(SftpOperation::Upload, id, remote, local, Vec::new()), false),
                    SftpClientCommand::Download(id, remote, local) => (id, request(SftpOperation::Download, id, remote, local, Vec::new()), false),
                    SftpClientCommand::Cancel(id, transfer_id) => (id, request(SftpOperation::CancelTransfer, id, String::new(), String::new(), transfer_id), false),
                };
                let result = tokio::select! {
                    result = send(&config, request) => result,
                    _ = shutdown.changed() => break,
                };
                publish(&shared, session_id, result, listing);
            }
        }
    }
}

fn request(
    operation: SftpOperation,
    session_id: SessionId,
    remote_path: String,
    local_path: String,
    transfer_id: Vec<u8>,
) -> SftpRequest {
    SftpRequest {
        operation: operation as i32,
        session_id: session_id.as_uuid().as_bytes().to_vec(),
        remote_path,
        local_path,
        max_entries: if operation == SftpOperation::List {
            512
        } else {
            0
        },
        transfer_id,
    }
}

fn publish(
    shared: &Arc<Mutex<SftpView>>,
    session_id: SessionId,
    result: Result<SftpResponse, String>,
    listing: bool,
) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    view.generation = view.generation.wrapping_add(1);
    match result {
        Ok(response) if response.status == SftpStatus::Ok as i32 => {
            if view.session_id != Some(session_id) {
                view.entries.clear();
                view.transfer = None;
                view.status.clear();
            }
            view.session_id = Some(session_id);
            view.error = None;
            if listing {
                view.entries = response.entries;
                view.truncated = response.truncated;
                view.status = format!("已列出 {} 项", view.entries.len());
            }
            if let Some(transfer) = response.transfer {
                view.status = match SftpTransferState::try_from(transfer.state) {
                    Ok(SftpTransferState::Running) => "传输中".into(),
                    Ok(SftpTransferState::Succeeded) => "传输完成".into(),
                    Ok(SftpTransferState::Cancelled) => "传输已取消".into(),
                    _ => "传输失败".into(),
                };
                if transfer.state == SftpTransferState::Failed as i32 {
                    view.error = Some(transfer.detail.clone());
                }
                view.transfer = Some(transfer);
            }
        }
        Ok(response) => {
            if listing {
                view.entries.clear();
                view.truncated = false;
            }
            view.error = Some(response.detail);
        }
        Err(error) => {
            if listing {
                view.entries.clear();
                view.truncated = false;
            }
            view.error = Some(error);
        }
    }
}

async fn send(
    config: &DesktopConnectionConfig,
    request: SftpRequest,
) -> Result<SftpResponse, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(40),
        send_inner(config, request),
    )
    .await
    .map_err(|_| "SFTP request timed out; check transfer status before retrying".to_owned())?
}

async fn send_inner(
    config: &DesktopConnectionConfig,
    request: SftpRequest,
) -> Result<SftpResponse, String> {
    let resolved = config.resolve().map_err(|error| error.to_string())?;
    let mut stream = transport::connect(&resolved.endpoint)
        .await
        .map_err(|error| error.to_string())?;
    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::SFTP_CONTROL;
    let negotiated = client_handshake(&mut stream, 1, handshake)
        .await
        .map_err(|error| error.to_string())?;
    if negotiated.feature_bits & features::SFTP_CONTROL == 0 {
        return Err("daemon does not support SFTP control; restart the daemon".into());
    }
    write_envelope(
        &mut stream,
        &Envelope {
            request_id: 2,
            payload: Some(envelope::Payload::SftpRequest(request)),
            ..Default::default()
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let response = read_envelope(&mut stream)
        .await
        .map_err(|error| error.to_string())?;
    if response.request_id != 2 {
        return Err("daemon returned a mismatched SFTP response".into());
    }
    match response.payload {
        Some(envelope::Payload::SftpResponse(response)) => Ok(response),
        _ => Err("daemon returned an unexpected SFTP response".into()),
    }
}
