//! SFTP requests and transfers remain owned by the daemon across GUI reconnects.

use crate::SshSessionRegistry;
use crate::sftp_lifecycle::SftpTarget as SshSession;
use cshell_domain::SessionId;
use cshell_ipc::{
    MAX_SFTP_DIRECTORY_ENTRIES, MAX_SFTP_PATH_BYTES, SftpCleanupLocation, SftpDirectoryEntry,
    SftpOperation, SftpRequest, SftpResponse, SftpStatus, SftpTransfer, SftpTransferState,
};
use cshell_sftp::{RemoteFileType, SftpError, TransferControl, TransferProgress};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_ACTIVE_TRANSFERS: usize = 8;
const MAX_TRACKED_TRANSFERS: usize = 64;
mod browse;
mod migration;
mod namespace;
mod queue;
mod verified;
mod workspace;
mod workspace_store;
mod terminal_upload;
mod tree;
mod sync;

#[derive(Clone, Debug, Default)]
pub(crate) struct SftpIpcService {
    uploads: terminal_upload::Uploads,
    trees: tree::Trees,
    sync_plans:sync::Plans,
    jobs: Arc<Mutex<BTreeMap<Vec<u8>, TransferJob>>>,
    queues: queue::Queues,
    browsers: browse::Browsers,
    workspace: workspace::Workspace,
    namespaces: namespace::Namespaces,
}

#[derive(Clone, Debug)]
struct TransferJob {
    exclusive: bool,
    session_id: SessionId,
    control: TransferControl,
    snapshot: SftpTransfer,
    started_at: std::time::Instant,
}

