use super::*;
use crate::daemon_connection::ResolvedDesktopEndpoint;
use cshell_application::{SftpVerifiedContent, SftpVerifiedTransfer};
use cshell_ipc::{HandshakePolicy, server_handshake};

fn draft() -> SftpQueueDraft {
    SftpQueueDraft {
        verified: true,
        local_path: if cfg!(windows) {
            "C:/Users/operator/file.txt"
        } else {
            "/home/operator/file.txt"
        }
        .into(),
        remote_path: "/home/operator/file.txt".into(),
        ..Default::default()
    }
}
fn item(state: SftpQueueState) -> SftpQueueItem {
    SftpQueueItem {
        id: SftpQueueItemId::new(),
        draft: draft(),
        state,
        transfer_id: Some(SessionId::new()),
        receipt: None,
        verified: Some(SftpVerifiedTransfer {
            temporary_owned: true,
            prefix_sha256: Some([0; 32]),
            content: Some(SftpVerifiedContent {
                size: 1024,
                sha256: [1; 32],
            }),
            ..Default::default()
        }),
    }
}
fn view(queue: SftpQueueSnapshot) -> Arc<Mutex<SftpView>> {
    Arc::new(Mutex::new(SftpView {
        session_id: Some(queue.session_id),
        queue: Some(Arc::new(queue)),
        queue_writable: true,
        queue_busy: true,
        ..Default::default()
    }))
}
fn reply(queue: &SftpQueueSnapshot, wrapped: bool) -> SftpResponse {
    SftpResponse {
        status: SftpStatus::Ok as i32,
        document_json: if wrapped {
            serde_json::to_vec(&SftpWorkspaceReply::Queue(queue.clone())).unwrap()
        } else {
            serde_json::to_vec(queue).unwrap()
        },
        ..Default::default()
    }
}

async fn mock(
    replies: Vec<(SftpOperation, SftpResponse)>,
) -> (
    DesktopConnectionConfig,
    tokio::task::JoinHandle<Vec<SftpRequest>>,
) {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\cshell-sftp-ui-{}", SessionId::new());
    #[cfg(unix)]
    let endpoint = directory.path().join("sftp-ui.sock");
    let listener = transport::LocalListener::bind(&endpoint).unwrap();
    let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
        endpoint,
        instance_token: [5; 32],
        daemon_instance_id: [6; 16],
    });
    let server = tokio::spawn(async move {
        let _directory = directory;
        let mut requests = Vec::new();
        for (operation, response) in replies {
            let mut stream =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id(
                    [5; 32],
                    [6; 16],
                    features::SFTP_CONTROL
                        | features::SFTP_LIVE_CONTROL
                        | features::SFTP_WORKSPACE
                        | features::SFTP_VERIFIED_QUEUE
                        | features::SFTP_BROWSER_QUEUE,
                ),
            )
            .await
            .unwrap();
            let envelope = read_envelope(&mut stream).await.unwrap();
            let Some(envelope::Payload::SftpRequest(request)) = envelope.payload else {
                panic!("not SFTP")
            };
            assert_eq!(request.operation, operation as i32);
            requests.push(request);
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id: envelope.request_id,
                    payload: Some(envelope::Payload::SftpResponse(response)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        requests
    });
    (config, server)
}

#[tokio::test]
async fn batch_adds_then_starts_only_after_matching_receipt() {
    let session = SessionId::new();
    let before = SftpQueueSnapshot::initial(session);
    let shared = view(before.clone());
    let mut added = before;
    added.revision += 1;
    let mut pending = item(SftpQueueState::Pending);
    pending.transfer_id = None;
    pending.verified = None;
    added.items.push(pending);
    let (config, server) = mock(vec![
        (
            SftpOperation::QueueGet,
            reply(&SftpQueueSnapshot::initial(session), false),
        ),
        (SftpOperation::QueueEnqueueAndStart, reply(&added, true)),
    ])
    .await;
    batch(&config, &shared, session, vec![draft()], 1, true).await;
    let requests = server.await.unwrap();
    assert_eq!(requests[0].operation, SftpOperation::QueueGet as i32);
    assert_eq!(requests[1].expected_revision, 1);
    assert!(requests[1].confirmed);
    assert_eq!(shared.lock().unwrap().queue.as_ref().unwrap().revision, 2);
    assert!(!shared.lock().unwrap().queue_busy);
}

