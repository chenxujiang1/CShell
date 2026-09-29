//! SFTP requests and transfers remain owned by the daemon across GUI reconnects.

use crate::{SshSession, SshSessionRegistry};
use cshell_domain::SessionId;
use cshell_ipc::{
    MAX_SFTP_DIRECTORY_ENTRIES, MAX_SFTP_PATH_BYTES, SftpDirectoryEntry, SftpOperation,
    SftpRequest, SftpResponse, SftpStatus, SftpTransfer, SftpTransferState,
};
use cshell_sftp::{RemoteFileType, SftpError, TransferControl, TransferProgress};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_ACTIVE_TRANSFERS: usize = 8;
const MAX_TRACKED_TRANSFERS: usize = 64;

#[derive(Clone, Debug, Default)]
pub(crate) struct SftpIpcService {
    jobs: Arc<Mutex<BTreeMap<Vec<u8>, TransferJob>>>,
}

#[derive(Clone, Debug)]
struct TransferJob {
    session_id: SessionId,
    control: TransferControl,
    snapshot: SftpTransfer,
    started_at: std::time::Instant,
}

impl SftpIpcService {
    pub(crate) async fn handle(
        &self,
        request: SftpRequest,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        let operation = match SftpOperation::try_from(request.operation) {
            Ok(operation) => operation,
            Err(_) => {
                return SftpResponse::with_status(
                    SftpStatus::InvalidRequest,
                    "Unknown SFTP operation",
                );
            }
        };
        let session_id = match <[u8; 16]>::try_from(request.session_id.as_slice()) {
            Ok(bytes) => SessionId::from_bytes(bytes),
            Err(_) => {
                return SftpResponse::with_status(
                    SftpStatus::InvalidRequest,
                    "Invalid SSH session ID",
                );
            }
        };
        match operation {
            SftpOperation::List => self.list(request, session_id, sessions).await,
            SftpOperation::Upload | SftpOperation::Download => {
                self.start(request, operation, session_id, sessions)
            }
            SftpOperation::TransferStatus | SftpOperation::CancelTransfer => {
                self.transfer_status(request, operation, session_id)
            }
            SftpOperation::CurrentTransfer => self.current_transfer(session_id),
        }
    }

    async fn list(
        &self,
        request: SftpRequest,
        session_id: SessionId,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        if !valid_path(&request.remote_path)
            || !(1..=MAX_SFTP_DIRECTORY_ENTRIES).contains(&request.max_entries)
        {
            return SftpResponse::with_status(
                SftpStatus::InvalidRequest,
                "Invalid directory path or page limit",
            );
        }
        let session = match sessions.get(session_id) {
            Ok(session) if session.running() => session,
            _ => {
                return SftpResponse::with_status(
                    SftpStatus::Unavailable,
                    "SSH session is unavailable",
                );
            }
        };
        let client = match session.open_sftp().await {
            Ok(client) => client,
            Err(error) => return SftpResponse::with_status(SftpStatus::Failed, error.to_string()),
        };
        let result = client
            .list_directory(request.remote_path, request.max_entries as usize)
            .await;
        let _ = client.close().await;
        match result {
            Ok(page) => {
                let mut entries = Vec::with_capacity(page.entries.len());
                for entry in page.entries {
                    if !valid_path(&entry.path) || !valid_path(&entry.name) {
                        return SftpResponse::with_status(
                            SftpStatus::Failed,
                            "Remote entry exceeds path limit",
                        );
                    }
                    entries.push(SftpDirectoryEntry {
                        name: entry.name,
                        path: entry.path,
                        kind: match entry.metadata.file_type {
                            RemoteFileType::File => 1,
                            RemoteFileType::Directory => 2,
                            RemoteFileType::Symlink => 3,
                            RemoteFileType::Other => 4,
                        },
                        size: entry.metadata.size,
                        modified_unix_seconds: entry.metadata.modified_unix_seconds,
                    });
                }
                SftpResponse {
                    status: SftpStatus::Ok as i32,
                    detail: String::new(),
                    entries,
                    truncated: page.truncated,
                    transfer: None,
                }
            }
            Err(error) => SftpResponse::with_status(SftpStatus::Failed, error.to_string()),
        }
    }

