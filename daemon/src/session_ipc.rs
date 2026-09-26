use crate::{
    LocalSessionError, LocalSessionInfo, LocalSessionRegistry, ProfileIpcService,
    SessionRegistryError, SshSession, SshSessionError, SshSessionRegistry, SubscriptionFrame,
    TerminalFrameSubscription,
};
use cshell_domain::{ProfileId, SessionId, TerminalSize};
use cshell_ipc::{
    Envelope, HistorySearchCodecError, HistorySearchMatch, HistorySearchResult, IpcError, LogPage,
    LogPageCodecError, LogRow, LogStyleSpan, SessionCloseResponse, SessionCreateResponse,
    SessionFailureCode, SessionListResponse, SessionSummary, TerminalControlCodecError,
    TerminalControlResponse, TerminalControlStatus, TerminalInputRequest, TerminalResizeRequest,
    TerminalStyle, envelope, read_envelope, write_envelope,
};
use cshell_output_store::{JournalColor, JournalStyle};
use cshell_ssh::{HostKeyCheck, SshError};
use cshell_terminal::{Color, Style};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum SessionIpcError {
    #[error(transparent)]
    Transport(#[from] IpcError),
    #[error(transparent)]
    Registry(#[from] SessionRegistryError),
    #[error("IPC request is not a supported session data-plane message")]
    UnsupportedRequest,
    #[error("terminal subscription has no initial frame")]
    InitialFrameUnavailable,
    #[error(transparent)]
    InvalidLogPage(#[from] LogPageCodecError),
    #[error(transparent)]
    InvalidHistorySearch(#[from] HistorySearchCodecError),
    #[error(transparent)]
    InvalidTerminalControl(#[from] TerminalControlCodecError),
    #[error(transparent)]
    LocalSession(#[from] LocalSessionError),
}

/// Authenticated IPC data-plane dispatcher for daemon-owned terminal sessions.
#[derive(Clone, Debug)]
pub struct SessionIpcService {
    registry: Arc<LocalSessionRegistry>,
    profiles: Option<Arc<ProfileIpcService>>,
    ssh_sessions: Arc<SshSessionRegistry>,
    known_hosts_path: Option<std::path::PathBuf>,
}

#[derive(Debug)]
struct DispatchResult {
    response: Envelope,
    subscription: Option<TerminalFrameSubscription>,
}

pub(crate) struct SshLaunchFailure {
    pub(crate) code: SessionFailureCode,
    pub(crate) detail: String,
}
impl SshLaunchFailure {
    pub(crate) fn new(code: SessionFailureCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}
impl From<String> for SshLaunchFailure {
    fn from(detail: String) -> Self {
        Self::new(SessionFailureCode::InvalidConfiguration, detail)
    }
}
impl From<&str> for SshLaunchFailure {
    fn from(detail: &str) -> Self {
        Self::from(detail.to_owned())
    }
}

impl SessionIpcService {
    #[must_use]
    pub fn new(registry: Arc<LocalSessionRegistry>) -> Self {
        Self {
            registry,
            profiles: None,
            ssh_sessions: Arc::new(SshSessionRegistry::default()),
            known_hosts_path: None,
        }
    }

    #[must_use]
    pub fn with_known_hosts_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.known_hosts_path = Some(path.into());
        self
    }

    pub fn with_profiles(mut self, profiles: Arc<ProfileIpcService>) -> Self {
        self.profiles = Some(profiles);
        self
    }

    pub fn handle_request(&self, request: Envelope) -> Result<Envelope, SessionIpcError> {
        self.dispatch(request).map(|result| result.response)
    }

    /// Serves one request after the connection has completed authenticated handshake.
    pub async fn serve_one<S>(&self, stream: &mut S) -> Result<(), SessionIpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let request = read_envelope(stream).await?;
        let response = self.dispatch_any(request).await?.response;
        write_envelope(stream, &response).await?;
        Ok(())
    }

    /// Keeps an authenticated client subscribed at display refresh cadence.
    ///
    /// The reader owns its half of the stream so reads are never cancelled midway
    /// through a length-prefixed frame. The writer polls a latest-only subscription;
    /// slow clients therefore coalesce generations instead of accumulating snapshots.
    pub async fn serve_connection<S>(&self, stream: S) -> Result<(), SessionIpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_connection_with_features(stream, u64::MAX).await
    }

    pub async fn serve_connection_with_features<S>(
        &self,
        mut stream: S,
        negotiated_features: u64,
    ) -> Result<(), SessionIpcError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let initial_request = read_envelope(&mut stream).await?;
        let initial = self
            .dispatch_with_features(initial_request, negotiated_features)
            .await?;
        let mut subscription = initial.subscription;
        write_envelope(&mut stream, &initial.response).await?;

        let (mut reader, mut writer) = tokio::io::split(stream);
        let (request_sender, mut requests) = tokio::sync::mpsc::channel(8);
        let reader_worker = tokio::spawn(async move {
            loop {
                let request = read_envelope(&mut reader).await;
                let stop = request.is_err();
                if request_sender.send(request).await.is_err() || stop {
                    break;
                }
            }
        });
        let mut refresh = tokio::time::interval(std::time::Duration::from_millis(16));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let result = loop {
            tokio::select! {
                incoming = requests.recv() => {
                    let Some(incoming) = incoming else {
                        break Ok(());
                    };
                    let request = match incoming {
                        Ok(request) => request,
                        Err(error) if is_client_disconnect(&error) => break Ok(()),
                        Err(error) => break Err(SessionIpcError::Transport(error)),
                    };
                    let dispatched = match self.dispatch_with_features(request, negotiated_features).await {
                        Ok(dispatched) => dispatched,
                        Err(error) => break Err(error),
                    };
                    if let Some(next_subscription) = dispatched.subscription {
                        subscription = Some(next_subscription);
                    }
                    if let Err(error) = write_envelope(&mut writer, &dispatched.response).await {
                        break if is_client_disconnect(&error) {
                            Ok(())
                        } else {
                            Err(SessionIpcError::Transport(error))
                        };
                    }
                }
                _ = refresh.tick() => {
                    let Some(frame) = subscription.as_mut().and_then(TerminalFrameSubscription::poll) else {
                        continue;
                    };
                    let envelope = Envelope {
                        request_id: 0,
                        deadline_unix_ms: 0,
                        payload: Some(frame.into_payload()),
                    };
                    if let Err(error) = write_envelope(&mut writer, &envelope).await {
                        break if is_client_disconnect(&error) {
                            Ok(())
                        } else {
                            Err(SessionIpcError::Transport(error))
                        };
                    }
                }
            }
        };
        reader_worker.abort();
        result
    }

    async fn dispatch_any(&self, request: Envelope) -> Result<DispatchResult, SessionIpcError> {
        self.dispatch_with_features(request, u64::MAX).await
    }

    async fn dispatch_with_features(
        &self,
        mut request: Envelope,
        negotiated_features: u64,
    ) -> Result<DispatchResult, SessionIpcError> {
        if matches!(
            request.payload.as_ref(),
            Some(envelope::Payload::ProfileRequest(_))
        ) {
            let Some(envelope::Payload::ProfileRequest(profile_request)) = request.payload.take()
            else {
                unreachable!("profile payload was matched above")
            };
            let response = if negotiated_features & cshell_ipc::features::PROFILE_CONTROL == 0 {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    revision: 0,
                    catalog: None,
                    preview: None,
                    host_key_preview: None,
                    local_shells: Vec::new(),
                    detail: "Profile control was not negotiated".to_owned(),
                }
            } else if negotiated_features & cshell_ipc::features::SSH_PROFILE_TARGET == 0
                && profile_request.changes.iter().any(|change| {
                    matches!(
                        change.change.as_ref(),
                        Some(cshell_ipc::profile_change::Change::UpsertSshConnection(_))
                            | Some(cshell_ipc::profile_change::Change::RemoveSshConnection(_))
                    )
                })
            {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    revision: 0,
                    catalog: None,
                    preview: None,
                    host_key_preview: None,
                    local_shells: Vec::new(),
                    detail: "SSH Profile target control was not negotiated".to_owned(),
                }
            } else if matches!(
                cshell_ipc::ProfileOperation::try_from(profile_request.operation),
                Ok(cshell_ipc::ProfileOperation::SetPassword
                    | cshell_ipc::ProfileOperation::DeletePassword)
            ) && negotiated_features & cshell_ipc::features::SSH_PROFILE_SESSION == 0
            {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    revision: 0,
                    catalog: None,
                    preview: None,
                    host_key_preview: None,
                    local_shells: Vec::new(),
                    detail: "SSH Profile credentials were not negotiated".into(),
                }
            } else if negotiated_features & cshell_ipc::features::SSH_HOST_KEY_IMPORT == 0
                && matches!(
                    cshell_ipc::ProfileOperation::try_from(profile_request.operation),
                    Ok(cshell_ipc::ProfileOperation::PreviewHostKey
                        | cshell_ipc::ProfileOperation::ConfirmHostKey)
                )
            {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    detail: "SSH host-key import was not negotiated".into(),
                    ..cshell_ipc::ProfileResponse::default()
                }
            } else if negotiated_features & cshell_ipc::features::SSH_PROFILE_ROUTE == 0
                && profile_request.changes.iter().any(|change| {
                    matches!(
                        &change.change,
                        Some(
                            cshell_ipc::profile_change::Change::UpsertSshConnection(_)
                                | cshell_ipc::profile_change::Change::RemoveSshConnection(_)
                        )
                    )
                })
            {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    detail: "SSH Profile routes were not negotiated".into(),
                    ..cshell_ipc::ProfileResponse::default()
                }
            } else if negotiated_features & cshell_ipc::features::SSH_PROFILE_AUTH == 0
                && (matches!(
                    cshell_ipc::ProfileOperation::try_from(profile_request.operation),
                    Ok(cshell_ipc::ProfileOperation::SetKeyPassphrase
                        | cshell_ipc::ProfileOperation::DeleteKeyPassphrase)
                ) || profile_request.changes.iter().any(|change| {
                    matches!(
                        change.change.as_ref(),
                        Some(cshell_ipc::profile_change::Change::UpsertSshConnection(value))
                            if value.auth_method != 0
                                || value.private_key_path.is_some()
                                || value.certificate_path.is_some()
                                || value.agent_backend != 0
                                || value.agent_identity.is_some()
                    )
                }))
            {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    revision: 0,
                    catalog: None,
                    preview: None,
                    host_key_preview: None,
                    local_shells: Vec::new(),
                    detail: "SSH Profile authentication was not negotiated".into(),
                }
            } else if negotiated_features & cshell_ipc::features::LOCAL_PROFILE == 0
                && (profile_request.operation
                    == cshell_ipc::ProfileOperation::DiscoverLocalShells as i32
                    || profile_request.changes.iter().any(|change| {
                        matches!(
                            &change.change,
                            Some(
                                cshell_ipc::profile_change::Change::UpsertLocalConnection(_)
                                    | cshell_ipc::profile_change::Change::RemoveLocalConnection(_)
                            )
                        )
                    }))
            {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    detail: "Local Profile control was not negotiated".into(),
                    ..cshell_ipc::ProfileResponse::default()
                }
            } else if negotiated_features & cshell_ipc::features::LOCAL_LAUNCH_OPTIONS == 0
                && profile_request.changes.iter().any(|change| {
                    matches!(
                        &change.change,
                        Some(cshell_ipc::profile_change::Change::UpsertLocalConnection(_))
                    )
                })
            {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unsupported as i32,
                    detail: "Local launch options were not negotiated".into(),
                    ..cshell_ipc::ProfileResponse::default()
                }
            } else if let Some(profiles) = &self.profiles {
                profiles.handle(profile_request).await
            } else {
                cshell_ipc::ProfileResponse {
                    status: cshell_ipc::ProfileStatus::Unavailable as i32,
                    revision: 0,
                    catalog: None,
                    preview: None,
                    host_key_preview: None,
                    local_shells: Vec::new(),
                    detail: "Profile storage is unavailable".to_owned(),
                }
            };
            return Ok(DispatchResult {
                response: Envelope {
                    request_id: request.request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::ProfileResponse(response)),
                },
                subscription: None,
            });
        }
        if let Some(envelope::Payload::SessionCreateRequest(create)) = &request.payload
            && create.profile_id.is_none()
            && create.local_launch.is_some()
        {
            return Ok(DispatchResult {
                response: Envelope {
                    request_id: request.request_id,
                    payload: Some(envelope::Payload::SessionCreateResponse(
                        SessionCreateResponse {
                            session: None,
                            detail: "Local launch overrides require a saved Profile".into(),
                            failure_code: SessionFailureCode::InvalidConfiguration as i32,
                        },
                    )),
                    ..Envelope::default()
                },
                subscription: None,
            });
        }
        if let Some(envelope::Payload::SessionCreateRequest(create)) = &request.payload
            && let Some(profile_id) = &create.profile_id
        {
            let result = self
                .spawn_profile(
                    profile_id,
                    create.rows,
                    create.cols,
                    create.local_launch.as_ref(),
                    negotiated_features,
                )
                .await;
            let (session, detail, failure_code) = match result {
                Ok(session) => (Some(session), String::new(), SessionFailureCode::None),
                Err(error) => (None, error.detail, error.code),
            };
            return Ok(DispatchResult {
                response: Envelope {
                    request_id: request.request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionCreateResponse(
                        SessionCreateResponse {
                            session,
                            detail,
                            failure_code: failure_code as i32,
                        },
                    )),
                },
                subscription: None,
            });
        }
        if let Some(envelope::Payload::SnapshotRequest(snapshot)) = &request.payload {
            let id = LocalSessionRegistry::parse_session_id(&snapshot.session_id)?;
            if self.ssh_sessions.contains(id) {
                let session = self
                    .ssh_sessions
                    .get(id)
                    .map_err(|_| SessionIpcError::UnsupportedRequest)?;
                let mut subscription = session.subscribe();
                let Some(SubscriptionFrame::Full(frame)) = subscription.poll() else {
                    return Err(SessionIpcError::InitialFrameUnavailable);
                };
                return Ok(DispatchResult {
                    response: Envelope {
                        request_id: request.request_id,
                        deadline_unix_ms: 0,
                        payload: Some(envelope::Payload::FullFrame(frame)),
                    },
                    subscription: Some(subscription),
                });
            }
        }
        if let Some(envelope::Payload::SessionCloseRequest(close)) = &request.payload {
            if close.apply_view_policy
                && negotiated_features & cshell_ipc::features::LOCAL_LAUNCH_OPTIONS == 0
            {
                return Err(SessionIpcError::UnsupportedRequest);
            }
            let id = LocalSessionRegistry::parse_session_id(&close.session_id)?;
            if close.apply_view_policy {
                if self.ssh_sessions.contains(id) {
                    // SSH view close also preserves the connection. Explicit close terminates it.
                } else {
                    let registry = Arc::clone(&self.registry);
                    tokio::task::spawn_blocking(move || registry.close_view(id))
                        .await
                        .map_err(|_| SessionIpcError::UnsupportedRequest)??;
                }
                return Ok(DispatchResult {
                    response: Envelope {
                        request_id: request.request_id,
                        deadline_unix_ms: 0,
                        payload: Some(envelope::Payload::SessionCloseResponse(
                            SessionCloseResponse {
                                session_id: close.session_id.clone(),
                            },
                        )),
                    },
                    subscription: None,
                });
            }
            if self.ssh_sessions.contains(id) {
                let session = self
                    .ssh_sessions
                    .remove(id)
                    .map_err(|_| SessionIpcError::UnsupportedRequest)?;
                session
                    .close()
                    .await
                    .map_err(|_| SessionIpcError::UnsupportedRequest)?;
                return Ok(DispatchResult {
                    response: Envelope {
                        request_id: request.request_id,
                        deadline_unix_ms: 0,
                        payload: Some(envelope::Payload::SessionCloseResponse(
                            SessionCloseResponse {
                                session_id: close.session_id.clone(),
                            },
                        )),
                    },
                    subscription: None,
                });
            }
        }
        self.dispatch(request)
    }

    async fn spawn_profile(
        &self,
        bytes: &[u8],
        rows: u32,
        cols: u32,
        local_launch: Option<&cshell_ipc::LocalLaunchOptions>,
        negotiated_features: u64,
    ) -> Result<SessionSummary, SshLaunchFailure> {
        let profile_id = ProfileId::from_bytes(bytes.try_into().map_err(|_| "invalid Profile ID")?);
        let profiles = self.profiles.as_ref().ok_or_else(|| {
            SshLaunchFailure::new(
                SessionFailureCode::StorageUnavailable,
                "Profile storage unavailable",
            )
        })?;
        let plan = profiles.session_plan(profile_id).await?;
        let rows = u16::try_from(rows)
            .ok()
            .filter(|value| *value > 0)
            .ok_or("invalid terminal rows")?;
        let cols = u16::try_from(cols)
            .ok()
            .filter(|value| *value > 0)
            .ok_or("invalid terminal columns")?;
        if usize::from(rows) * usize::from(cols) > 1_000_000 {
            return Err("terminal dimensions exceed the limit".into());
        }
        let plan = match plan {
            crate::profile_ipc::SavedSessionPlan::Local(mut profile, close_policy) => {
                if negotiated_features & cshell_ipc::features::LOCAL_PROFILE == 0 {
                    return Err(SshLaunchFailure::new(
                        SessionFailureCode::Unsupported,
                        "Local Profile sessions were not negotiated",
                    ));
                }
                if let Some(overrides) = local_launch {
                    if negotiated_features & cshell_ipc::features::LOCAL_LAUNCH_OPTIONS == 0 {
                        return Err(SshLaunchFailure::new(
                            SessionFailureCode::Unsupported,
                            "Local launch options were not negotiated",
                        ));
                    }
                    // Validate the launch layer independently, then the combined layer.
                    // Windows environment names are case insensitive.
                    let mut merged = cshell_domain::LocalConnectionRecord {
                        profile_id,
                        program: profile.program.to_string_lossy().into_owned(),
                        args: profile.args.clone(),
                        cwd: overrides.cwd_path.as_ref().map_or(
                            cshell_domain::LocalWorkingDirectory::Inherit,
                            |path| cshell_domain::LocalWorkingDirectory::Explicit {
                                path: path.clone(),
                            },
                        ),
                        env_overrides: overrides.env_overrides.clone(),
                        close_policy,
                    };
                    cshell_application::validate_local_connection(&merged)
                        .map_err(|_| "invalid Local launch overrides")?;
                    for (key, value) in &overrides.env_overrides {
                        #[cfg(windows)]
                        profile
                            .env_overrides
                            .retain(|existing, _| !existing.eq_ignore_ascii_case(key));
                        profile.env_overrides.insert(key.clone(), value.clone());
                    }
                    merged.env_overrides.clone_from(&profile.env_overrides);
                    cshell_application::validate_local_connection(&merged)
                        .map_err(|_| "invalid Local launch overrides")?;
                    if let Some(path) = &overrides.cwd_path {
                        profile.cwd_policy =
                            cshell_local::WorkingDirectoryPolicy::Explicit(path.into());
                    }
                }
                let registry = Arc::clone(&self.registry);
                return tokio::task::spawn_blocking(move || {
                    registry.spawn_saved_local(
                        profile_id,
                        &profile,
                        TerminalSize::cells(rows, cols),
                        close_policy,
                    )
                })
                .await
                .map_err(|_| {
                    SshLaunchFailure::new(
                        SessionFailureCode::InvalidConfiguration,
                        "Local Profile worker failed",
                    )
                })?
                .map(SessionSummary::from)
                .map_err(|error| {
                    SshLaunchFailure::new(
                        SessionFailureCode::InvalidConfiguration,
                        error.to_string(),
                    )
                });
            }
            crate::profile_ipc::SavedSessionPlan::Ssh(plan) => {
                if local_launch.is_some() {
                    return Err("Local launch overrides require a Local Profile".into());
                }
                if negotiated_features & cshell_ipc::features::SSH_PROFILE_SESSION == 0 {
                    return Err(SshLaunchFailure::new(
                        SessionFailureCode::Unsupported,
                        "SSH Profile sessions were not negotiated",
                    ));
                }
                plan
            }
        };
        let known_hosts_path = self
            .known_hosts_path
            .clone()
            .map_or_else(default_known_hosts_path, Ok)?;
        let connection = crate::ssh_route::connect_profile(&plan, &known_hosts_path).await?;
        let id = SessionId::new();
        let size = TerminalSize::cells(rows, cols);
        let session = SshSession::from_connection(
            id,
            plan.title.clone(),
            connection,
            size,
            &self.registry.journal_path(id),
            256,
        )
        .await
        .map_err(ssh_launch_failure)?;
        let session = self
            .ssh_sessions
            .insert(session.with_profile_id(profile_id));
        Ok(session.summary())
    }

    fn dispatch(&self, request: Envelope) -> Result<DispatchResult, SessionIpcError> {
        let request_id = request.request_id;
        let (payload, subscription) = match request.payload {
            Some(envelope::Payload::SnapshotRequest(snapshot_request)) => {
                let mut subscription = self.registry.subscribe_request(&snapshot_request)?;
                let Some(SubscriptionFrame::Full(full_frame)) = subscription.poll() else {
                    return Err(SessionIpcError::InitialFrameUnavailable);
                };
                (envelope::Payload::FullFrame(full_frame), Some(subscription))
            }
            Some(envelope::Payload::SessionListRequest(_request)) => {
                let sessions = self
                    .registry
                    .list()?
                    .into_iter()
                    .map(SessionSummary::from)
                    .chain(
                        self.ssh_sessions
                            .list()
                            .into_iter()
                            .map(|session| session.summary()),
                    )
                    .collect();
                (
                    envelope::Payload::SessionListResponse(SessionListResponse { sessions }),
                    None,
                )
            }
            Some(envelope::Payload::SessionCreateRequest(request)) => {
                let session = self
                    .registry
                    .spawn_platform_default(request.rows, request.cols)?;
                (
                    envelope::Payload::SessionCreateResponse(SessionCreateResponse {
                        session: Some(SessionSummary::from(session)),
                        detail: String::new(),
                        failure_code: SessionFailureCode::None as i32,
                    }),
                    None,
                )
            }
            Some(envelope::Payload::SessionCloseRequest(request)) => {
                let session_id = LocalSessionRegistry::parse_session_id(&request.session_id)?;
                self.registry.close(session_id)?;
                (
                    envelope::Payload::SessionCloseResponse(SessionCloseResponse {
                        session_id: request.session_id,
                    }),
                    None,
                )
            }
            Some(envelope::Payload::LogPageRequest(request)) => {
                let session_id = request.session_id.clone();
                let page = self.registry.fulfill_log_page_request(&request)?;
                let rows = page
                    .rows
                    .into_iter()
                    .map(|row| LogRow {
                        line_id: row.line_id,
                        text: row.text,
                        style_spans: row
                            .style_spans
                            .into_iter()
                            .map(|span| LogStyleSpan {
                                start: span.start,
                                end: span.end,
                                style: Some(TerminalStyle::from(terminal_style(span.style))),
                            })
                            .collect(),
                        truncated: row.truncated,
                    })
                    .collect();
                let response = LogPage {
                    session_id,
                    revision: page.revision,
                    anchor_line_id: page.anchor_line_id,
                    rows,
                    total_line_count: page.total_line_count,
                    has_before: page.has_before,
                    has_after: page.has_after,
                };
                response.validate()?;
                (envelope::Payload::LogPage(response), None)
            }
            Some(envelope::Payload::HistorySearchRequest(request)) => {
                let session_id = request.session_id.clone();
                let result = self.registry.fulfill_history_search_request(&request)?;
                let next_cursor = result.next_cursor;
                let response = HistorySearchResult {
                    session_id,
                    revision: result.revision,
                    matches: result
                        .matches
                        .into_iter()
                        .map(|matched| HistorySearchMatch {
                            line_id: matched.line_id,
                            byte_start: matched.byte_start as u32,
                            byte_end: matched.byte_end as u32,
                        })
                        .collect(),
                    scanned_lines: result.scanned_lines as u32,
                    next_line_id: next_cursor.map(|cursor| cursor.line_id),
                    next_byte_offset: next_cursor.map(|cursor| cursor.byte_offset as u32),
                    incomplete: result.incomplete,
                };
                response.validate()?;
                (envelope::Payload::HistorySearchResult(response), None)
            }
            Some(envelope::Payload::TerminalInputRequest(request)) => (
                envelope::Payload::TerminalControlResponse(self.dispatch_terminal_input(request)),
                None,
            ),
            Some(envelope::Payload::TerminalResizeRequest(request)) => (
                envelope::Payload::TerminalControlResponse(self.dispatch_terminal_resize(request)),
                None,
            ),
            _ => return Err(SessionIpcError::UnsupportedRequest),
        };
        Ok(DispatchResult {
            response: Envelope {
                request_id,
                deadline_unix_ms: 0,
                payload: Some(payload),
            },
            subscription,
        })
    }

    fn dispatch_terminal_input(&self, request: TerminalInputRequest) -> TerminalControlResponse {
        let response_session_id = valid_control_session_id(&request.session_id);
        let session_id = match LocalSessionRegistry::parse_session_id(&request.session_id) {
            Ok(session_id) => session_id,
            Err(error) => {
                return TerminalControlResponse::new(
                    response_session_id,
                    registry_control_status(&error),
                );
            }
        };
        let action = match request.decode_action() {
            Ok(action) => action,
            Err(_) => {
                return TerminalControlResponse::new(
                    response_session_id,
                    TerminalControlStatus::InvalidRequest,
                );
            }
        };
        if self.ssh_sessions.contains(session_id) {
            let result = self
                .ssh_sessions
                .get(session_id)
                .map_err(|error| ssh_control_status(&error))
                .and_then(|session| {
                    session
                        .send_input(&action)
                        .map_err(|error| ssh_control_status(&error))
                });
            return TerminalControlResponse::new(
                response_session_id,
                result.err().unwrap_or(TerminalControlStatus::Accepted),
            );
        }
        let result = self
            .registry
            .attach(session_id)
            .map_err(|error| registry_control_status(&error))
            .and_then(|attachment| {
                attachment
                    .send_input(&action)
                    .map_err(|error| local_control_status(&error))
            });
        TerminalControlResponse::new(
            response_session_id,
            result.err().unwrap_or(TerminalControlStatus::Accepted),
        )
    }

    fn dispatch_terminal_resize(&self, request: TerminalResizeRequest) -> TerminalControlResponse {
        let response_session_id = valid_control_session_id(&request.session_id);
        let session_id = match LocalSessionRegistry::parse_session_id(&request.session_id) {
            Ok(session_id) => session_id,
            Err(error) => {
                return TerminalControlResponse::new(
                    response_session_id,
                    registry_control_status(&error),
                );
            }
        };
        let size = match request.decode_size() {
            Ok(size) => size,
            Err(_) => {
                return TerminalControlResponse::new(
                    response_session_id,
                    TerminalControlStatus::InvalidRequest,
                );
            }
        };
        if self.ssh_sessions.contains(session_id) {
            let result = self
                .ssh_sessions
                .get(session_id)
                .map_err(|error| ssh_control_status(&error))
                .and_then(|session| {
                    session
                        .resize(size)
                        .map_err(|error| ssh_control_status(&error))
                });
            return TerminalControlResponse::new(
                response_session_id,
                result.err().unwrap_or(TerminalControlStatus::Accepted),
            );
        }
        let result = self
            .registry
            .attach(session_id)
            .map_err(|error| registry_control_status(&error))
            .and_then(|attachment| {
                attachment
                    .resize(size)
                    .map_err(|error| local_control_status(&error))
            });
        TerminalControlResponse::new(
            response_session_id,
            result.err().unwrap_or(TerminalControlStatus::Accepted),
        )
    }
}