impl SftpIpcService {
    pub(crate) fn stop_session_transfers(&self,session:SessionId) {
        for job in self.jobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).values() {
            if job.session_id==session {job.control.cancel();}
        }
    }

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
            SftpOperation::SyncScan|SftpOperation::SyncApply=>self.sync_request(request,session_id,operation,sessions).await,
            SftpOperation::TreeStart|SftpOperation::TreeGet|SftpOperation::TreePause|SftpOperation::TreeContinue|SftpOperation::TreeCancel|SftpOperation::TreeRetry=>self.tree(request,session_id,operation,sessions).await,
            SftpOperation::TerminalUploadStart | SftpOperation::TerminalUploadGet
            | SftpOperation::TerminalUploadPause | SftpOperation::TerminalUploadContinue
            | SftpOperation::TerminalUploadCancel => self.terminal_upload(request, session_id, operation, sessions).await,
            SftpOperation::RemoteBrowse
            | SftpOperation::LocalBrowse
            | SftpOperation::BrowseNext
            | SftpOperation::BrowseClose => {
                self.browse(request, session_id, operation, sessions).await
            }
            SftpOperation::LocalList => self.local_list(request).await,
            SftpOperation::QueueGet
            | SftpOperation::QueueAdd
            | SftpOperation::QueueStart
            | SftpOperation::QueuePause
            | SftpOperation::QueueCancelPending
            | SftpOperation::QueueRemove => {
                self.queue(request, session_id, operation, sessions).await
            }
            SftpOperation::QueueAddVerified
            | SftpOperation::QueueStartVerified
            | SftpOperation::QueueResumeVerified
            | SftpOperation::QueueAdoptVerified
            | SftpOperation::QueueCleanupVerified => {
                self.queue(request, session_id, operation, sessions).await
            }
            SftpOperation::QueueEdit
            | SftpOperation::QueueAddManaged
            | SftpOperation::QueueStartManaged
            | SftpOperation::QueueConfigure
            | SftpOperation::QueueStartOne
            | SftpOperation::QueueRetryMany => {
                self.queue(request, session_id, operation, sessions).await
            }
            SftpOperation::QueueMigrate => self.migrate_queue(request, session_id, sessions).await,
            SftpOperation::BrowseManaged => {
                self.browse_managed(request, session_id, sessions).await
            }
            SftpOperation::EditReview|SftpOperation::EditRebase|SftpOperation::FileInspect
            | SftpOperation::FileApply
            | SftpOperation::FileJobGet
            | SftpOperation::FileJobCancel
            | SftpOperation::EditOpen
            | SftpOperation::EditGet
            | SftpOperation::EditLaunch
            | SftpOperation::EditSave
            | SftpOperation::EditClose
            | SftpOperation::WorkspaceList
            | SftpOperation::WorkspaceForget
            | SftpOperation::PeerGet
            | SftpOperation::EditRebind => {
                self.workspace_request(request, session_id, operation, sessions)
                    .await
            }
            SftpOperation::QueueAddMany | SftpOperation::QueueEnqueueAndStart => {
                self.workspace_request(request, session_id, operation, sessions)
                    .await
            }
            SftpOperation::List => self.list(request, session_id, sessions).await,
            SftpOperation::Upload | SftpOperation::Download => {
                self.start(request, operation, session_id, sessions)
            }
            SftpOperation::TransferStatus
            | SftpOperation::CancelTransfer
            | SftpOperation::PauseTransfer
            | SftpOperation::ResumeTransfer => self.transfer_status(request, operation, session_id),
            SftpOperation::CurrentTransfer => self.current_transfer(session_id),
            SftpOperation::ConnectProfile | SftpOperation::Disconnect | SftpOperation::ConnectionStatus => SftpResponse::with_status(SftpStatus::Unsupported,"SFTP lifecycle requires the session dispatcher"),
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
        let session = match sessions.get_sftp(session_id) {
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
                        permissions: entry.metadata.permissions,
                        uid: entry.metadata.uid,
                        gid: entry.metadata.gid,
                    });
                }
                SftpResponse {
                    status: SftpStatus::Ok as i32,
                    detail: String::new(),
                    entries,
                    truncated: page.truncated,
                    transfer: None,
                    document_json: Vec::new(),
                    ..Default::default()
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
        let session = match sessions.get_sftp(session_id) {
            Ok(session) if session.running() => session,
            _ => {
                return SftpResponse::with_status(
                    SftpStatus::Unavailable,
                    "SSH session is unavailable",
                );
            }
        };
        self.start_bound(request, operation, session_id, session, None)
    }
    fn start_bound(
        &self,
        request: SftpRequest,
        operation: SftpOperation,
        session_id: SessionId,
        session: Arc<SshSession>,
        planned: Option<Vec<u8>>,
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
        if !session.running() {
            return SftpResponse::with_status(
                SftpStatus::Unavailable,
                "SSH session is unavailable",
            );
        }
        let transfer_id = planned.unwrap_or_else(|| SessionId::new().as_uuid().as_bytes().to_vec());
        let control = TransferControl::default();
        let snapshot =
            match self.register_transfer(session_id, transfer_id.clone(), control.clone(), 1) {
                Ok(snapshot) => snapshot,
                Err(status) => {
                    return SftpResponse::with_status(status, "Too many active SFTP transfers");
                }
            };
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
            document_json: Vec::new(),
            ..Default::default()
        }
    }

    fn register_transfer(
        &self,
        session_id: SessionId,
        transfer_id: Vec<u8>,
        control: TransferControl,
        session_limit: usize,
    ) -> Result<SftpTransfer, SftpStatus> {
        if !(1..=4).contains(&session_limit) {
            return Err(SftpStatus::InvalidRequest);
        }
        let snapshot = SftpTransfer {
            transfer_id: transfer_id.clone(),
            state: SftpTransferState::Running as i32,
            bytes_transferred: 0,
            total_bytes: None,
            detail: String::new(),
            ..Default::default()
        };
        {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.len() >= MAX_TRACKED_TRANSFERS {
                jobs.retain(|id, job| {
                    job.snapshot.state == SftpTransferState::Running as i32
                        || self.queues.owns_transfer(job.session_id, id)
                });
            }
            let active = jobs
                .values()
                .filter(|job| job.snapshot.state == SftpTransferState::Running as i32)
                .count();
            let session_busy = jobs
                .values()
                .filter(|job| {
                    job.session_id == session_id
                        && job.snapshot.state == SftpTransferState::Running as i32
                })
                .count()
                >= session_limit
                || jobs.values().any(|job| {
                    job.session_id == session_id
                        && job.exclusive
                        && job.snapshot.state == SftpTransferState::Running as i32
                });
            if active >= MAX_ACTIVE_TRANSFERS
                || jobs.len() >= MAX_TRACKED_TRANSFERS
                || session_busy
                || jobs.contains_key(&transfer_id)
            {
                return Err(SftpStatus::CapacityReached);
            }
            jobs.insert(
                transfer_id.clone(),
                TransferJob {
                    exclusive: session_limit == 1,
                    session_id,
                    control: control.clone(),
                    snapshot: snapshot.clone(),
                    started_at: std::time::Instant::now(),
                },
            );
        }
        Ok(snapshot)
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
            document_json: Vec::new(),
            ..Default::default()
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
        if job.snapshot.state == SftpTransferState::Running as i32 {
            if operation == SftpOperation::PauseTransfer {
                job.control.pause();
            }
            if operation == SftpOperation::ResumeTransfer {
                job.control.resume();
            }
        }
        job.snapshot.paused =
            job.snapshot.state == SftpTransferState::Running as i32 && job.control.is_paused();
        SftpResponse {
            status: SftpStatus::Ok as i32,
            detail: String::new(),
            entries: Vec::new(),
            truncated: false,
            transfer: Some(job.snapshot.clone()),
            document_json: Vec::new(),
            ..Default::default()
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
    let result: Result<cshell_sftp::TransferOutcome, SftpError> = async {
        let client = session
            .open_sftp()
            .await
            .map_err(|_| SftpError::ChannelUnavailable)?;
        if control.is_cancelled() {
            let _ = client.close().await;
            return Err(SftpError::TransferCancelled);
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
        let result = control
            .run(async {
                match operation {
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
                }
            })
            .await;
        let _ = client.close().await;
        result
    }
    .await;
    let mut jobs = jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(job) = jobs.get_mut(&transfer_id) {
        complete_transfer(&mut job.snapshot, operation, result);
    }
}

fn complete_transfer(
    snapshot: &mut SftpTransfer,
    operation: SftpOperation,
    result: Result<cshell_sftp::TransferOutcome, SftpError>,
) {
    snapshot.cleanup_path = None;
    snapshot.cleanup_location = SftpCleanupLocation::Unspecified as i32;
    snapshot.destination_committed = false;
    match result {
        Ok(outcome) => {
            snapshot.state = SftpTransferState::Succeeded as i32;
            snapshot.bytes_transferred = outcome.bytes_transferred;
            snapshot.total_bytes = outcome.total_bytes;
            snapshot.detail.clear();
            snapshot.destination_committed = true;
        }
        Err(error) => {
            snapshot.state = if matches!(error, SftpError::TransferCancelled) {
                SftpTransferState::Cancelled as i32
            } else {
                SftpTransferState::Failed as i32
            };
            snapshot.detail = error.to_string();
            let path = match &error {
                SftpError::TemporaryCleanupFailed { path, .. } => Some(path),
                SftpError::CommittedCleanupFailed { path, .. } => {
                    snapshot.destination_committed = true;
                    Some(path)
                }
                _ => None,
            };
            if let Some(path) = path.filter(|path| valid_path(path)) {
                snapshot.cleanup_path = Some(path.clone());
                snapshot.cleanup_location = if operation == SftpOperation::Upload {
                    SftpCleanupLocation::Remote as i32
                } else {
                    SftpCleanupLocation::DaemonLocal as i32
                };
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
    use super::{SftpIpcService, TransferJob, complete_transfer};
    use crate::SshSessionRegistry;
    use cshell_domain::SessionId;
    use cshell_ipc::{
        MAX_SFTP_PATH_BYTES, SftpCleanupLocation, SftpOperation, SftpRequest, SftpStatus,
        SftpTransfer, SftpTransferState,
    };
    use cshell_sftp::{SftpError, TransferControl};

    fn request(operation: SftpOperation, session_id: SessionId) -> SftpRequest {
        SftpRequest {
            operation: operation as i32,
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            remote_path: String::new(),
            local_path: String::new(),
            max_entries: 0,
            transfer_id: Vec::new(),
            ..Default::default()
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
                exclusive: true,
                session_id: id,
                control: control.clone(),
                started_at: std::time::Instant::now(),
                snapshot: SftpTransfer {
                    transfer_id: transfer_id.clone(),
                    state: SftpTransferState::Running as i32,
                    bytes_transferred: 1024,
                    total_bytes: Some(4096),
                    detail: String::new(),
                    ..SftpTransfer::default()
                },
            },
        );
        let recovered = service
            .clone()
            .handle(request(SftpOperation::CurrentTransfer, id), &sessions)
            .await;
        assert_eq!(recovered.transfer.unwrap().bytes_transferred, 1024);
        let mut pause = request(SftpOperation::PauseTransfer, id);
        pause.transfer_id = transfer_id.clone();
        let paused = service
            .handle(pause.clone(), &sessions)
            .await
            .transfer
            .unwrap();
        assert!(paused.paused);
        assert_eq!(paused.bytes_transferred, 1024);
        assert!(control.is_paused() && !control.is_cancelled());
        pause.session_id = SessionId::new().as_uuid().as_bytes().to_vec();
        assert_eq!(
            service.handle(pause, &sessions).await.status,
            SftpStatus::Unavailable as i32
        );
        let mut resume = request(SftpOperation::ResumeTransfer, id);
        resume.transfer_id = transfer_id.clone();
        assert!(
            !service
                .handle(resume, &sessions)
                .await
                .transfer
                .unwrap()
                .paused
        );
        assert!(!control.is_paused());
        let mut cancel = request(SftpOperation::CancelTransfer, id);
        cancel.transfer_id = transfer_id;
        assert_eq!(
            service.handle(cancel, &sessions).await.status,
            SftpStatus::Ok as i32
        );
        assert!(control.is_cancelled());
    }

    #[tokio::test]
    async fn cleanup_failure_survives_cancel_and_reconnect_without_cross_session_access() {
        let service = SftpIpcService::default();
        let sessions = SshSessionRegistry::default();
        let id = SessionId::new();
        let control = TransferControl::default();
        control.cancel();
        let mut snapshot = SftpTransfer::default();
        complete_transfer(
            &mut snapshot,
            SftpOperation::Upload,
            Err(SftpError::TemporaryCleanupFailed {
                path: "/retained.tmp".into(),
                cause: "cancelled".into(),
                reason: "server text".into(),
            }),
        );
        assert_eq!(snapshot.state, SftpTransferState::Failed as i32);
        assert_eq!(
            snapshot.cleanup_location,
            SftpCleanupLocation::Remote as i32
        );
        assert!(!snapshot.destination_committed);
        service.jobs.lock().unwrap().insert(
            vec![1; 16],
            TransferJob {
                exclusive: true,
                session_id: id,
                control,
                started_at: std::time::Instant::now(),
                snapshot,
            },
        );
        let recovered = service
            .clone()
            .handle(request(SftpOperation::CurrentTransfer, id), &sessions)
            .await;
        let transfer = recovered.transfer.unwrap();
        assert_eq!(transfer.cleanup_path.as_deref(), Some("/retained.tmp"));
        assert_eq!(transfer.state, SftpTransferState::Failed as i32);
        assert!(
            service
                .handle(
                    request(SftpOperation::CurrentTransfer, SessionId::new()),
                    &sessions
                )
                .await
                .transfer
                .is_none()
        );
    }

    #[test]
    fn completion_preserves_commit_and_bounds_cleanup_metadata() {
        let mut snapshot = SftpTransfer::default();
        complete_transfer(
            &mut snapshot,
            SftpOperation::Download,
            Err(SftpError::CommittedCleanupFailed {
                path: "local.tmp".into(),
                reason: "private diagnostic".into(),
            }),
        );
        assert_eq!(snapshot.state, SftpTransferState::Failed as i32);
        assert!(snapshot.destination_committed);
        assert_eq!(
            snapshot.cleanup_location,
            SftpCleanupLocation::DaemonLocal as i32
        );
        assert!(!snapshot.detail.contains("private diagnostic"));
        for path in [
            String::new(),
            "nul\0path".into(),
            "x".repeat(MAX_SFTP_PATH_BYTES + 1),
        ] {
            complete_transfer(
                &mut snapshot,
                SftpOperation::Download,
                Err(SftpError::CommittedCleanupFailed {
                    path,
                    reason: String::new(),
                }),
            );
            assert!(snapshot.destination_committed);
            assert!(snapshot.cleanup_path.is_none());
        }
        complete_transfer(
            &mut snapshot,
            SftpOperation::Download,
            Err(SftpError::TransferCancelled),
        );
        assert_eq!(snapshot.state, SftpTransferState::Cancelled as i32);
        assert!(!snapshot.destination_committed);
        assert!(snapshot.cleanup_path.is_none());
    }
}
