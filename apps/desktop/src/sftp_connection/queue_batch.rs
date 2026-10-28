use super::*;
use cshell_application::{SftpQueueItem, SftpQueueRetry, SftpQueueState};
use cshell_domain::SftpQueueItemId;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchAction {
    Pause,
    Resume,
    Cancel,
    Retry,
    RemoveFinished,
}
impl BatchAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pause => "暂停",
            Self::Resume => "继续",
            Self::Cancel => "取消",
            Self::Retry => "重试",
            Self::RemoveFinished => "移除",
        }
    }
    pub(crate) fn allows(self, item: &SftpQueueItem, view: &SftpView) -> bool {
        let paused = item
            .transfer_id
            .and_then(|id| view.queue_transfers.get(&id))
            .is_some_and(|t| t.paused);
        let retrying = view
            .queue
            .as_ref()
            .is_some_and(|q| q.retry_items.contains(&item.id));
        match self {
            Self::Pause => {
                item.state == SftpQueueState::Running
                    && !paused
                    && !view.stopping_items.contains(&item.id)
            }
            Self::Resume => {
                item.state == SftpQueueState::Running
                    && paused
                    && !view.stopping_items.contains(&item.id)
            }
            Self::Cancel => {
                matches!(
                    item.state,
                    SftpQueueState::Pending | SftpQueueState::Running
                ) && !view.stopping_items.contains(&item.id)
                    || retrying
            }
            Self::Retry => item.can_retry_verified() && !retrying,
            Self::RemoveFinished => matches!(
                item.state,
                SftpQueueState::Succeeded | SftpQueueState::Cancelled
            ),
        }
    }
}
fn publish(
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    queue: SftpQueueSnapshot,
) -> Result<(), String> {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) {
        return Err("传输会话已变化".into());
    }
    view.queue = Some(Arc::new(queue));
    view.generation = view.generation.wrapping_add(1);
    Ok(())
}
async fn mutate(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    operation: SftpOperation,
    item: SftpQueueItemId,
) -> Result<bool, String> {
    for _ in 0..8 {
        let queue = transfer_controls::load(config, session).await?;
        let eligible = queue.items.iter().any(|row| {
            row.id == item
                && match operation {
                    SftpOperation::QueueRemove => matches!(
                        row.state,
                        SftpQueueState::Succeeded | SftpQueueState::Cancelled
                    ),
                    _ => {
                        row.state == SftpQueueState::Pending || queue.retry_items.contains(&row.id)
                    }
                }
        });
        if !eligible {
            publish(shared, session, queue)?;
            return Ok(false);
        }
        let reply = send(
            config,
            SftpRequest {
                operation: operation as i32,
                session_id: session.as_uuid().as_bytes().to_vec(),
                expected_revision: queue.revision,
                transfer_id: item.as_uuid().as_bytes().to_vec(),
                confirmed: true,
                ..Default::default()
            },
        )
        .await?;
        // Only a pre-mutation CAS rejection is safe to retry. Never replay uncertain writes.
        if reply.status == SftpStatus::StaleRevision as i32 {
            continue;
        }
        let after = transfer_controls::parse(reply, session, queue.revision)?;
        publish(shared, session, after)?;
        return Ok(true);
    }
    Err("队列正在频繁更新，请刷新后重试".into())
}
async fn live(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    item: &SftpQueueItem,
    action: BatchAction,
) -> Result<(), String> {
    let transfer = item.transfer_id.ok_or("运行任务缺少传输标识")?;
    let operation = match action {
        BatchAction::Pause => SftpOperation::PauseTransfer,
        BatchAction::Resume => SftpOperation::ResumeTransfer,
        _ => SftpOperation::CancelTransfer,
    };
    let reply = send(
        config,
        request(
            operation,
            session,
            String::new(),
            String::new(),
            transfer.as_uuid().as_bytes().to_vec(),
        ),
    )
    .await?;
    let snapshot = reply
        .transfer
        .filter(|t| t.transfer_id == transfer.as_uuid().as_bytes())
        .filter(|_| reply.status == SftpStatus::Ok as i32)
        .ok_or("任务控制结果未确认，请刷新核对")?;
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) {
        return Err("传输会话已变化".into());
    }
    if action == BatchAction::Cancel {
        view.stopping_items.insert(item.id);
    }
    if snapshot.paused {
        view.paused_items.insert(item.id);
    } else {
        view.paused_items.remove(&item.id);
    }
    view.queue_transfers.insert(transfer, snapshot);
    Ok(())
}
async fn retry(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    ids: &[SftpQueueItemId],
) -> Result<(usize, usize), String> {
    for _ in 0..8 {
        let queue = transfer_controls::load(config, session).await?;
        let items: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| {
                !queue.retry_items.contains(id)
                    && queue
                        .items
                        .iter()
                        .any(|row| row.id == *id && row.can_retry_verified())
            })
            .collect();
        if items.is_empty() {
            publish(shared, session, queue)?;
            return Ok((0, ids.len()));
        }
        let start = !shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue_hold;
        let reply = send(
            config,
            SftpRequest {
                operation: SftpOperation::QueueRetryMany as i32,
                session_id: session.as_uuid().as_bytes().to_vec(),
                expected_revision: queue.revision,
                document_json: serde_json::to_vec(&SftpQueueRetry {
                    items: items.clone(),
                    start,
                })
                .map_err(|_| "重试列表无效")?,
                confirmed: true,
                ..Default::default()
            },
        )
        .await?;
        if reply.status == SftpStatus::StaleRevision as i32 {
            continue;
        }
        let after = transfer_controls::parse(reply, session, queue.revision)?;
        if !items.iter().all(|id| after.retry_items.contains(id)) {
            return Err("重试回执不匹配，请刷新核对；不会重复提交".into());
        }
        {
            let mut view = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if view.session_id != Some(session) {
                return Err("传输会话已变化".into());
            }
            // IDs persist across retries; these markers belong to the previous transfer.
            for id in &items {
                view.paused_items.remove(id);
                view.stopping_items.remove(id);
            }
        }
        publish(shared, session, after)?;
        return Ok((items.len(), ids.len() - items.len()));
    }
    Err("队列正在频繁更新，请刷新后重试".into())
}
pub(super) async fn run(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    action: BatchAction,
    ids: Vec<SftpQueueItemId>,
    revision: u64,
) {
    let result = async {
        let unique: BTreeSet<_> = ids.iter().copied().collect();
        if ids.is_empty()
            || ids.len() > cshell_application::MAX_SFTP_QUEUE_ITEMS
            || unique.len() != ids.len()
        {
            return Err("批量任务选择无效".into());
        }
        {
            let mut view = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if view.session_id != Some(session)
                || !view.queue_writable
                || view.queue.as_ref().is_none_or(|q| q.revision < revision)
            {
                return Err("传输队列已变化，请重新选择".into());
            }
            view.batch_status = Some(format!("正在{} {} 项…", action.label(), ids.len()));
            view.generation = view.generation.wrapping_add(1);
        }
        if action == BatchAction::Retry {
            return retry(config, shared, session, &ids).await;
        }
        let (mut done, mut skipped) = (0, 0);
        for (index, id) in ids.iter().enumerate() {
            let queue = transfer_controls::load(config, session).await?;
            publish(shared, session, queue.clone())?;
            let item = queue.items.iter().find(|row| row.id == *id);
            let Some(item) = item else {
                skipped += 1;
                continue;
            };
            let allowed = action.allows(
                item,
                &shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if !allowed {
                skipped += 1;
                continue;
            }
            if item.state == SftpQueueState::Running && action != BatchAction::RemoveFinished {
                live(config, shared, session, item, action).await?;
                done += 1;
            } else {
                let operation = if action == BatchAction::RemoveFinished {
                    SftpOperation::QueueRemove
                } else {
                    SftpOperation::QueueCancelPending
                };
                if mutate(config, shared, session, operation, *id).await? {
                    done += 1;
                } else {
                    skipped += 1;
                }
            }
            let mut view = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if view.session_id!=Some(session) {return Err("传输会话已变化".into());}
            view.batch_status = Some(format!(
                "正在{} {}/{} 项…",
                action.label(),
                index + 1,
                ids.len()
            ));
            view.generation = view.generation.wrapping_add(1);
        }
        Ok((done, skipped))
    }
    .await;
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) {
        return;
    }
    view.queue_busy = false;
    match result {
        Ok((done, skipped)) => {
            view.batch_status = Some(format!(
                "{}：已处理 {done} 项，跳过 {skipped} 项",
                action.label()
            ));
            view.error = None;
        }
        Err(error) => {
            view.queue_writable = false;
            view.batch_status = Some(format!("批量{}已停止，请刷新核对", action.label()));
            view.error = Some(format!(
                "批量{}已停止：{error}；已确认的操作保留，请刷新核对后续项。",
                action.label()
            ));
        }
    }
    view.generation = view.generation.wrapping_add(1);
}