pub(crate) async fn read_ssh_auth_file(
    path: &str,
    label: &str,
) -> Result<Zeroizing<String>, String> {
    const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;
    let path = std::path::Path::new(path);
    if !path.is_absolute() {
        return Err(format!("{label} reference must be an absolute path"));
    }
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| format!("{label} file is unavailable"))?;
    if !metadata.is_file() || metadata.len() > MAX_AUTH_FILE_BYTES {
        return Err(format!("{label} must be a file no larger than 1 MiB"));
    }
    let bytes = Zeroizing::new(
        tokio::fs::read(path)
            .await
            .map_err(|_| format!("{label} file cannot be read"))?,
    );
    if bytes.len() as u64 > MAX_AUTH_FILE_BYTES {
        return Err(format!("{label} file exceeds 1 MiB"));
    }
    String::from_utf8(bytes.to_vec())
        .map(Zeroizing::new)
        .map_err(|_| format!("{label} file is not UTF-8"))
}

pub(crate) fn ssh_launch_failure(error: SshSessionError) -> SshLaunchFailure {
    let (code, detail) = match error {
        SshSessionError::Ssh(SshError::ProxyRejected(detail)) => {
            (SessionFailureCode::ProxyRejected, detail)
        }
        SshSessionError::Ssh(SshError::HostKeyRejected(HostKeyCheck::Unknown)) => (
            SessionFailureCode::HostKeyUnknown,
            "Unknown SSH host key. Preview and confirm its fingerprint in the Profile editor.",
        ),
        SshSessionError::Ssh(SshError::HostKeyRejected(HostKeyCheck::Changed)) => (
            SessionFailureCode::HostKeyChanged,
            "SSH host key changed. Verify the change with the server administrator.",
        ),
        SshSessionError::Ssh(SshError::HostKeyRejected(HostKeyCheck::Revoked)) => (
            SessionFailureCode::HostKeyRevoked,
            "SSH host key is revoked. Contact the server administrator.",
        ),
        SshSessionError::Ssh(SshError::AuthenticationRejected) => (
            SessionFailureCode::AuthenticationRejected,
            "SSH authentication rejected. Check the saved username and authentication settings.",
        ),
        SshSessionError::Ssh(error) if error.is_transport_failure() => (
            SessionFailureCode::NetworkUnavailable,
            "SSH network connection failed or timed out. Check the host, port and network, then retry manually.",
        ),
        SshSessionError::Ssh(
            SshError::Agent(_)
            | SshError::AgentAuthentication(_)
            | SshError::AgentIdentityUnavailable
            | SshError::AgentBackendUnsupported { .. }
            | SshError::AgentBackendsUnavailable(_)
            | SshError::PrivateKey(_)
            | SshError::Certificate(_)
            | SshError::CertificateKeyMismatch,
        ) => (
            SessionFailureCode::CredentialUnavailable,
            "SSH key, certificate or agent credential is unavailable. Check the Profile authentication settings.",
        ),
        SshSessionError::Pipeline(_) => (
            SessionFailureCode::StorageUnavailable,
            "SSH terminal output storage could not be opened.",
        ),
        _ => (
            SessionFailureCode::ProtocolRejected,
            "SSH negotiation or terminal request failed. Check server SSH compatibility and shell access.",
        ),
    };
    SshLaunchFailure::new(code, detail)
}

