use super::*;
use cshell_application::{SftpTreeSnapshot, SftpTreeStart, TerminalUploadState};
pub(super) async fn run(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    op: SftpOperation,
    id: SessionId,
    options: Option<SftpTreeStart>,
) {
    run_inner(config,shared,session,op,id,options,true).await;
}
async fn run_inner(config:&DesktopConnectionConfig,shared:&Arc<Mutex<SftpView>>,session:SessionId,op:SftpOperation,id:SessionId,options:Option<SftpTreeStart>,explicit:bool){
    let result = send(
        config,
        SftpRequest {
            operation: op as i32,
            session_id: session.as_uuid().as_bytes().to_vec(),
            transfer_id: id.as_uuid().as_bytes().to_vec(),
            confirmed: op != SftpOperation::TreeGet,
            document_json: options
                .map(|v| serde_json::to_vec(&v).unwrap_or_default())
                .unwrap_or_default(),
            ..Default::default()
        },
    )
    .await
    .and_then(|r| {
        if r.status != SftpStatus::Ok as i32 {
            return Err(r.detail);
        }
        let snapshot: SftpTreeSnapshot =
            serde_json::from_slice(&r.document_json).map_err(|_| "目录任务回执无效")?;
        if snapshot.id != id || snapshot.session != session {
            return Err("目录任务的会话或标识不匹配".into());
        }
        Ok(snapshot)
    });
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) {
        return;
    }
    if explicit {view.tree_pending.remove(&id);}
    view.generation = view.generation.wrapping_add(1);
    match result {
        Ok(snapshot) => {
            view.trees.insert(id, snapshot);
        }
        Err(error) => {
            if let Some(snapshot) = view.trees.get_mut(&id) {
                snapshot.state = TerminalUploadState::Failed;
                snapshot.bytes_per_second = 0;
                snapshot.retryable = false;
                snapshot.detail = format!("结果未确认，请刷新此任务进度；不要重复传输。{error}");
            }
            view.error = Some(error);
        }
    }
}
pub(super) async fn poll(config: &DesktopConnectionConfig, shared: &Arc<Mutex<SftpView>>) {
    let tasks = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.trees
            .values()
            .filter(|v| !v.state.finished() && !view.tree_pending.contains(&v.id) && !view.sync_pending.is_some_and(|(id,_)|id==v.id))
            .map(|v| (v.session, v.id))
            .collect::<Vec<_>>()
    };
    for (session, id) in tasks {
        run_inner(config, shared, session, SftpOperation::TreeGet, id, None,false).await;
    }
}
pub(super) async fn pause_all(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    pause: bool,
) {
    let ids = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.trees
            .values()
            .filter(|r| !r.state.finished())
            .map(|r| r.id)
            .collect::<Vec<_>>()
    };
    for id in ids {
        run(
            config,
            shared,
            session,
            if pause {
                SftpOperation::TreePause
            } else {
                SftpOperation::TreeContinue
            },
            id,
            None,
        )
        .await;
    }
}
