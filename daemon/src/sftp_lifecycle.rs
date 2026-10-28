//! Managed SFTP views use their own subsystem channels and may share authenticated SSH.
use crate::{SessionIpcService, SshSession, SshSessionError, SshSessionRegistry};
use cshell_domain::{ClipboardTrust, ProfileId, SessionId};
use cshell_ipc::{SftpOperation, SftpRequest, SftpResponse, SftpStatus, features};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Default)]
pub(crate) struct Lifecycle {
    state: Mutex<LifecycleState>,
}

#[derive(Debug, Default)]
struct LifecycleState {
    gates: BTreeMap<SessionId, Weak<OperationGate>>,
    opening: BTreeSet<SessionId>,
}

#[derive(Debug)]
struct OperationGate {
    mutex: tokio::sync::Mutex<()>,
    cancelled: tokio::sync::watch::Sender<bool>,
}

/// A pending connection counts toward capacity until publication or cancellation.
struct Reservation<'a> {
    registry: &'a SshSessionRegistry,
    session: SessionId,
}

impl Reservation<'_> {
    fn commit(&self, target: Arc<SftpTarget>) {
        let mut state = self
            .registry
            .sftp_lifecycle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.registry
            .sftp_targets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.session, target);
        state.opening.remove(&self.session);
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.registry
            .sftp_lifecycle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .opening
            .remove(&self.session);
    }
}

pub(crate) enum SftpTarget {
    Terminal(Arc<SshSession>),
    Shared {
        info: cshell_application::SftpConnectionInfo,
        lease: crate::shared_ssh::TransportLease,
        stopped: AtomicBool,
    },
    Dedicated {
        info: cshell_application::SftpConnectionInfo,
        transport: crate::ssh_route::RoutedSshConnection,
        stopped: AtomicBool,
    },
}
impl SftpTarget {
    pub(crate) fn generation(&self) -> u64 {
        match self {
            Self::Terminal(s) => s.transport_generation(),
            _ => 0,
        }
    }
    pub(crate) fn checksum_connection(&self) -> Option<Arc<cshell_ssh::SshConnection>> {
        if !self.running() {
            return None;
        }
        match self {
            Self::Terminal(s) => s.checksum_connection(),
            Self::Shared { lease, .. } => Some(lease.client().clone()),
            Self::Dedicated { transport, .. } => Some(Arc::new(transport.client.clone())),
        }
    }
    pub(crate) fn running(&self) -> bool {
        match self {
            Self::Terminal(s) => s.running(),
            Self::Shared { lease, stopped, .. } => {
                !stopped.load(Ordering::Acquire) && lease.running()
            }
            Self::Dedicated {
                transport, stopped, ..
            } => !stopped.load(Ordering::Acquire) && !transport.client.transport_closed(),
        }
    }
    pub(crate) fn clipboard_trust(&self) -> Option<&ClipboardTrust> {
        match self {
            Self::Terminal(s) => s.clipboard_trust(),
            Self::Dedicated { info, .. } | Self::Shared { info, .. } => Some(&info.peer),
        }
    }
    pub(crate) async fn open_sftp(&self) -> Result<cshell_sftp::SftpClient, SshSessionError> {
        if !self.running() {
            return Err(SshSessionError::Closed);
        }
        match self {
            Self::Terminal(s) => s.open_sftp().await,
            Self::Shared { lease, .. } => Ok(lease.client().open_sftp().await?),
            Self::Dedicated { transport, .. } => Ok(transport.client.open_sftp().await?),
        }
    }
    fn info(&self) -> Option<cshell_application::SftpConnectionInfo> {
        if let Self::Dedicated { info, .. } | Self::Shared { info, .. } = self {
            let mut info = info.clone();
            info.running = self.running();
            Some(info)
        } else {
            None
        }
    }
    pub(crate) async fn close(&self) {
        if let Self::Shared { lease, stopped, .. } = self {
            stopped.store(true, Ordering::Release);
            lease.release().await;
        }
        if let Self::Dedicated {
            transport, stopped, ..
        } = self
        {
            stopped.store(true, Ordering::Release);
            let _ = transport.client.disconnect_confirmed().await;
            if let Some(jump) = &transport.jump {
                jump.disconnect().await;
            }
        }
    }
}
impl SshSessionRegistry {
    fn sftp_gate(&self, session: SessionId) -> Arc<OperationGate> {
        let mut state = self
            .sftp_lifecycle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.gates.retain(|_, gate| gate.strong_count() != 0);
        if let Some(gate) = state.gates.get(&session).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(OperationGate {
            mutex: tokio::sync::Mutex::new(()),
            cancelled: tokio::sync::watch::channel(false).0,
        });
        state.gates.insert(session, Arc::downgrade(&gate));
        gate
    }