    fn start(
        &self,
        request: SftpRequest,
        operation: SftpOperation,
        session_id: SessionId,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        if !valid_path(&request.remote_path)
            || !valid_path(&request.local_path)
            || !std::path::Path::new(&request.local_path).is_absolute()
        {
            return SftpResponse::with_status(
                SftpStatus::InvalidRequest,
                "Transfer requires a valid absolute local path",
            );
        }
        let session = match sessions.get(session_id) {
            Ok(session) if session.running() => session,
            _ => {
                return SftpResponse::with_status(
                    SftpStatus::Unavailable,
                    "SSH session is unavailable",
                );
            }
        };
        let transfer_id = SessionId::new().as_uuid().as_bytes().to_vec();
        let control = TransferControl::default();
        let snapshot = SftpTransfer {
            transfer_id: transfer_id.clone(),
            state: SftpTransferState::Running as i32,
            bytes_transferred: 0,
            total_bytes: None,
            detail: String::new(),
        };
        {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.len() >= MAX_TRACKED_TRANSFERS {
                jobs.retain(|_, job| job.snapshot.state == SftpTransferState::Running as i32);
            }
            let active = jobs
                .values()
                .filter(|job| job.snapshot.state == SftpTransferState::Running as i32)
                .count();
            let session_busy = jobs.values().any(|job| {
                job.session_id == session_id
                    && job.snapshot.state == SftpTransferState::Running as i32
            });
            if active >= MAX_ACTIVE_TRANSFERS || jobs.len() >= MAX_TRACKED_TRANSFERS || session_busy
            {
                return SftpResponse::with_status(
                    SftpStatus::CapacityReached,
                    "Too many active SFTP transfers",
                );
            }
            jobs.insert(
                transfer_id.clone(),
                TransferJob {
                    session_id,
                    control: control.clone(),
                    snapshot: snapshot.clone(),
                    started_at: std::time::Instant::now(),
                },
            );
        }
        let jobs = Arc::clone(&self.jobs);
        tokio::spawn(async move {
            run_transfer(jobs, transfer_id, session, request, operation, control).await;
        });
        SftpResponse {
            status: SftpStatus::Ok as i32,
            detail: String::new(),
            entries: Vec::new(),
            truncated: false,
            transfer: Some(snapshot),
        }
    }

    fn current_transfer(&self, session_id: SessionId) -> SftpResponse {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let transfer = jobs
            .values()
            .filter(|job| job.session_id == session_id)
            .max_by_key(|job| job.started_at)
            .map(|job| job.snapshot.clone());
        SftpResponse {
            status: SftpStatus::Ok as i32,
            detail: String::new(),
            entries: Vec::new(),
            truncated: false,
            transfer,
        }
    }

    fn transfer_status(
        &self,
        request: SftpRequest,
        operation: SftpOperation,
        session_id: SessionId,
    ) -> SftpResponse {
        if request.transfer_id.len() != 16 {
            return SftpResponse::with_status(SftpStatus::InvalidRequest, "Invalid transfer ID");
        }
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(job) = jobs
            .get_mut(&request.transfer_id)
            .filter(|job| job.session_id == session_id)
        else {
            return SftpResponse::with_status(
                SftpStatus::Unavailable,
                "SFTP transfer is unavailable",
            );
        };
        if operation == SftpOperation::CancelTransfer
            && job.snapshot.state == SftpTransferState::Running as i32
        {
            job.control.cancel();
            job.snapshot.detail = "Cancellation requested".into();
        }
        SftpResponse {
            status: SftpStatus::Ok as i32,
            detail: String::new(),
            entries: Vec::new(),
            truncated: false,
            transfer: Some(job.snapshot.clone()),
        }
    }
}