#[tokio::test]
async fn mismatched_insertion_is_not_started_or_replayed() {
    let session = SessionId::new();
    let mut queue = SftpQueueSnapshot::initial(session);
    let shared = view(queue.clone());
    queue.revision += 1;
    let (config, server) = mock(vec![
        (
            SftpOperation::QueueGet,
            reply(&SftpQueueSnapshot::initial(session), false),
        ),
        (SftpOperation::QueueEnqueueAndStart, reply(&queue, true)),
    ])
    .await;
    batch(&config, &shared, session, vec![draft()], 1, true).await;
    assert_eq!(server.await.unwrap().len(), 2);
    assert!(!shared.lock().unwrap().queue_writable);
    assert!(shared.lock().unwrap().error.is_some());
    assert_eq!(shared.lock().unwrap().queue.as_ref().unwrap().revision, 1);
}

#[tokio::test]
async fn atomic_append_keeps_running_file_and_retries_only_explicit_prewrite_cas_rejection() {
    let session = SessionId::new();
    let mut before = SftpQueueSnapshot::initial(session);
    before.paused = false;
    let mut running = item(SftpQueueState::Running);
    running.draft.remote_path = "/running.txt".into();
    before.items.push(running.clone());
    let shared = view(before.clone());
    let mut fresh = before.clone();
    fresh.revision += 1;
    let mut added = fresh.clone();
    added.revision += 1;
    let mut pending = item(SftpQueueState::Pending);
    pending.transfer_id = None;
    pending.verified = None;
    added.items.push(pending);
    let (config, server) = mock(vec![
        (SftpOperation::QueueGet, reply(&before, false)),
        (
            SftpOperation::QueueEnqueueAndStart,
            SftpResponse::with_status(SftpStatus::Conflict, "queue revision changed"),
        ),
        (SftpOperation::QueueGet, reply(&fresh, false)),
        (SftpOperation::QueueEnqueueAndStart, reply(&added, true)),
    ])
    .await;
    batch(&config, &shared, session, vec![draft()], 1, true).await;
    let requests = server.await.unwrap();
    assert_eq!(requests[1].expected_revision, 1);
    assert_eq!(requests[3].expected_revision, 2);
    assert_eq!(
        shared.lock().unwrap().queue.as_ref().unwrap().items[0],
        running
    );
    assert_eq!(
        shared.lock().unwrap().queue.as_ref().unwrap().items.len(),
        2
    );
}