pub(crate) fn default_known_hosts_path() -> Result<std::path::PathBuf, String> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    let home = home.ok_or("cannot locate the user home directory for known_hosts")?;
    Ok(std::path::PathBuf::from(home)
        .join(".ssh")
        .join("known_hosts"))
}

fn valid_control_session_id(session_id: &[u8]) -> Vec<u8> {
    if session_id.len() == 16 {
        session_id.to_vec()
    } else {
        Vec::new()
    }
}

fn registry_control_status(error: &SessionRegistryError) -> TerminalControlStatus {
    match error {
        SessionRegistryError::InvalidSessionIdLength(_) => TerminalControlStatus::InvalidRequest,
        SessionRegistryError::UnknownSession(_) => TerminalControlStatus::UnknownSession,
        SessionRegistryError::Session(error) => local_control_status(error),
        _ => TerminalControlStatus::Failed,
    }
}

fn ssh_control_status(error: &SshSessionError) -> TerminalControlStatus {
    match error {
        SshSessionError::Backpressure => TerminalControlStatus::Backpressure,
        SshSessionError::Closed => TerminalControlStatus::SessionClosed,
        SshSessionError::Unknown(_) => TerminalControlStatus::UnknownSession,
        _ => TerminalControlStatus::Failed,
    }
}