async fn run_transfer(
    jobs: Arc<Mutex<BTreeMap<Vec<u8>, TransferJob>>>,
    transfer_id: Vec<u8>,
    session: Arc<SshSession>,
    request: SftpRequest,
    operation: SftpOperation,
    control: TransferControl,
) {
    let result: Result<cshell_sftp::TransferOutcome, String> = async {
        let client = session
            .open_sftp()
            .await
            .map_err(|error| error.to_string())?;
        if control.is_cancelled() {
            let _ = client.close().await;
            return Err(SftpError::TransferCancelled.to_string());
        }
        let local = PathBuf::from(request.local_path);
        let progress_jobs = Arc::clone(&jobs);
        let progress_id = transfer_id.clone();
        let progress = move |event: TransferProgress| {
            let mut jobs = progress_jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(job) = jobs.get_mut(&progress_id) {
                job.snapshot.bytes_transferred = event.bytes_transferred;
                job.snapshot.total_bytes = event.total_bytes;
            }
        };
        let result = match operation {
            SftpOperation::Upload => {
                client
                    .upload_from_path_atomic(local, request.remote_path, &control, progress)
                    .await
            }
            SftpOperation::Download => {
                client
                    .download_to_path_atomic(request.remote_path, local, &control, progress)
                    .await
            }
            _ => unreachable!("only transfers are spawned"),
        };
        let _ = client.close().await;
        result.map_err(|error| error.to_string())
    }
    .await;
    let mut jobs = jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(job) = jobs.get_mut(&transfer_id) {
        match result {
            Ok(outcome) => {
                job.snapshot.state = SftpTransferState::Succeeded as i32;
                job.snapshot.bytes_transferred = outcome.bytes_transferred;
                job.snapshot.total_bytes = outcome.total_bytes;
                job.snapshot.detail.clear();
            }
            Err(detail) => {
                job.snapshot.state = if control.is_cancelled() {
                    SftpTransferState::Cancelled as i32
                } else {
                    SftpTransferState::Failed as i32
                };
                job.snapshot.detail = detail;
            }
        }
    }
}

fn valid_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= MAX_SFTP_PATH_BYTES && !path.contains('\0')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{SftpIpcService, TransferJob};
    use crate::SshSessionRegistry;
    use cshell_domain::SessionId;
    use cshell_ipc::{SftpOperation, SftpRequest, SftpStatus, SftpTransfer, SftpTransferState};
    use cshell_sftp::TransferControl;

    fn request(operation: SftpOperation, session_id: SessionId) -> SftpRequest {
        SftpRequest {
            operation: operation as i32,
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            remote_path: String::new(),
            local_path: String::new(),
            max_entries: 0,
            transfer_id: Vec::new(),
        }
    }

    #[tokio::test]
    async fn rejects_invalid_paths_and_non_ssh_sessions() {
        let service = SftpIpcService::default();
        let sessions = SshSessionRegistry::default();
        let id = SessionId::new();
        let mut list = request(SftpOperation::List, id);
        list.remote_path = "/".into();
        list.max_entries = 513;
        assert_eq!(
            service.handle(list.clone(), &sessions).await.status,
            SftpStatus::InvalidRequest as i32
        );
        list.max_entries = 512;
        assert_eq!(
            service.handle(list, &sessions).await.status,
            SftpStatus::Unavailable as i32
        );
        let mut upload = request(SftpOperation::Upload, id);
        upload.remote_path = "/tmp/result".into();
        upload.local_path = "a\0b".into();
        assert_eq!(
            service.handle(upload, &sessions).await.status,
            SftpStatus::InvalidRequest as i32
        );
    }

    #[tokio::test]
    async fn transfer_can_be_recovered_and_cancelled_after_service_clone() {
        let service = SftpIpcService::default();
        let sessions = SshSessionRegistry::default();
        let id = SessionId::new();
        let transfer_id = SessionId::new().as_uuid().as_bytes().to_vec();
        let control = TransferControl::default();
        service.jobs.lock().unwrap().insert(
            transfer_id.clone(),
            TransferJob {
                session_id: id,
                control: control.clone(),
                started_at: std::time::Instant::now(),
                snapshot: SftpTransfer {
                    transfer_id: transfer_id.clone(),
                    state: SftpTransferState::Running as i32,
                    bytes_transferred: 1024,
                    total_bytes: Some(4096),
                    detail: String::new(),
                },
            },
        );
        let recovered = service
            .clone()
            .handle(request(SftpOperation::CurrentTransfer, id), &sessions)
            .await;
        assert_eq!(recovered.transfer.unwrap().bytes_transferred, 1024);
        let mut cancel = request(SftpOperation::CancelTransfer, id);
        cancel.transfer_id = transfer_id;
        assert_eq!(
            service.handle(cancel, &sessions).await.status,
            SftpStatus::Ok as i32
        );
        assert!(control.is_cancelled());
    }
}