#[tokio::test]
async fn live_queue_pause_and_continue_read_the_current_revision() {
    let session = SessionId::new();
    let mut before = SftpQueueSnapshot::initial(session);
    before.paused = false;
    before.items.push(item(SftpQueueState::Running));
    let shared = view(before.clone());
    let mut paused = before.clone();
    paused.paused = true;
    paused.revision += 1;
    let mut resumed = paused.clone();
    resumed.paused = false;
    resumed.revision += 1;
    let frozen = SftpResponse {
        transfer: Some(SftpTransfer {
            transfer_id: before.items[0]
                .transfer_id
                .unwrap()
                .as_uuid()
                .as_bytes()
                .to_vec(),
            paused: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let (config, server) = mock(vec![
        (SftpOperation::QueueGet, reply(&before, false)),
        (SftpOperation::PauseTransfer, frozen.clone()),
        (SftpOperation::QueueGet, reply(&before, false)),
        (SftpOperation::QueuePause, reply(&paused, false)),
        (SftpOperation::QueueGet, reply(&paused, false)),
        (SftpOperation::PauseTransfer, frozen),
        (SftpOperation::QueueGet, reply(&paused, false)),
        (SftpOperation::QueueStartManaged, reply(&resumed, false)),
    ])
    .await;
    queue_control(&config, &shared, session, true).await;
    assert!(shared.lock().unwrap().queue.as_ref().unwrap().paused);
    queue_control(&config, &shared, session, false).await;
    assert!(!shared.lock().unwrap().queue.as_ref().unwrap().paused);
    let requests = server.await.unwrap();
    assert_eq!(requests[3].expected_revision, 1);
    assert_eq!(requests[7].expected_revision, 2);
}

#[tokio::test]
async fn live_pause_preserves_exact_transfer_and_progress() {
    let session = SessionId::new();
    let mut queue = SftpQueueSnapshot::initial(session);
    queue.paused = false;
    let running = item(SftpQueueState::Running);
    let id = running.id;
    let transfer = running.transfer_id.unwrap();
    queue.items.push(running);
    let shared = view(queue.clone());
    queue.paused = true;
    queue.revision += 1;
    let cancelled = SftpResponse {
        status: SftpStatus::Ok as i32,
        transfer: Some(SftpTransfer {
            transfer_id: transfer.as_uuid().as_bytes().to_vec(),
            state: SftpTransferState::Running as i32,
            paused: true,
            bytes_transferred: 256,
            total_bytes: Some(1024),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (config, server) = mock(vec![(SftpOperation::PauseTransfer, cancelled)]).await;
    stop(&config, &shared, session, id, 1, true).await;
    let requests = server.await.unwrap();
    assert_eq!(requests[0].transfer_id, transfer.as_uuid().as_bytes());
    let view = shared.lock().unwrap();
    assert!(view.paused_items.contains(&id));
    assert!(!view.stopping_items.contains(&id));
    assert_eq!(view.queue_transfers[&transfer].bytes_transferred, 256);
    assert!(!view.queue.as_ref().unwrap().paused);
    assert!(!view.queue_busy);
}

#[test]
fn initialization_can_be_paused_and_scope_reset_clears_controls() {
    let mut running = item(SftpQueueState::Running);
    assert!(can_pause(&running));
    running.verified = Some(SftpVerifiedTransfer::default());
    assert!(can_pause(&running), "initialization must be pausable");
    let session = SessionId::new();
    let mut view = SftpView {
        session_id: Some(session),
        ..Default::default()
    };
    view.paused_items.insert(running.id);
    view.queue_transfers
        .insert(running.transfer_id.unwrap(), SftpTransfer::default());
    reset_scope(&mut view, SessionId::new());
    assert!(view.paused_items.is_empty() && view.queue_transfers.is_empty());
}

#[test]
fn mutation_gate_blocks_repeated_clicks_stale_revision_and_other_sessions() {
    let session = SessionId::new();
    let queue = SftpQueueSnapshot::initial(session);
    let shared = view(queue);
    shared.lock().unwrap().queue_busy = false;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let connection = DesktopSftpConnection {
        shared,
        sender: Some(sender),
        shutdown,
        worker: None,
    };
    assert!(!connection.request(SftpClientCommand::TransferBatch(
        SessionId::new(),
        vec![draft()],
        1,
        true
    )));
    assert!(!connection.request(SftpClientCommand::TransferBatch(
        session,
        vec![draft()],
        2,
        true
    )));
    assert!(connection.request(SftpClientCommand::TransferBatch(
        session,
        vec![draft()],
        1,
        true
    )));
    assert!(!connection.request(SftpClientCommand::TransferBatch(
        session,
        vec![draft()],
        1,
        true
    )));
    assert!(receiver.try_recv().is_ok());
    assert!(receiver.try_recv().is_err());
}

mod batches;
