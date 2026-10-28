use super::*;
use cshell_application::{SftpQueueItem, SftpQueueState};
use cshell_domain::SftpQueueItemId;
#[cfg(test)]
mod tests;

pub(crate) fn can_pause(item: &SftpQueueItem) -> bool {
    item.state == SftpQueueState::Running
}

pub(super) fn parse(
    reply: SftpResponse,
    session: SessionId,
    revision: u64,
) -> Result<SftpQueueSnapshot, String> {
    if reply.status != SftpStatus::Ok as i32
        || reply.transfer.is_some()
        || !reply.entries.is_empty()
    {
        return Err(if reply.detail.is_empty() {
            "传输请求被拒绝，请刷新后重试".into()
        } else {
            reply.detail
        });
    }
    let queue = decode_sftp_queue(&reply.document_json)
        .or_else(|_| match decode_sftp_workspace(&reply.document_json)? {
            SftpWorkspaceReply::Queue(queue) => Ok(queue),
            _ => Err("传输回执类型不匹配"),
        })
        .map_err(str::to_owned)?;
    if queue.session_id != session || revision.checked_add(1) != Some(queue.revision) {
        return Err("传输回执的会话或版本不匹配；不会重复提交".into());
    }
    Ok(queue)
}
pub(super) async fn load(
    config: &DesktopConnectionConfig,
    session: SessionId,
) -> Result<SftpQueueSnapshot, String> {
    let reply = send(
        config,
        request(
            SftpOperation::QueueGet,
            session,
            String::new(),
            String::new(),
            Vec::new(),
        ),
    )
    .await?;
    if reply.status != SftpStatus::Ok as i32 {
        return Err(reply.detail);
    }
    let queue = decode_sftp_queue(&reply.document_json).map_err(str::to_owned)?;
    if queue.session_id != session || queue.migrated_to.is_some() {
        return Err("传输会话已变化".into());
    }
    Ok(queue)
}
fn finish(
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    result: Result<SftpQueueSnapshot, String>,
    status: &str,
) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) {
        return;
    }
    view.queue_busy = false;
    view.generation = view.generation.wrapping_add(1);
    match result {
        Ok(queue) => {
            view.queue = Some(Arc::new(queue));
            view.error = None;
            view.status = status.into();
        }
        Err(error) => {
            view.queue_writable = false;
            view.error = Some(error);
        }
    }
}
fn current(
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    revision: u64,
) -> Result<Arc<SftpQueueSnapshot>, String> {
    let view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    view.queue
        .as_ref()
        .filter(|queue| {
            view.session_id == Some(session)
                && view.queue_writable
                && queue.revision == revision
                && queue.migrated_to.is_none()
        })
        .cloned()
        .ok_or_else(|| "传输队列已变化，请刷新后重新选择".into())
}
pub(super) async fn batch(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    drafts: Vec<SftpQueueDraft>,
    revision: u64,
    start: bool,
) {
    let result = async {
        current(shared, session, revision)?;
        if drafts.is_empty()
            || drafts
                .iter()
                .any(|draft| !draft.verified || draft.validate().is_err())
        {
            return Err("文件列表无效，未提交传输".into());
        }
        for _ in 0..8 {
            let before = load(config, session).await?;
            if before.items.len() + drafts.len() > cshell_application::MAX_SFTP_QUEUE_ITEMS {
                return Err("传输队列已满".into());
            }
            let query = SftpRequest {
                operation: if start {
                    SftpOperation::QueueEnqueueAndStart
                } else {
                    SftpOperation::QueueAddMany
                } as i32,
                session_id: session.as_uuid().as_bytes().to_vec(),
                expected_revision: before.revision,
                document_json: serde_json::to_vec(&drafts).map_err(|_| "无法提交文件列表")?,
                confirmed: true,
                ..Default::default()
            };
            let reply = send(config, query).await?;
            // Only an explicit pre-mutation CAS rejection can be retried.
            // Never replay a transport/storage error or uncertain receipt.
            if reply.status == SftpStatus::Conflict as i32
                && reply.detail == "queue revision changed"
            {
                continue;
            }
            let queue = parse(reply, session, before.revision)?;
            if queue.items.len() != before.items.len() + drafts.len()
                || queue.items[..before.items.len()] != before.items[..]
                || !queue.items[before.items.len()..]
                    .iter()
                    .zip(&drafts)
                    .all(|(item, draft)| {
                        item.draft == *draft && item.state == SftpQueueState::Pending
                    })
            {
                return Err("新增文件回执不匹配，请刷新核对；不会重复添加".into());
            }
            return Ok(queue);
        }
        Err("队列繁忙，文件尚未加入，请稍后重试".into())
    }
    .await;
    finish(
        shared,
        session,
        result,
        if start {
            "文件已加入并自动传输"
        } else {
            "文件已加入暂停队列"
        },
    );
}
pub(super) async fn stop(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    item: SftpQueueItemId,
    revision: u64,
    pause: bool,
) {
    live_transfer(
        config,
        shared,
        session,
        item,
        revision,
        if pause {
            SftpOperation::PauseTransfer
        } else {
            SftpOperation::CancelTransfer
        },
    )
    .await;
}
pub(super) async fn resume(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    item: SftpQueueItemId,
    revision: u64,
) {
    live_transfer(
        config,
        shared,
        session,
        item,
        revision,
        SftpOperation::ResumeTransfer,
    )
    .await;
}
async fn live_transfer(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    item: SftpQueueItemId,
    revision: u64,
    operation: SftpOperation,
) {
    let result = async {
        let queue = current(shared, session, revision)?;
        let transfer = queue
            .items
            .iter()
            .find(|row| row.id == item && row.state == SftpQueueState::Running)
            .and_then(|row| row.transfer_id)
            .ok_or("任务已结束或会话已变化")?;
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
            .filter(|snapshot| snapshot.transfer_id == transfer.as_uuid().as_bytes())
            .filter(|_| reply.status == SftpStatus::Ok as i32)
            .ok_or("任务控制未确认，请刷新状态")?;
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.session_id == Some(session) {
            if operation == SftpOperation::CancelTransfer {
                view.stopping_items.insert(item);
            }
            if snapshot.paused {
                view.paused_items.insert(item);
            } else {
                view.paused_items.remove(&item);
            }
            view.queue_transfers.insert(transfer, snapshot);
        }
        Ok((*queue).clone())
    }
    .await;
    finish(shared, session, result, "任务状态已更新");
}
pub(super) async fn queue_control(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    pause: bool,
) {
    let result = async {
        for _ in 0..8 {
            let mut queue = load(config, session).await?;
            // Freeze live writers before reading the CAS version. Progress
            // checkpoints otherwise invalidate it continuously on fast links.
            let mut froze = false;
            for item in queue
                .items
                .iter()
                .filter(|item| item.state == SftpQueueState::Running)
            {
                let transfer = item.transfer_id.ok_or("运行任务缺少传输标识")?;
                let reply = send(
                    config,
                    request(
                        SftpOperation::PauseTransfer,
                        session,
                        String::new(),
                        String::new(),
                        transfer.as_uuid().as_bytes().to_vec(),
                    ),
                )
                .await?;
                let snapshot = reply
                    .transfer
                    .filter(|value| value.transfer_id == transfer.as_uuid().as_bytes())
                    .filter(|_| reply.status == SftpStatus::Ok as i32)
                    .ok_or("部分任务暂停尚未确认，请刷新后核对")?;
                let mut view = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if view.session_id != Some(session) {
                    return Err("传输会话已变化".into());
                }
                view.queue_transfers.insert(transfer, snapshot);
                froze = true;
            }
            if froze {
                queue = load(config, session).await?;
            }
            let query = SftpRequest {
                operation: if pause {
                    SftpOperation::QueuePause
                } else {
                    SftpOperation::QueueStartManaged
                } as i32,
                session_id: session.as_uuid().as_bytes().to_vec(),
                expected_revision: queue.revision,
                confirmed: true,
                ..Default::default()
            };
            let reply = send(config, query).await?;
            if reply.status == SftpStatus::StaleRevision as i32 {
                continue;
            }
            return parse(reply, session, queue.revision);
        }
        Err("队列繁忙，请重试暂停或继续".into())
    }
    .await;
    finish(
        shared,
        session,
        result,
        if pause {
            "队列已暂停，当前文件也已挂起"
        } else {
            "队列已继续"
        },
    );
}
pub(super) async fn poll(config: &DesktopConnectionConfig, shared: &Arc<Mutex<SftpView>>) {
    let (session, ids) = {
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(queue) = view.queue.clone() else {
            return;
        };
        if view.queue_busy {
            return;
        }
        view.queue_transfers
            .retain(|id, _| queue.items.iter().any(|item| item.transfer_id == Some(*id)));
        view.paused_items
            .retain(|id| queue.items.iter().any(|item| item.id == *id));
        view.stopping_items.retain(|id| {
            queue
                .items
                .iter()
                .any(|item| item.id == *id && matches!(item.state, SftpQueueState::Running | SftpQueueState::Unknown))
        });
        let ids: Vec<_> = queue
            .items
            .iter()
            .filter_map(|item| {
                item.transfer_id.filter(|id| {
                    item.state == SftpQueueState::Running
                        || view
                            .queue_transfers
                            .get(id)
                            .is_some_and(|t| t.state == SftpTransferState::Running as i32)
                })
            })
            .take(4)
            .collect();
        (queue.session_id, ids)
    };
    for id in ids {
        if let Ok(reply) = send(
            config,
            request(
                SftpOperation::TransferStatus,
                session,
                String::new(),
                String::new(),
                id.as_uuid().as_bytes().to_vec(),
            ),
        )
        .await
            && reply.status == SftpStatus::Ok as i32
            && let Some(transfer) = reply
                .transfer
                .filter(|t| t.transfer_id == id.as_uuid().as_bytes())
        {
            let mut view = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if view.session_id == Some(session) {
                view.queue_transfers.insert(id, transfer);
                view.generation = view.generation.wrapping_add(1);
            }
        }
    }
}
