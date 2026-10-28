use super::*;
use crate::sftp_connection::{BatchAction, queue_batch};

#[tokio::test]
async fn batch_cancel_uses_fresh_revisions_and_skips_finished_rows() {
    let session = SessionId::new();
    let mut before = SftpQueueSnapshot::initial(session);
    before.paused = false;
    let mut pending = item(SftpQueueState::Pending);
    pending.verified = None;
    pending.transfer_id = None;
    let mut done = item(SftpQueueState::Cancelled);
    done.verified = None;
    done.transfer_id = None;
    before.items = vec![pending.clone(), done.clone()];
    let shared = view(before.clone());
    let mut latest = before.clone();
    latest.revision += 1;
    let mut after = latest.clone();
    after.revision += 1;
    after.items[0].state = SftpQueueState::Cancelled;
    let (config, server) = mock(vec![
        (SftpOperation::QueueGet, reply(&before, false)),
        (SftpOperation::QueueGet, reply(&latest, false)),
        (SftpOperation::QueueCancelPending, reply(&after, false)),
        (SftpOperation::QueueGet, reply(&after, false)),
    ])
    .await;
    queue_batch::run(
        &config,
        &shared,
        session,
        BatchAction::Cancel,
        vec![pending.id, done.id],
        1,
    )
    .await;
    let requests = server.await.unwrap();
    assert_eq!(requests[2].expected_revision, 2);
    let view = shared.lock().unwrap();
    assert!(!view.queue_busy);
    assert!(view.queue_writable);
    assert_eq!(
        view.batch_status.as_deref(),
        Some("取消：已处理 1 项，跳过 1 项")
    );
    assert!(!view.queue.as_ref().unwrap().paused);
}
#[tokio::test]
async fn uncertain_batch_reply_stops_without_replaying_later_items() {
    let session = SessionId::new();
    let mut before = SftpQueueSnapshot::initial(session);
    for _ in 0..2 {
        let mut row = item(SftpQueueState::Pending);
        row.verified = None;
        row.transfer_id = None;
        before.items.push(row);
    }
    let ids = before.items.iter().map(|row| row.id).collect();
    let shared = view(before.clone());
    let (config, server) = mock(vec![
        (SftpOperation::QueueGet, reply(&before, false)),
        (SftpOperation::QueueGet, reply(&before, false)),
        (
            SftpOperation::QueueCancelPending,
            SftpResponse::with_status(SftpStatus::Failed, "uncertain storage result"),
        ),
    ])
    .await;
    queue_batch::run(&config, &shared, session, BatchAction::Cancel, ids, 1).await;
    assert_eq!(server.await.unwrap().len(), 3);
    let view = shared.lock().unwrap();
    assert!(!view.queue_busy);
    assert!(!view.queue_writable);
    assert!(view.error.as_ref().unwrap().contains("已停止"));
}
#[tokio::test]
async fn batch_retry_keeps_ids_and_respects_queue_hold() {
    let session = SessionId::new();
    let mut before = SftpQueueSnapshot::initial(session);
    let failed = item(SftpQueueState::Unknown);
    let id = failed.id;
    before.items.push(failed);
    let shared = view(before.clone());
    {
        let mut view = shared.lock().unwrap();
        view.queue_hold = true;
        view.paused_items.insert(id);
        view.stopping_items.insert(id);
    }
    let mut after = before.clone();
    after.revision += 1;
    after.retry_items.insert(id);
    let (config, server) = mock(vec![
        (SftpOperation::QueueGet, reply(&before, false)),
        (SftpOperation::QueueRetryMany, reply(&after, false)),
    ])
    .await;
    queue_batch::run(&config, &shared, session, BatchAction::Retry, vec![id], 1).await;
    let requests = server.await.unwrap();
    let plan: cshell_application::SftpQueueRetry =
        serde_json::from_slice(&requests[1].document_json).unwrap();
    assert!(!plan.start);
    assert_eq!(plan.items, vec![id]);
    let view = shared.lock().unwrap();
    assert!(view.queue.as_ref().unwrap().retry_items.contains(&id));
    assert!(!view.paused_items.contains(&id));
    assert!(!view.stopping_items.contains(&id));
    assert!(view.queue_writable);
    assert!(!view.queue_busy);
}
#[tokio::test]
async fn batch_live_controls_pause_only_selected_transfers() {
    let session = SessionId::new();
    let mut before = SftpQueueSnapshot::initial(session);
    before.concurrency = Some(3);
    before.items = vec![item(SftpQueueState::Running), item(SftpQueueState::Running)];
    before.items[1].draft.remote_path = "/other.bin".into();
    let id = before.items[0].id;
    let transfer = before.items[0].transfer_id.unwrap();
    let shared = view(before.clone());
    let (config, server) = mock(vec![
        (SftpOperation::QueueGet, reply(&before, false)),
        (
            SftpOperation::PauseTransfer,
            SftpResponse {
                transfer: Some(SftpTransfer {
                    transfer_id: transfer.as_uuid().as_bytes().to_vec(),
                    paused: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
    ])
    .await;
    queue_batch::run(&config, &shared, session, BatchAction::Pause, vec![id], 1).await;
    let requests = server.await.unwrap();
    assert_eq!(requests[1].transfer_id, transfer.as_uuid().as_bytes());
    let view = shared.lock().unwrap();
    assert!(view.paused_items.contains(&id));
    assert_eq!(view.paused_items.len(), 1);
    assert!(!view.queue_busy);
}
