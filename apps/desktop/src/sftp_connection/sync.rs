use super::*;
pub(super) async fn run(
    config: DesktopConnectionConfig,
    shared: Arc<Mutex<SftpView>>,
    session: SessionId,
    op: SftpOperation,
    id: SessionId,
    data: Vec<u8>,
) {
    let result = send(
        &config,
        SftpRequest {
            operation: op as i32,
            session_id: session.as_uuid().as_bytes().to_vec(),
            transfer_id: id.as_uuid().as_bytes().to_vec(),
            document_json: data,
            confirmed: op == SftpOperation::SyncApply,
            ..Default::default()
        },
    )
    .await;
    let refused = result
        .as_ref()
        .is_ok_and(|r| r.status != SftpStatus::Ok as i32);
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) || view.sync_pending != Some((id, op)) {
        return;
    }
    view.sync_pending = None;
    view.generation = view.generation.wrapping_add(1);
    let result = result.and_then(|reply| {
        if reply.status != SftpStatus::Ok as i32 {
            return Err(reply.detail);
        }
        if op == SftpOperation::SyncScan {
            let preview: cshell_application::SftpSyncPreview =
                serde_json::from_slice(&reply.document_json).map_err(|_| "目录比较回复无效")?;
            if preview.id != id || preview.session != session || preview.entries.len() > 10000 {
                return Err("目录比较范围不匹配".into());
            }
            view.sync_preview = Some(Arc::new(preview));
        } else {
            let snapshot: cshell_application::SftpTreeSnapshot =
                serde_json::from_slice(&reply.document_json)
                    .map_err(|_| "同步提交回执未确认，请检查任务，不要重复提交")?;
            if snapshot.id != id || snapshot.session != session {
                return Err("同步任务身份不匹配，请检查任务，不要重复提交".into());
            }
            view.trees.insert(id, snapshot);
            view.sync_preview = None;
        }
        Ok(())
    });
    if result.is_err() && op == SftpOperation::SyncApply {
        if refused {
            view.trees.remove(&id);
        } else if let Some(task) = view.trees.get_mut(&id) {
            task.detail = "提交结果未确认；正在查询原任务，不会重复提交。".into();
        }
    }
    view.sync_error = result.err();
    view.batch_status = Some(if let Some(error) = &view.sync_error {
        format!("目录比较 / 同步未完成：{error}")
    } else if op == SftpOperation::SyncScan {
        format!(
            "目录比较已就绪 · {} 项；在更多功能 → 目录比较 / 同步查看",
            view.sync_preview.as_ref().map_or(0, |v| v.entries.len())
        )
    } else {
        "同步已提交 · 可在传输区查看进度、暂停或取消".into()
    });
}
