use super::*;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConnectionPhase {
    Connecting,
    Connected,
    Disconnecting,
    #[default]
    Disconnected,
    Uncertain,
}
impl ConnectionPhase {
    pub fn busy(self) -> bool {
        matches!(self, Self::Connecting | Self::Disconnecting)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "连接中",
            Self::Connected => "已连接",
            Self::Disconnecting => "断开中",
            Self::Disconnected => "已断开",
            Self::Uncertain => "状态未知",
        }
    }
}
pub fn connect(
    profile: cshell_domain::ProfileId,
    previous: Option<SessionId>,
) -> SftpClientCommand {
    SftpClientCommand::ConnectProfile {
        session: SessionId::new(),
        profile,
        previous,
        source_ssh_session: None,
    }
}
pub fn connect_shared(profile:cshell_domain::ProfileId,previous:Option<SessionId>,source_ssh_session:Option<SessionId>)->SftpClientCommand {
    SftpClientCommand::ConnectProfile{session:SessionId::new(),profile,previous,source_ssh_session}
}
async fn disconnect(config: &DesktopConnectionConfig, session: SessionId) -> Result<(), String> {
    let reply = send(
        config,
        request(
            SftpOperation::Disconnect,
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
    Ok(())
}
fn closed(shared: &Arc<Mutex<SftpView>>, session: SessionId, error: Option<String>) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) {
        return;
    }
    view.connection_phase = if error.is_some() {
        ConnectionPhase::Uncertain
    } else {
        ConnectionPhase::Disconnected
    };
    if let Some(info) = &mut view.connection_info {
        info.running = false;
    }
    view.queue_writable = false;
    view.file_writable = false;
    view.edit_writable = false;
    view.queue_busy = false;
    view.workspace_busy = false;
    view.local_cursor = None;
    view.remote_cursor = None;
    view.remote_loaded_directory = None;
    view.queue_hold = true;
    view.status = if error.is_some() {
        "SFTP 未连接或状态无法确认，请重连检查"
    } else {
        "SFTP 已断开；可点击重新连接"
    }
    .into();
    view.error = error;
    view.generation = view.generation.wrapping_add(1);
}
pub(super) async fn run(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    command: SftpClientCommand,
) {
    let (session, profile, previous, source_ssh_session) = match command {
        SftpClientCommand::Disconnect(session) => {
            let previous = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connection_previous;
            if let Some(previous) = previous.filter(|p| *p != session) {
                if let Err(error) = disconnect(config, previous).await {
                    closed(shared, session, Some(error));
                    return;
                }
                shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .connection_previous = None;
            }
            let result = disconnect(config, session).await;
            closed(shared, session, result.err());
            return;
        }
        SftpClientCommand::ConnectProfile {
            session,
            profile,
            previous,
            source_ssh_session,
        } => (session, profile, previous, source_ssh_session),
        _ => return,
    };
    if let Some(previous) = previous {
        if let Err(error) = disconnect(config, previous).await {
            closed(
                shared,
                session,
                Some(format!("未能确认旧 SFTP 已断开：{error}")),
            );
            return;
        }
        shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connection_previous = None;
    }
    let mut query = request(
        SftpOperation::ConnectProfile,
        session,
        String::new(),
        String::new(),
        Vec::new(),
    );
    query.document_json = serde_json::to_vec(&cshell_application::SftpConnectRequest {
        profile,source_ssh_session,discover_directory:true,
    }).unwrap_or_default();
    let mut cancelled=false;
    let capture=shared.clone();
    let result = crate::connection_diagnostics::observe(true,move|trace|{let mut view=capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.session_id==Some(session){view.connection_trace=Some(trace);view.generation=view.generation.wrapping_add(1);}},send(config, query)).await;
    if result.is_err(){let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.session_id==Some(session){crate::connection_diagnostics::uncertain(&mut view.connection_trace);}}
    let result=result.and_then(|reply| {
        cancelled=reply.status==SftpStatus::Cancelled as i32 || reply.status==SftpStatus::Failed as i32 && reply.detail=="AuthenticationRejected: SSH 验证已取消，连接未建立。";
        if reply.status != SftpStatus::Ok as i32 {
            return Err(reply.detail);
        }
        let info: cshell_application::SftpConnectionInfo =
            serde_json::from_slice(&reply.document_json).map_err(|_| "SFTP 连接回复无效")?;
        if info.session != session || info.profile != profile || !info.running || info.shared_from != source_ssh_session {
            return Err("SFTP 连接回复与请求不匹配".into());
        }
        Ok(info)
    });
    match result {
        Ok(info) => {
            let close = {
                let mut view = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if view.session_id != Some(session) {
                    return;
                }
                view.peer = Some(info.peer.clone());
                view.connection_info = Some(info);
                view.error = None;
                view.status = if view.connection_info.as_ref().is_some_and(|i|i.shared_from.is_some()) {"SFTP 已连接 · 复用 SSH，无需再次认证"} else {"SFTP 已连接 · 独立连接"}.into();
                view.generation = view.generation.wrapping_add(1);
                view.close_requested
            };
            if close {
                let result = disconnect(config, session).await;
                closed(shared, session, result.err());
            } else {
                load_browser(config, shared, session).await;
                let close = {
                    let mut view = shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let close = view.close_requested;
                    if !close {
                        view.connection_phase = ConnectionPhase::Connected;
                        view.status = if view.connection_info.as_ref().is_some_and(|i|i.shared_from.is_some()) {"SFTP 已连接 · 复用 SSH，无需再次认证"} else {"SFTP 已连接 · 独立连接"}.into();
                        view.generation = view.generation.wrapping_add(1);
                    }
                    close
                };
                if close {
                    let result = disconnect(config, session).await;
                    closed(shared, session, result.err());
                }
            }
        }
        Err(_) if cancelled=>{
            closed(shared,session,None);
            let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if view.session_id==Some(session) {
                view.connection_cancelled=true;view.status="已取消 SFTP 连接".into();
                view.generation=view.generation.wrapping_add(1);
            }
        },
        Err(error) => closed(shared, session, Some(error)),
    }
}
pub(super) async fn load_browser(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
) {
    let query = request(
        SftpOperation::QueueGet,
        session,
        String::new(),
        String::new(),
        Vec::new(),
    );
    // The first acknowledged queue is the write baseline for this connection.
    // Reopening an existing browser must not clear an uncertain mutation; that
    // still requires the user's explicit QueueGet refresh.
    let initial_load = {
        let view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_id == Some(session) && view.queue.is_none() && !view.queue_busy
    };
    publish_queue(shared, session, send(config, query).await, initial_load, None);
    let epoch = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workspace_epoch;
    workspace::run_command(
        config,
        shared,
        session,
        WorkspaceCommand {
            operation: SftpOperation::PeerGet,
            data: Vec::new(),
            revision: 0,
            object: Vec::new(),
            source_session: None,
            epoch,
        },
    )
    .await;
    let (shared_local,remote_directory)={
        let view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (view.shared_local_browser,view.remote_loaded_directory.clone()
            .or_else(||view.connection_info.as_ref().and_then(|i|i.initial_directory.clone()))
            .unwrap_or_else(||"/".into()))
    };
    for (local, path) in [
        (true, crate::sftp_settings::local_directory()),
        (false, remote_directory),
    ] {
        if local && shared_local {continue;}
        if let Err(error) = browse_command(
            config,
            shared,
            SftpClientCommand::BrowseManaged(
                session,
                SftpBrowseOptions {
                    local,
                    path,
                    links: SftpLinkPolicy::NoFollow,
                },
            ),
        )
        .await
        {
            let mut view = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if view.session_id == Some(session) {
                view.error = Some(error);
                view.generation = view.generation.wrapping_add(1);
            }
        }
    }
}
pub(super) async fn poll(config: &DesktopConnectionConfig, shared: &Arc<Mutex<SftpView>>) {
    let session = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.connection_profile.is_none() || view.connection_phase != ConnectionPhase::Connected
        {
            return;
        }
        view.session_id
    };
    let Some(session) = session else {
        return;
    };
    let expected = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.connection_info.clone()
    };
    let result = send(
        config,
        request(
            SftpOperation::ConnectionStatus,
            session,
            String::new(),
            String::new(),
            Vec::new(),
        ),
    )
    .await
    .and_then(|reply| {
        if reply.status != SftpStatus::Ok as i32 {
            return Err(reply.detail);
        }
        let info: cshell_application::SftpConnectionInfo =
            serde_json::from_slice(&reply.document_json).map_err(|_| "SFTP 状态回复无效")?;
        if info.session != session
            || expected
                .as_ref()
                .is_some_and(|e| e.profile != info.profile || e.peer != info.peer)
            || !info.running
        {
            return Err("SFTP 连接已中断，请重新连接".into());
        }
        Ok(())
    });
    if let Err(error) = result {
        closed(shared, session, Some(error));
    }
}

#[cfg(test)]
mod tests;

pub(super) async fn shutdown(config: &DesktopConnectionConfig, shared: &Arc<Mutex<SftpView>>) {
    let (session, previous) = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            view.connection_profile.and(view.session_id),
            view.connection_previous,
        )
    };
    let local={let view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.connection_profile.is_none(){view.session_id.zip(view.local_cursor.clone())}else{None}};
    let cleanup = async {
        if let Some((session,cursor))=local {
            let _=send(config,SftpRequest{operation:SftpOperation::BrowseClose as i32,session_id:session.as_uuid().as_bytes().to_vec(),transfer_id:cursor.id,expected_revision:cursor.revision,..Default::default()}).await;
        }
        if let Some(previous) = previous {
            let _ = disconnect(config, previous).await;
        }
        if let Some(session) = session.filter(|s| Some(*s) != previous) {
            let _ = disconnect(config, session).await;
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), cleanup).await;
}