fn local_control_status(error: &LocalSessionError) -> TerminalControlStatus {
    match error {
        LocalSessionError::InputBackpressure => TerminalControlStatus::Backpressure,
        LocalSessionError::Closed => TerminalControlStatus::SessionClosed,
        _ => TerminalControlStatus::Failed,
    }
}

fn terminal_style(style: JournalStyle) -> Style {
    Style {
        foreground: terminal_color(style.foreground),
        background: terminal_color(style.background),
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        inverse: style.inverse,
    }
}

fn terminal_color(color: JournalColor) -> Color {
    match color {
        JournalColor::Default => Color::Default,
        JournalColor::Indexed(value) => Color::Indexed(value),
        JournalColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

impl From<LocalSessionInfo> for SessionSummary {
    fn from(info: LocalSessionInfo) -> Self {
        Self {
            session_id: info.session_id.as_uuid().as_bytes().to_vec(),
            title: info.title,
            running: info.running,
            generation: info.generation,
            profile_id: info.profile_id.map(|id| id.as_uuid().as_bytes().to_vec()),
            terminal_detail: if info.running {
                info.launch_detail.unwrap_or_default()
            } else {
                "Local terminal exited".into()
            },
        }
    }
}

fn is_client_disconnect(error: &IpcError) -> bool {
    matches!(
        error,
        IpcError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            )
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{SessionIpcService, terminal_style};
    use crate::LocalSessionRegistry;
    use cshell_domain::{InputAction, TerminalSize};
    use cshell_ipc::{
        Envelope, HistorySearchDirection, HistorySearchRequest, LogPageRequest, SessionListRequest,
        SnapshotRequest, TerminalControlStatus, TerminalInputRequest, TerminalResizeRequest,
        envelope, read_envelope, write_envelope,
    };
    use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
    use cshell_output_store::{JournalColor, JournalStyle};
    use cshell_terminal::{Color, Style};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn long_running_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "IPC snapshot probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "ping -n 30 127.0.0.1 >nul".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "IPC snapshot probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-lc".to_owned(), "sleep 30".to_owned()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    fn interactive_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "IPC interactive input probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec!["/D".to_owned(), "/Q".to_owned()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "IPC interactive input probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec![],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn journal_style_maps_losslessly_to_terminal_style() {
        assert_eq!(
            terminal_style(JournalStyle {
                foreground: JournalColor::Rgb(1, 2, 3),
                background: JournalColor::Indexed(237),
                bold: true,
                italic: true,
                underline: true,
                inverse: true,
            }),
            Style {
                foreground: Color::Rgb(1, 2, 3),
                background: Color::Indexed(237),
                bold: true,
                italic: true,
                underline: true,
                inverse: true,
            }
        );
    }

    #[test]
    fn control_plane_creates_lists_and_closes_a_local_session() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let service = SessionIpcService::new(Arc::clone(&registry));

        let list = service
            .handle_request(Envelope {
                request_id: 1,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionListRequest(
                    cshell_ipc::SessionListRequest {},
                )),
            })
            .unwrap();
        let Some(envelope::Payload::SessionListResponse(list)) = list.payload else {
            panic!("expected session list");
        };
        assert!(list.sessions.is_empty());

        let created = service
            .handle_request(Envelope {
                request_id: 2,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionCreateRequest(
                    cshell_ipc::SessionCreateRequest {
                        local_launch: None,
                        rows: 24,
                        cols: 80,
                        profile_id: None,
                    },
                )),
            })
            .unwrap();
        let Some(envelope::Payload::SessionCreateResponse(created)) = created.payload else {
            panic!("expected create response");
        };
        let session = created.session.unwrap();
        assert_eq!(session.session_id.len(), 16);
        assert_eq!(registry.len(), 1);

        let closed = service
            .handle_request(Envelope {
                request_id: 3,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionCloseRequest(
                    cshell_ipc::SessionCloseRequest {
                        apply_view_policy: false,
                        session_id: session.session_id.clone(),
                    },
                )),
            })
            .unwrap();
        assert!(matches!(
            closed.payload,
            Some(envelope::Payload::SessionCloseResponse(response))
                if response.session_id == session.session_id
        ));
        assert!(registry.is_empty());
    }

    #[test]
    fn terminal_input_and_resize_are_dispatched_to_the_daemon_owned_session() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&long_running_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);
        let service = SessionIpcService::new(Arc::clone(&registry));

        let input =
            TerminalInputRequest::from_action(session_id, &InputAction::Text(String::new()))
                .unwrap();
        let response = service
            .handle_request(Envelope {
                request_id: 10,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::TerminalInputRequest(input)),
            })
            .unwrap();
        let Some(envelope::Payload::TerminalControlResponse(response)) = response.payload else {
            panic!("terminal input must return a control response");
        };
        assert_eq!(
            response.decoded_status().unwrap(),
            TerminalControlStatus::Accepted
        );

        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1_000,
            pixel_height: 600,
            generation: 1,
        };
        let response = service
            .handle_request(Envelope {
                request_id: 11,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::TerminalResizeRequest(
                    TerminalResizeRequest::from_size(session_id, size),
                )),
            })
            .unwrap();
        let Some(envelope::Payload::TerminalControlResponse(response)) = response.payload else {
            panic!("terminal resize must return a control response");
        };
        assert_eq!(
            response.decoded_status().unwrap(),
            TerminalControlStatus::Accepted
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let snapshot = loop {
            let frame = registry.attach(session_id).unwrap().full_frame().unwrap();
            let snapshot = frame.decode_terminal_snapshot().unwrap();
            if (snapshot.rows, snapshot.cols) == (30, 100) {
                break snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "resize was not published before the deadline"
            );
            std::thread::yield_now();
        };
        assert_eq!((snapshot.rows, snapshot.cols), (30, 100));
    }

    #[test]
    fn terminal_control_failures_return_status_without_failing_the_ipc_dispatcher() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let service = SessionIpcService::new(registry);
        let missing_session = cshell_domain::SessionId::new();

        let malformed = service
            .handle_request(Envelope {
                request_id: 20,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::TerminalInputRequest(
                    TerminalInputRequest {
                        session_id: missing_session.as_uuid().as_bytes().to_vec(),
                        key: None,
                        text: None,
                        paste: None,
                        control: None,
                    },
                )),
            })
            .unwrap();
        let Some(envelope::Payload::TerminalControlResponse(malformed)) = malformed.payload else {
            panic!("malformed input must receive a response");
        };
        assert_eq!(
            malformed.decoded_status().unwrap(),
            TerminalControlStatus::InvalidRequest
        );

        let unknown = TerminalInputRequest::from_action(
            missing_session,
            &InputAction::Text("ignored".to_owned()),
        )
        .unwrap();
        let unknown = service
            .handle_request(Envelope {
                request_id: 21,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::TerminalInputRequest(unknown)),
            })
            .unwrap();
        let Some(envelope::Payload::TerminalControlResponse(unknown)) = unknown.payload else {
            panic!("unknown session must receive a response");
        };
        assert_eq!(
            unknown.decoded_status().unwrap(),
            TerminalControlStatus::UnknownSession
        );
    }

    #[test]
    fn typed_ipc_input_executes_in_the_real_pty_and_reaches_the_journal() {
        const MARKER: &str = "CSHELL_CONTROL_EXECUTED";
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&interactive_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);
        let service = SessionIpcService::new(Arc::clone(&registry));

        let input = TerminalInputRequest::from_action(
            session_id,
            &InputAction::Text(format!("echo {MARKER}\r")),
        )
        .unwrap();
        let response = service
            .handle_request(Envelope {
                request_id: 30,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::TerminalInputRequest(input)),
            })
            .unwrap();
        let Some(envelope::Payload::TerminalControlResponse(response)) = response.payload else {
            panic!("interactive input must return a control response");
        };
        assert_eq!(
            response.decoded_status().unwrap(),
            TerminalControlStatus::Accepted
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let page = registry
                .fulfill_log_page_request(&LogPageRequest {
                    session_id: session_id.as_uuid().as_bytes().to_vec(),
                    anchor_line_id: None,
                    cell_offset: 0,
                    rows_before: 128,
                    rows_after: 0,
                })
                .unwrap();
            if page
                .rows
                .iter()
                .any(|row| row.text.contains(MARKER) && !row.text.contains("echo "))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "typed input did not produce an executed-command output line; journal rows: {:?}",
                page.rows
                    .iter()
                    .map(|row| row.text.as_str())
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        registry.close(session_id).unwrap();
    }

    fn streaming_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "IPC streaming probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "echo CSHELL_STREAM_ONE & ping -n 2 127.0.0.1 >nul & echo CSHELL_STREAM_TWO"
                    .to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "IPC streaming probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-lc".to_owned(),
                "printf CSHELL_STREAM_ONE; sleep 1; printf CSHELL_STREAM_TWO".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    fn styled_streaming_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "IPC styled log probe".to_owned(),
            program: PathBuf::from("powershell.exe"),
            args: vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                "[Console]::Write(([char]27).ToString() + '[38;5;196mCSHELL_STYLED' + ([char]27) + '[0m' + [Environment]::NewLine); Start-Sleep -Seconds 2".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "IPC styled log probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-lc".to_owned(),
                "printf '\\033[38;5;196mCSHELL_STYLED\\033[0m\\n'; sleep 2".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn snapshot_request_crosses_framed_ipc_and_returns_typed_full_frame() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&long_running_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);

        let service = SessionIpcService::new(Arc::clone(&registry));
        let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
        let server_task = tokio::spawn(async move { service.serve_one(&mut server).await });
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 73,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SnapshotRequest(SnapshotRequest {
                    session_id: session_id.as_uuid().as_bytes().to_vec(),
                    current_generation: None,
                })),
            },
        )
        .await
        .unwrap();

        let response = read_envelope(&mut client).await.unwrap();
        assert_eq!(response.request_id, 73);
        let Some(envelope::Payload::FullFrame(frame)) = response.payload else {
            panic!("expected a full terminal frame");
        };
        assert_eq!(frame.session_id, session_id.as_uuid().as_bytes());
        let snapshot = frame.decode_terminal_snapshot().unwrap();
        assert_eq!((snapshot.rows, snapshot.cols), (24, 80));
        server_task.await.unwrap().unwrap();

        registry.close(session_id).unwrap();
    }

    #[tokio::test]
    async fn rejected_terminal_control_keeps_the_framed_connection_usable() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let service = SessionIpcService::new(Arc::clone(&registry));
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let server_task = tokio::spawn(async move { service.serve_connection(server).await });

        write_envelope(
            &mut client,
            &Envelope {
                request_id: 80,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionListRequest(SessionListRequest {})),
            },
        )
        .await
        .unwrap();
        let initial = read_envelope(&mut client).await.unwrap();
        assert!(matches!(
            initial,
            Envelope {
                request_id: 80,
                payload: Some(envelope::Payload::SessionListResponse(_)),
                ..
            }
        ));

        write_envelope(
            &mut client,
            &Envelope {
                request_id: 81,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::TerminalInputRequest(
                    TerminalInputRequest {
                        session_id: vec![0; 16],
                        key: None,
                        text: None,
                        paste: None,
                        control: None,
                    },
                )),
            },
        )
        .await
        .unwrap();
        let rejected = read_envelope(&mut client).await.unwrap();
        let Some(envelope::Payload::TerminalControlResponse(rejected)) = rejected.payload else {
            panic!("invalid terminal control must receive a typed response");
        };
        assert_eq!(
            rejected.decoded_status().unwrap(),
            TerminalControlStatus::InvalidRequest
        );

        write_envelope(
            &mut client,
            &Envelope {
                request_id: 82,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionListRequest(SessionListRequest {})),
            },
        )
        .await
        .unwrap();
        let after_rejection = read_envelope(&mut client).await.unwrap();
        assert!(matches!(
            after_rejection,
            Envelope {
                request_id: 82,
                payload: Some(envelope::Payload::SessionListResponse(_)),
                ..
            }
        ));

        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn connection_streams_merged_frames_until_the_client_disconnects() {
        use cshell_ipc::{ApplyResult, TerminalReplicaState};

        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&streaming_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);

        let service = SessionIpcService::new(Arc::clone(&registry));
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let server_task = tokio::spawn(async move { service.serve_connection(server).await });
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 91,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SnapshotRequest(SnapshotRequest {
                    session_id: session_id.as_uuid().as_bytes().to_vec(),
                    current_generation: None,
                })),
            },
        )
        .await
        .unwrap();

        let initial = read_envelope(&mut client).await.unwrap();
        let Some(envelope::Payload::FullFrame(initial)) = initial.payload else {
            panic!("subscription must begin with a full frame");
        };
        let mut replica = TerminalReplicaState::new(session_id.as_uuid().as_bytes().to_vec());
        assert_eq!(replica.apply_full(initial).unwrap(), ApplyResult::Applied);

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let envelope = tokio::time::timeout(remaining, read_envelope(&mut client))
                .await
                .unwrap()
                .unwrap();
            match envelope.payload {
                Some(envelope::Payload::FrameDelta(delta)) => {
                    assert_eq!(replica.apply_delta(delta).unwrap(), ApplyResult::Applied);
                }
                Some(envelope::Payload::FullFrame(frame)) => {
                    assert_eq!(replica.apply_full(frame).unwrap(), ApplyResult::Applied);
                }
                _ => panic!("unexpected subscription payload"),
            }
            let visible: String = replica
                .snapshot()
                .unwrap()
                .cells
                .iter()
                .map(|cell| cell.character)
                .collect();
            if visible.contains("CSHELL_STREAM_ONE") {
                break;
            }
        }

        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        registry.close(session_id).unwrap();
    }

    #[test]
    fn log_page_request_crosses_registry_index_and_ipc_schema() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&streaming_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);
        let service = SessionIpcService::new(Arc::clone(&registry));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let response = service
                .handle_request(Envelope {
                    request_id: 101,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::LogPageRequest(LogPageRequest {
                        session_id: session_id.as_uuid().as_bytes().to_vec(),
                        anchor_line_id: None,
                        cell_offset: 0,
                        rows_before: 32,
                        rows_after: 0,
                    })),
                })
                .unwrap();
            let Some(envelope::Payload::LogPage(page)) = response.payload else {
                panic!("expected an indexed log page");
            };
            page.validate().unwrap();
            if page
                .rows
                .iter()
                .any(|row| row.text.contains("CSHELL_STREAM_ONE"))
            {
                assert_eq!(response.request_id, 101);
                assert!(page.revision > 0);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "journal index did not observe PTY output"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        registry.close(session_id).unwrap();
    }

    #[test]
    fn history_search_crosses_registry_index_and_ipc_with_bounded_results() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&streaming_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);
        let service = SessionIpcService::new(Arc::clone(&registry));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let response = service
                .handle_request(Envelope {
                    request_id: 103,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::HistorySearchRequest(
                        HistorySearchRequest {
                            session_id: session_id.as_uuid().as_bytes().to_vec(),
                            query: "stream_one".to_owned(),
                            case_sensitive: false,
                            whole_word: false,
                            regex: false,
                            direction: HistorySearchDirection::Forward as i32,
                            cursor_line_id: None,
                            cursor_byte_offset: None,
                            max_scan_lines: 32,
                            max_matches: 1,
                        },
                    )),
                })
                .unwrap();
            let Some(envelope::Payload::HistorySearchResult(result)) = response.payload else {
                panic!("expected a bounded history search result");
            };
            result.validate().unwrap();
            assert!(result.scanned_lines <= 32);
            assert!(result.matches.len() <= 1);
            if !result.matches.is_empty() {
                assert_eq!(response.request_id, 103);
                assert!(result.revision > 0);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "history search did not observe PTY output"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        registry.close(session_id).unwrap();
    }

    #[test]
    fn styled_log_page_crosses_pty_journal_registry_and_ipc() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&styled_streaming_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);
        let service = SessionIpcService::new(Arc::clone(&registry));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let response = service
                .handle_request(Envelope {
                    request_id: 102,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::LogPageRequest(LogPageRequest {
                        session_id: session_id.as_uuid().as_bytes().to_vec(),
                        anchor_line_id: None,
                        cell_offset: 0,
                        rows_before: 8,
                        rows_after: 0,
                    })),
                })
                .unwrap();
            let Some(envelope::Payload::LogPage(page)) = response.payload else {
                panic!("expected a styled indexed log page");
            };
            page.validate().unwrap();
            if let Some(row) = page
                .rows
                .iter()
                .find(|row| row.text.contains("CSHELL_STYLED"))
            {
                let style = Style::try_from(row.style_spans[0].style.clone().unwrap()).unwrap();
                assert_eq!(style.foreground, Color::Indexed(196));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "styled journal output did not reach IPC"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        registry.close(session_id).unwrap();
    }
}