    fn reserve_sftp(&self, session: SessionId) -> Result<Reservation<'_>, (SftpStatus, String)> {
        let mut state = self
            .sftp_lifecycle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let targets = self
            .sftp_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if targets.contains_key(&session) || state.opening.contains(&session) {
            return Err((SftpStatus::Conflict, "连接标识已被使用，请重新连接".into()));
        }
        if targets
            .values()
            .filter(|target| target.info().is_some())
            .count()
            + state.opening.len()
            >= 64
        {
            return Err((SftpStatus::CapacityReached, "SFTP 连接数量达到上限".into()));
        }
        state.opening.insert(session);
        Ok(Reservation {
            registry: self,
            session,
        })
    }

    pub(crate) fn get_sftp(&self, id: SessionId) -> Result<Arc<SftpTarget>, SshSessionError> {
        self.sftp_targets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
            .ok_or(SshSessionError::Unknown(id))
    }
}
impl SessionIpcService {
    pub(crate) async fn shutdown_sftp(&self) {
        let targets = {
            let mut map = self
                .ssh_sessions
                .sftp_targets
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *map)
        };
        let mut closing = tokio::task::JoinSet::new();
        for target in targets.into_values() {
            closing.spawn(async move {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(3), target.close()).await;
            });
        }
        while closing.join_next().await.is_some() {}
    }
    pub(crate) async fn sftp_lifecycle(
        &self,
        request: &SftpRequest,
        negotiated: u64,
    ) -> SftpResponse {
        let result = self.sftp_lifecycle_inner(request, negotiated).await;
        match result {
            Ok(Some(info)) => SftpResponse {
                document_json: serde_json::to_vec(&info).unwrap_or_default(),
                ..Default::default()
            },
            Ok(None) => SftpResponse::default(),
            Err((status, message)) => SftpResponse::with_status(status, message),
        }
    }
    async fn sftp_lifecycle_inner(
        &self,
        request: &SftpRequest,
        negotiated: u64,
    ) -> Result<Option<cshell_application::SftpConnectionInfo>, (SftpStatus, String)> {
        let invalid = || (SftpStatus::InvalidRequest, "无效的 SFTP 连接请求".into());
        let operation = SftpOperation::try_from(request.operation).map_err(|_| invalid())?;
        let session = SessionId::from_bytes(
            request
                .session_id
                .as_slice()
                .try_into()
                .map_err(|_| invalid())?,
        );
        if session.as_uuid().is_nil()
            || !request.remote_path.is_empty()
            || !request.local_path.is_empty()
            || !request.transfer_id.is_empty()
            || request.max_entries != 0
            || request.expected_revision != 0
            || request.confirmed
        {
            return Err(invalid());
        }
        if !matches!(
            operation,
            SftpOperation::ConnectProfile
                | SftpOperation::Disconnect
                | SftpOperation::ConnectionStatus
        ) {
            return Err(invalid());
        }
        if operation == SftpOperation::ConnectionStatus {
            if !request.document_json.is_empty() {
                return Err(invalid());
            }
            return self
                .ssh_sessions
                .get_sftp(session)
                .ok()
                .and_then(|s| s.info())
                .map(Some)
                .ok_or((SftpStatus::Unavailable, "SFTP 已断开".into()));
        }
        if operation == SftpOperation::Disconnect {
            if !request.document_json.is_empty() {
                return Err(invalid());
            }
            if let Some(profiles) = &self.profiles {
                profiles.vault.keyboard_auth.cancel_sftp(session);
            }
        }
        let gate = self.ssh_sessions.sftp_gate(session);
        if operation == SftpOperation::Disconnect {
            gate.cancelled.send_replace(true);
        }
        let _gate = gate.mutex.lock().await;
        if operation == SftpOperation::Disconnect {
            if let Ok(target) = self.ssh_sessions.get_sftp(session) {
                if target.info().is_none() {
                    return Err((
                        SftpStatus::InvalidRequest,
                        "不能通过 SFTP 断开终端连接".into(),
                    ));
                }
                // Mark closed before awaiting so no new work can start on this transport.
                if let SftpTarget::Dedicated { stopped, .. } | SftpTarget::Shared { stopped, .. } =
                    target.as_ref()
                {
                    stopped.store(true, Ordering::Release);
                }
                self.sftp.stop_session_transfers(session);
                let paused = self.sftp.suspend_session_queue(session).await;
                self.sftp.close_session_cursors(session).await;
                target.close().await;
                self.ssh_sessions
                    .sftp_targets
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&session);
                if paused.is_err() {
                    return Err((
                        SftpStatus::Failed,
                        "SFTP 已断开，但队列暂停状态保存失败，请检查任务历史".into(),
                    ));
                }
            }
            return Ok(None);
        }
        if negotiated & features::SSH_PROFILE_SESSION == 0
            || negotiated & features::VAULT_CONTROL == 0
        {
            return Err((
                SftpStatus::Unsupported,
                "独立 SFTP 需要协商 SSH 配置和凭据库支持".into(),
            ));
        }
        let options: cshell_application::SftpConnectRequest =
            serde_json::from_slice(&request.document_json)
                .or_else(|_| {
                    serde_json::from_slice::<ProfileId>(&request.document_json).map(|profile| {
                        cshell_application::SftpConnectRequest {
                            profile,
                            source_ssh_session: None,
                            discover_directory: false,
                        }
                    })
                })
                .map_err(|_| invalid())?;
        let reservation = self.ssh_sessions.reserve_sftp(session)?;
        let mut cancelled = gate.cancelled.subscribe();
        tokio::select! {
            biased;
            _ = cancelled.wait_for(|value| *value) => {
                Err((SftpStatus::Cancelled, "SFTP 连接已取消".into()))
            }
            result = self.connect_sftp(session, options, &reservation) => result,
        }
    }

    async fn connect_sftp(
        &self,
        session: SessionId,
        options: cshell_application::SftpConnectRequest,
        reservation: &Reservation<'_>,
    ) -> Result<Option<cshell_application::SftpConnectionInfo>, (SftpStatus, String)> {
        let profile = options.profile;
        if let Some(source) = options.source_ssh_session {
            let source = self.ssh_sessions.get(source).map_err(|_| {
                (
                    SftpStatus::Unavailable,
                    "原 SSH 会话已关闭，请先连接 SSH 或独立打开 SFTP".into(),
                )
            })?;
            if source.profile_id() != Some(profile) {
                return Err((
                    SftpStatus::InvalidRequest,
                    "SFTP 配置与原 SSH 会话不匹配".into(),
                ));
            }
            let peer = source
                .clipboard_trust()
                .cloned()
                .ok_or((SftpStatus::Failed, "原 SSH 会话没有已验证的主机身份".into()))?;
            if let Some(peer) = source.clipboard_trust() {
                cshell_ssh::connection_progress::peer(0, &peer.host.host, peer.host.port);
            }
            cshell_ssh::connection_progress::stage(
                cshell_domain::ConnectionStage::ReusingTransport,
            );
            let lease = source
                .fork_transport()
                .map_err(|e| (SftpStatus::Unavailable, e.to_string()))?;
            let desired = if options.discover_directory {
                source.current_remote_directory().await
            } else {
                None
            };
            let probe = async {
                let client = lease.client().open_sftp().await.map_err(|e| {
                    (
                        SftpStatus::Failed,
                        format!("无法在现有 SSH 连接打开 SFTP 渠道：{e}"),
                    )
                })?;
                let directory = if options.discover_directory {
                    cshell_ssh::connection_progress::stage(
                        cshell_domain::ConnectionStage::InitialDirectory,
                    );
                    let home = client
                        .resolve_path(".")
                        .await
                        .unwrap_or_else(|_| "/".into());
                    Some(if let Some(desired) = desired {
                        client.directory_path(&desired, true).await.unwrap_or(home)
                    } else {
                        home
                    })
                } else {
                    None
                };
                let _ = client.close().await;
                Ok::<_, (SftpStatus, String)>(directory)
            };
            let directory = tokio::time::timeout(std::time::Duration::from_secs(15), probe)
                .await
                .map_err(|_| {
                    (
                        SftpStatus::Failed,
                        "现有 SSH 连接的 SFTP 渠道打开超时".into(),
                    )
                })??;
            if !lease.running() {
                return Err((SftpStatus::Unavailable, "SSH 共享连接已断开".into()));
            }
            let info = cshell_application::SftpConnectionInfo {
                session,
                profile,
                title: source.title().to_owned(),
                peer,
                running: true,
                shared_from: Some(source.id()),
                initial_directory: directory,
            };
            reservation.commit(Arc::new(SftpTarget::Shared {
                info: info.clone(),
                lease,
                stopped: AtomicBool::new(false),
            }));
            return Ok(Some(info));
        }
        let profiles = self
            .profiles
            .as_ref()
            .ok_or((SftpStatus::Unavailable, "配置服务不可用".into()))?;
        let plan = match profiles
            .session_plan(profile)
            .await
            .map_err(|e| (SftpStatus::Failed, e))?
        {
            crate::profile_ipc::SavedSessionPlan::Ssh(plan) => plan,
            _ => return Err((SftpStatus::InvalidRequest, "请选择 SSH 服务器配置".into())),
        };
        let known_hosts = self
            .known_hosts_path
            .clone()
            .map_or_else(crate::session_ipc::default_known_hosts_path, Ok)
            .map_err(|e| (SftpStatus::Failed, e))?;
        let vault = &profiles.vault;
        cshell_ssh::connection_progress::peer(0, &plan.target.host, plan.target.port);
        cshell_ssh::connection_progress::stage(cshell_domain::ConnectionStage::Credentials);
        crate::keyboard_auth::for_sftp(
            session,
            vault.prepare_connection(format!(
                "{}@{}:{}",
                plan.target.username, plan.target.host, plan.target.port
            )),
        )
        .await
        .map_err(|error| {
            (
                if matches!(error, cshell_ssh::SshError::AuthenticationCancelled) {
                    SftpStatus::Cancelled
                } else {
                    SftpStatus::Failed
                },
                format!("凭据库解锁未完成：{error}"),
            )
        })?;
        let mut cancellation = vault.cancellation();
        let epoch = *cancellation.borrow();
        let connect = async {
            let transport = crate::keyboard_auth::for_sftp(
                session,
                crate::ssh_route::connect_profile(&plan, &known_hosts, vault),
            )
            .await
            .map_err(|e| {
                (
                    if e.code == cshell_ipc::SessionFailureCode::AuthenticationCancelled {
                        SftpStatus::Cancelled
                    } else {
                        SftpStatus::Failed
                    },
                    format!("{:?}: {}", e.code, e.detail),
                )
            })?;
            let peer = plan
                .verified_peer(&transport.client, transport.jump.as_ref())
                .map_err(|_| (SftpStatus::Failed, "无法验证 SFTP 主机身份".into()))?;
            // Validate the subsystem now, including SFTP-only accounts that deny PTYs/shells.
            let probe = transport
                .client
                .open_sftp()
                .await
                .map_err(|e| (SftpStatus::Failed, format!("SFTP 子系统不可用：{e}")))?;
            cshell_ssh::connection_progress::stage(
                cshell_domain::ConnectionStage::InitialDirectory,
            );
            let directory = if options.discover_directory {
                Some(probe.resolve_path(".").await.unwrap_or_else(|_| "/".into()))
            } else {
                None
            };
            let _ = probe.close().await;
            Ok::<_, (SftpStatus, String)>((transport, peer, directory))
        };
        let (transport, peer, directory) = tokio::select! {
            biased;
            _=cancellation.changed()=>return Err((SftpStatus::Failed,"CredentialUnavailable: 凭据库已锁定".into())),
            result=tokio::time::timeout(std::time::Duration::from_secs(if plan.target.options.keyboard_interactive || !plan.target.options.authentication_order.is_empty() || plan.target.username.is_empty() || plan.target.auth_method==cshell_domain::SshAuthMethod::Password || plan.jumps.iter().any(|p|p.options.keyboard_interactive || !p.options.authentication_order.is_empty() || p.username.is_empty() || p.auth_method==cshell_domain::SshAuthMethod::Password) {300} else {35}),connect)=>result.map_err(|_|(SftpStatus::Failed,"SFTP 连接超时".into()))??,
        };
        let info = cshell_application::SftpConnectionInfo {
            session,
            profile,
            title: plan.title.clone(),
            peer,
            running: true,
            shared_from: None,
            initial_directory: directory,
        };
        let target = Arc::new(SftpTarget::Dedicated {
            info: info.clone(),
            transport,
            stopped: AtomicBool::new(false),
        });
        if vault
            .commit_auth(epoch, || reservation.commit(target.clone()))
            .is_err()
        {
            target.close().await;
            return Err((
                SftpStatus::Failed,
                "CredentialUnavailable: 凭据库已锁定".into(),
            ));
        }
        Ok(Some(info))
    }
}

impl std::fmt::Debug for SftpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpTarget")
            .field("running", &self.running())
            .field("dedicated", &self.info().is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
