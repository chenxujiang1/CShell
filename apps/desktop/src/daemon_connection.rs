use crate::daemon_input::{DesktopInputSecurity, DesktopInputSender, QueuedDesktopInput};
use cshell_domain::{ConnectionId, InputAction, ProfileId, SessionId, TerminalSize};
use cshell_ipc::{
    ClientFrameUpdate, DiscoveryRecord, Envelope, Handshake, HistorySearchCodecError,
    HistorySearchDirection, HistorySearchRequest as IpcHistorySearchRequest,
    HistorySearchResult as IpcHistorySearchResult, LogPage as IpcLogPage, LogPageCodecError,
    LogPageRequest as IpcLogPageRequest, RuntimePaths, SessionFailureCode, SnapshotCodecError,
    SubscriptionClientError, TerminalControlCodecError, TerminalControlResponse,
    TerminalControlStatus, TerminalInputRequest, TerminalResizeRequest,
    TerminalSubscriptionReplica, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use cshell_render::{
    LogPage as RenderLogPage, LogPageRequest as RenderLogPageRequest, LogRow as RenderLogRow,
    LogSourceId, LogStyleSpan as RenderLogStyleSpan,
};
use cshell_terminal::{FrameSnapshot, Style};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
mod desktop_lifetime;
pub(crate) use desktop_lifetime::DesktopDaemonLifetime;

#[derive(Clone, Debug)]
pub struct DesktopConnectionConfig {
    source: DesktopEndpointSource,
    session_id: Option<SessionId>,
    request_log_pages: bool,
}

#[derive(Clone, Debug)]
enum DesktopEndpointSource {
    Explicit(ResolvedDesktopEndpoint),
    Discovery(RuntimePaths),
}

#[derive(Clone)]
pub(crate) struct ResolvedDesktopEndpoint {
    #[cfg(windows)]
    pub(crate) endpoint: String,
    #[cfg(unix)]
    pub(crate) endpoint: PathBuf,
    pub(crate) instance_token: [u8; 32],
    pub(crate) daemon_instance_id: [u8; 16],
}
impl std::fmt::Debug for ResolvedDesktopEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedDesktopEndpoint")
            .field("daemon_instance_id", &self.daemon_instance_id)
            .field("instance_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DesktopDaemonView {
    pub(crate) connection_trace:Option<crate::connection_diagnostics::View>,
    pub(crate) connection_attempt:u64,
    pub(crate) input_epoch: ConnectionId,
    pub(crate) input_security: DesktopInputSecurity,
    pub(crate) compose_supported: bool,
    pub connected: bool,
    pub detail: String,
    pub snapshot: Option<Arc<FrameSnapshot>>,
    pub session_id: Option<SessionId>,
    pub session_title: Option<String>,
    pub log_page: Option<Arc<RenderLogPage>>,
    pub log_page_terminal_history: bool,
    pub(crate) log_page_epoch: u64,
    pub control_error: Option<String>,
    pub launch_error: Option<String>,
    pub launch_cancelled: bool,
    pub session_profile_id: Option<ProfileId>,
    pub session_running: bool,
    pub session_reconnecting: bool,
    pub session_opening: bool,
    pub terminal_detail: String,
    pub history_search: Option<Arc<DesktopHistorySearchResponse>>,
    pub sessions: Option<Vec<cshell_ipc::SessionSummary>>,
    pub closed_session: Option<SessionId>,
    pub close_pending: Option<SessionId>,
    pub close_error: Option<String>,
}

#[derive(Debug)]
pub struct DesktopDaemonConnection {
    shared: Arc<Mutex<DesktopDaemonView>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    log_requests: tokio::sync::watch::Sender<Option<DesktopLogPageRequest>>,
    history_search_requests: tokio::sync::watch::Sender<Option<DesktopHistorySearchRequest>>,
    input: DesktopInputSender,
    resize_requests: tokio::sync::watch::Sender<Option<TerminalSize>>,
    open_requests: tokio::sync::watch::Sender<Option<DesktopSessionAction>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, PartialEq)]
pub(crate) enum DesktopSessionAction {
    OpenProfile(ProfileId),
    DuplicateSshChannel(SessionId, Option<ProfileId>),
    OpenLocal(ProfileId, Arc<cshell_ipc::LocalLaunchOptions>),
    OpenContext(ProfileId, Arc<cshell_application::TerminalResolutionReport>),
    CloseView(SessionId, bool),
    NewShell(ProfileId),
    Attach(SessionId),
    Idle,
    NewDefault,
}

impl std::fmt::Debug for DesktopSessionAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::OpenProfile(_) => "OpenProfile",
            Self::DuplicateSshChannel(..) => "DuplicateSshChannel",
            Self::OpenLocal(..) => "OpenLocal([REDACTED])",
            Self::OpenContext(..) => "OpenContext([REDACTED])",
            Self::NewShell(_) => "NewShell",
            Self::Attach(_) => "Attach",
            Self::CloseView(..) => "CloseView",
            Self::Idle => "Idle",
            Self::NewDefault => "NewDefault",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DesktopLogPageRequest {
    epoch: u64,
    terminal_history: bool,
    anchor_line_id: Option<u64>,
    cell_offset: u32,
    rows_before: u16,
    rows_after: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopHistorySearchRequest {
    pub revision: u64,
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub direction: HistorySearchDirection,
    pub cursor: Option<(u64, u32)>,
}

#[derive(Clone, Debug)]
pub struct DesktopHistorySearchResponse {
    pub revision: u64,
    pub result: IpcHistorySearchResult,
}

struct DesktopWorkerReceivers {
    log_pages: tokio::sync::watch::Receiver<Option<DesktopLogPageRequest>>,
    history_search: tokio::sync::watch::Receiver<Option<DesktopHistorySearchRequest>>,
    input: tokio::sync::mpsc::Receiver<QueuedDesktopInput>,
    resize: tokio::sync::watch::Receiver<Option<TerminalSize>>,
    open: tokio::sync::watch::Receiver<Option<DesktopSessionAction>>,
}

struct DesktopRequestReceivers<'a> {
    log_pages: &'a mut tokio::sync::watch::Receiver<Option<DesktopLogPageRequest>>,
    history_search: &'a mut tokio::sync::watch::Receiver<Option<DesktopHistorySearchRequest>>,
    input: &'a mut tokio::sync::mpsc::Receiver<QueuedDesktopInput>,
    resize: &'a mut tokio::sync::watch::Receiver<Option<TerminalSize>>,
    open: &'a mut tokio::sync::watch::Receiver<Option<DesktopSessionAction>>,
}

#[derive(Debug, Error)]
pub enum DesktopConnectionError {
    #[error("{0} is required when CSHELL_IPC_ENDPOINT is configured")]
    MissingEnvironment(&'static str),
    #[error("{name} must contain exactly {expected} hexadecimal bytes")]
    InvalidHex { name: &'static str, expected: usize },
    #[error("cannot start desktop daemon client thread: {0}")]
    Thread(std::io::Error),
    #[error("cannot connect to daemon IPC endpoint: {0}")]
    Connect(std::io::Error),
    #[error("daemon IPC handshake failed: {0}")]
    Handshake(#[from] cshell_ipc::HandshakeProtocolError),
    #[error("后台 IPC 连接或握手超时")]
    IpcSetupTimeout,
    #[error("daemon IPC transport failed: {0}")]
    Ipc(#[from] cshell_ipc::IpcError),
    #[error("terminal subscription failed: {0}")]
    Subscription(#[from] SubscriptionClientError),
    #[error("daemon returned an unexpected session control response")]
    UnexpectedControlResponse,
    #[error("switching to saved Profile")]
    SwitchSession(DesktopSessionAction),
    #[error("View closed; reopen the session from the active list if it was kept running")]
    ViewClosed,
    #[error("Close request outcome is unknown; inspect the active session list before retrying")]
    SessionCloseUncertain,
    #[error("saved Profile could not start: {0}")]
    ProfileLaunch(String),
    #[error("已取消连接")]
    LaunchCancelled,
    #[error("previous session is unavailable; reconnect explicitly to open a New Shell")]
    SessionUnavailable,
    #[error(
        "Profile open request outcome is unknown; inspect existing sessions before opening a New Shell"
    )]
    SessionCreationUncertain,
    #[error("daemon did not negotiate saved Profile sessions")]
    ProfileSessionNotNegotiated,
    #[error("daemon returned an invalid session identifier")]
    InvalidSessionId,
    #[error("daemon did not negotiate bounded log paging")]
    LogPagingNotNegotiated,
    #[error("daemon did not negotiate bounded history search")]
    HistorySearchNotNegotiated,
    #[error("daemon did not negotiate terminal input and resize control")]
    TerminalControlNotNegotiated,
    #[error("terminal control request is invalid: {0}")]
    InvalidTerminalControl(#[from] TerminalControlCodecError),
    #[error("daemon returned an invalid log page: {0}")]
    InvalidLogPage(#[from] LogPageCodecError),
    #[error("daemon returned an invalid history search result: {0}")]
    InvalidHistorySearch(#[from] HistorySearchCodecError),
    #[error("daemon returned an invalid log style: {0}")]
    InvalidLogStyle(#[from] SnapshotCodecError),
    #[error("cannot discover the per-user daemon: {0}")]
    Discovery(#[from] cshell_ipc::DiscoveryError),
    #[error("cannot locate the desktop executable while starting cshelld: {0}")]
    CurrentExecutable(std::io::Error),
    #[error("cannot start companion cshelld process: {0}")]
    StartDaemon(std::io::Error),
}

impl DesktopConnectionConfig {
    #[cfg(test)]
    pub(crate) fn for_fixture(endpoint: ResolvedDesktopEndpoint) -> Self {
        Self {
            source: DesktopEndpointSource::Explicit(endpoint),
            session_id: None,
            request_log_pages: false,
        }
    }

    pub fn from_env() -> Result<Option<Self>, DesktopConnectionError> {
        if std::env::var_os("CSHELL_DISABLE_DAEMON").is_some_and(|value| value == "1") {
            return Ok(None);
        }
        let session_id = std::env::var("CSHELL_SESSION_ID_HEX")
            .ok()
            .map(|value| {
                decode_hex::<16>(&value).map(SessionId::from_bytes).ok_or(
                    DesktopConnectionError::InvalidHex {
                        name: "CSHELL_SESSION_ID_HEX",
                        expected: 16,
                    },
                )
            })
            .transpose()?;
        let source = if let Some(endpoint) = std::env::var_os("CSHELL_IPC_ENDPOINT") {
            DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint {
                #[cfg(windows)]
                endpoint: endpoint.to_string_lossy().into_owned(),
                #[cfg(unix)]
                endpoint: PathBuf::from(endpoint),
                instance_token: required_hex::<32>("CSHELL_INSTANCE_TOKEN_HEX")?,
                daemon_instance_id: required_hex::<16>("CSHELL_DAEMON_INSTANCE_ID_HEX")?,
            })
        } else {
            DesktopEndpointSource::Discovery(RuntimePaths::for_current_user()?)
        };
        Ok(Some(Self {
            source,
            session_id,
            request_log_pages: false,
        }))
    }

    #[must_use]
    pub fn with_log_pages(mut self, request_log_pages: bool) -> Self {
        self.request_log_pages = request_log_pages;
        self
    }

    pub(crate) fn resolve(&self) -> Result<ResolvedDesktopEndpoint, DesktopConnectionError> {
        match &self.source {
            DesktopEndpointSource::Explicit(endpoint) => Ok(endpoint.clone()),
            DesktopEndpointSource::Discovery(paths) => {
                let record = DiscoveryRecord::load(paths)?;
                Ok(ResolvedDesktopEndpoint {
                    #[cfg(windows)]
                    endpoint: record.endpoint.to_string_lossy().into_owned(),
                    #[cfg(unix)]
                    endpoint: PathBuf::from(record.endpoint.clone()),
                    instance_token: record.instance_token,
                    daemon_instance_id: record.daemon_instance_id,
                })
            }
        }
    }

    fn should_start_daemon_after(&self, error: &DesktopConnectionError) -> bool {
        matches!(&self.source, DesktopEndpointSource::Discovery(_))
            && matches!(
                error,
                DesktopConnectionError::Discovery(_) | DesktopConnectionError::Connect(_)
            )
    }

    pub(crate) fn start_companion_daemon(&self) -> Result<(), DesktopConnectionError> {
        let DesktopEndpointSource::Discovery(paths) = &self.source else {
            return Ok(());
        };
        let executable = std::env::var_os("CSHELL_DAEMON_EXE")
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .map(|desktop| desktop.with_file_name(daemon_executable_name()))
                    .map_err(DesktopConnectionError::CurrentExecutable)
            })?;
        let mut command = std::process::Command::new(executable);
        command
            .env("CSHELL_RUNTIME_DIR", paths.root())
            .env("CSHELL_DESKTOP_MANAGED", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(DesktopConnectionError::StartDaemon)?;
        std::thread::Builder::new()
            .name("cshell-daemon-reaper".to_owned())
            .spawn(move || {
                let _result = child.wait();
            })
            .map_err(DesktopConnectionError::Thread)?;
        Ok(())
    }
}

impl DesktopDaemonConnection {
    #[cfg(test)]
    pub(crate) fn for_view_test(view:DesktopDaemonView)->Self {
        let shared=Arc::new(Mutex::new(view));
        Self{input:DesktopInputSender::new(shared.clone(),tokio::sync::mpsc::channel(1).0),shared,
            shutdown:tokio::sync::watch::channel(false).0,log_requests:tokio::sync::watch::channel(None).0,
            history_search_requests:tokio::sync::watch::channel(None).0,resize_requests:tokio::sync::watch::channel(None).0,
            open_requests:tokio::sync::watch::channel(None).0,worker:None}
    }
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, DesktopConnectionError> {
        Self::start_mode(config, false)
    }
    pub fn start_idle(config: DesktopConnectionConfig) -> Result<Self, DesktopConnectionError> {
        Self::start_mode(config, true)
    }
    fn start_mode(
        config: DesktopConnectionConfig,
        idle: bool,
    ) -> Result<Self, DesktopConnectionError> {
        let shared = Arc::new(Mutex::new(DesktopDaemonView {
            detail: "daemon connecting".to_owned(),
            ..DesktopDaemonView::default()
        }));
        let (shutdown, worker_shutdown) = tokio::sync::watch::channel(false);
        let (log_requests, worker_log_requests) = tokio::sync::watch::channel(None);
        let (history_search_requests, worker_history_search_requests) =
            tokio::sync::watch::channel(None);
        let (input_requests, worker_input_requests) = tokio::sync::mpsc::channel(256);
        let (resize_requests, worker_resize_requests) = tokio::sync::watch::channel(None);
        let (open_requests, worker_open_requests) = tokio::sync::watch::channel(None);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("cshell-desktop-ipc".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(reconnect_loop(
                        config,
                        worker_shared,
                        worker_shutdown,
                        idle,
                        DesktopWorkerReceivers {
                            log_pages: worker_log_requests,
                            history_search: worker_history_search_requests,
                            input: worker_input_requests,
                            resize: worker_resize_requests,
                            open: worker_open_requests,
                        },
                    )),
                    Err(error) => update_view(&worker_shared, false, error.to_string(), None),
                }
            })
            .map_err(DesktopConnectionError::Thread)?;
        Ok(Self {
            input: DesktopInputSender::new(Arc::clone(&shared), input_requests),
            shared,
            shutdown,
            log_requests,
            history_search_requests,
            resize_requests,
            open_requests,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn view(&self) -> DesktopDaemonView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn session_profile(&self, session: SessionId) -> Option<ProfileId> {
        let view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.session_id == Some(session) {
            view.session_profile_id
        } else {
            None
        }
    }

    /// Stop a view without joining its IPC thread on the GUI event thread.
    pub fn retire(mut self) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_input();
        let _sent = self.shutdown.send(true);
        if let Some(worker) = self.worker.take() {
            let _reaper = std::thread::Builder::new()
                .name("cshell-view-reaper".into())
                .spawn(move || {
                    let _joined = worker.join();
                });
        }
    }

    pub(crate) fn duplicate_ssh_channel(&self, source:SessionId, profile:Option<ProfileId>) -> bool {
        let mut view=self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() || view.session_opening {return false;}
        let sent=self.open_requests.send(Some(DesktopSessionAction::DuplicateSshChannel(source,profile))).is_ok();
        if sent {
            view.invalidate_input(); view.session_opening=true; view.session_reconnecting=false;
            view.session_profile_id=profile; view.closed_session=None; view.launch_error=None; view.launch_cancelled=false; view.snapshot=None;
        }
        sent
    }

    pub fn open_profile(&self, id: ProfileId) -> bool {
        self.open_profile_action(id, DesktopSessionAction::OpenProfile(id))
    }

    pub(crate) fn open_terminal_context(
        &self,
        report: Arc<cshell_application::TerminalResolutionReport>,
    ) -> bool {
        self.open_profile_action(
            report.context.profile_id,
            DesktopSessionAction::OpenContext(report.context.profile_id, report),
        )
    }

    pub fn open_local_profile(
        &self,
        id: ProfileId,
        options: cshell_ipc::LocalLaunchOptions,
    ) -> bool {
        self.open_profile_action(id, DesktopSessionAction::OpenLocal(id, Arc::new(options)))
    }

    pub fn close_session_view(&self, id: SessionId) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        let sent = self
            .open_requests
            .send(Some(DesktopSessionAction::CloseView(id, false)))
            .is_ok();
        if sent {
            view.close_pending = Some(id);
            view.close_error = None;
            view.closed_session = None;
            if view.session_id == Some(id) { view.invalidate_input(); }
        }
        sent
    }

    pub fn new_default_shell(&self) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        let sent = self
            .open_requests
            .send(Some(DesktopSessionAction::NewDefault))
            .is_ok();
        if sent {
            view.invalidate_input();
            view.session_opening = true;
            view.connection_attempt=view.connection_attempt.wrapping_add(1);view.connection_trace=None;
            view.session_reconnecting = false;
            view.session_profile_id = None;
            view.closed_session = None;
            view.launch_error = None;
            view.launch_cancelled = false;
            view.snapshot = None;
        }
        sent
    }
    pub fn detach_for_placeholder(&self) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        let sent = self
            .open_requests
            .send(Some(DesktopSessionAction::Idle))
            .is_ok();
        if sent {
            view.invalidate_input();
            view.closed_session = None;
        }
        sent
    }

    #[cfg(test)]
    pub fn close_current_view(&self, terminate: bool) -> bool {
        let view = self.view();
        if !view.connected {
            return false;
        }
        view.session_id.is_some_and(|id| if terminate {
            self.terminate_confirmed_session(id)
        } else {
            self.close_session_view(id)
        })
    }

    pub fn terminate_confirmed_session(&self, id: SessionId) -> bool {
        let view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        if view.session_id != Some(id)
            && !view.sessions.as_ref().is_some_and(|sessions| sessions.iter().any(|session| session.session_id == id.as_uuid().as_bytes()))
        {
            return false;
        }
        drop(view);
        self.terminate_observed_session(id)
    }

    /// The caller captured this exact ID from its workspace or session list.
    /// This also works before a replacement metadata connection's first list.
    pub(crate) fn terminate_observed_session(&self, id: SessionId) -> bool {
        let mut view = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        let sent = self
            .open_requests
            .send(Some(DesktopSessionAction::CloseView(id, true)))
            .is_ok();
        if sent {
            view.close_pending = Some(id);
            view.close_error = None;
            view.closed_session = None;
            if view.session_id == Some(id) { view.invalidate_input(); }
        }
        sent
    }

    fn open_profile_action(&self, id: ProfileId, action: DesktopSessionAction) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        let sent = self.open_requests.send(Some(action)).is_ok();
        if sent {
            view.launch_error = None;
            view.launch_cancelled = false;
            view.closed_session = None;
            view.session_profile_id = Some(id);
            view.session_opening = true;
            view.connection_attempt=view.connection_attempt.wrapping_add(1);view.connection_trace=None;
            view.session_reconnecting = false;
            view.terminal_detail.clear();
            view.invalidate_input();
            view.snapshot = None;
            view.detail = "opening saved Profile".into();
        }
        sent
    }

    pub fn reconnect_new_shell(&self) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        let Some(id) = view
            .session_profile_id
            .filter(|_| !view.connected && !view.session_opening)
        else {
            return false;
        };
        let sent = self
            .open_requests
            .send(Some(DesktopSessionAction::NewShell(id)))
            .is_ok();
        if sent {
            view.invalidate_input();
            view.launch_error = None;
            view.launch_cancelled = false;
            view.closed_session = None;
            view.detail = "Opening a New Shell; the previous process is not restored".into();
            view.session_opening = true;
            view.connection_attempt=view.connection_attempt.wrapping_add(1);view.connection_trace=None;
            view.session_reconnecting = false;
            view.terminal_detail.clear();
        }
        sent
    }

    pub fn attach_session(&self, id: SessionId) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.close_pending.is_some() { return false; }
        let sent = self
            .open_requests
            .send(Some(DesktopSessionAction::Attach(id)))
            .is_ok();
        if sent {
            view.launch_error = None;
            view.launch_cancelled = false;
            view.closed_session = None;
            view.invalidate_input();
            view.snapshot = None;
            view.detail = "switching terminal session".into();
            view.session_opening = true;
            view.connection_attempt=view.connection_attempt.wrapping_add(1);view.connection_trace=None;
            view.session_reconnecting = false;
        }
        sent
    }

    pub fn request_log_page(&self, request: RenderLogPageRequest) -> bool {self.request_page(request,false)}
    pub fn request_terminal_history_page(&self, request:RenderLogPageRequest)->bool {self.request_page(request,true)}
    pub fn cancel_log_page(&self) {
        let mut view = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        view.log_page_epoch = view.log_page_epoch.wrapping_add(1);
        view.log_page = None;
        view.log_page_terminal_history = false;
        drop(view);
        self.log_requests.send_replace(None);
    }
    fn request_page(&self, request:RenderLogPageRequest, terminal_history:bool)->bool {
        let epoch = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).log_page_epoch;
        let request = DesktopLogPageRequest {
            epoch,
            terminal_history,
            anchor_line_id: (!request.follow_tail).then_some(request.anchor_line_id),
            cell_offset: if request.follow_tail {
                0
            } else {
                request.cell_offset
            },
            rows_before: request.rows_before,
            rows_after: request.rows_after,
        };
        self.log_requests.send_if_modified(|current| {
            if *current == Some(request) {
                false
            } else {
                *current = Some(request);
                true
            }
        })
    }

    pub fn request_history_search(&self, request: DesktopHistorySearchRequest) -> bool {
        self.history_search_requests.send_if_modified(|current| {
            if *current == Some(request.clone()) {
                false
            } else {
                *current = Some(request);
                true
            }
        })
    }

    pub fn cancel_history_search(&self) -> bool {
        self.history_search_requests.send_if_modified(|current| {
            if current.is_none() {
                false
            } else {
                *current = None;
                true
            }
        })
    }

    /// Never queues keystrokes while disconnected: replaying stale interactive
    /// input into a later shell would be unsafe.
    pub fn send_input(&self, action: InputAction) -> bool {
        self.input.send_owned(action)
    }

    pub(crate) fn input_sender(&self) -> DesktopInputSender {
        self.input.clone()
    }

    /// Keeps only the latest viewport size so resize storms cannot build a
    /// control backlog behind terminal output.
    pub fn request_resize(
        &self,
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> bool {
        if rows == 0 || cols == 0 {
            return false;
        }
        let cols = cols.min(cshell_terminal::MAX_TERMINAL_COLUMNS);
        let rows = rows.min(cshell_terminal::MAX_TERMINAL_ROWS)
            .min((cshell_terminal::MAX_TERMINAL_CELLS / usize::from(cols)) as u16);
        self.resize_requests.send_if_modified(|current| {
            if current.is_some_and(|size| {
                (size.rows, size.cols, size.pixel_width, size.pixel_height)
                    == (rows, cols, pixel_width, pixel_height)
            }) {
                return false;
            }
            let generation = current.map_or(1, |size| size.generation.saturating_add(1));
            *current = Some(TerminalSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
                generation,
            });
            true
        })
    }
}

impl Drop for DesktopDaemonConnection {
    fn drop(&mut self) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_input();
        let _result = self.shutdown.send(true);
        if let Some(worker) = self.worker.take() {
            let _result = worker.join();
        }
    }
}

async fn reconnect_loop(
    config: DesktopConnectionConfig,
    shared: Arc<Mutex<DesktopDaemonView>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    initially_idle: bool,
    receivers: DesktopWorkerReceivers,
) {
    let DesktopWorkerReceivers {
        log_pages: mut log_requests,
        history_search: mut history_search_requests,
        input: mut input_requests,
        resize: mut resize_requests,
        open: mut open_requests,
    } = receivers;
    let delays = [100_u64, 250, 500, 1_000, 2_000, 5_000];
    let mut attempt = 0_usize;
    let mut last_daemon_start = None;
    let mut log_delivery_revision = 0_u64;
    let mut pending_action = initially_idle.then_some(DesktopSessionAction::Idle);
    while !*shutdown.borrow() {
        update_view(
            &shared,
            false,
            format!("daemon connecting (attempt {})", attempt + 1),
            None,
        );
        let requested_action = pending_action.take();
        let result = connect_once(
            &config,
            &shared,
            &mut shutdown,
            &mut log_delivery_revision,
            requested_action.clone(),
            DesktopRequestReceivers {
                log_pages: &mut log_requests,
                history_search: &mut history_search_requests,
                input: &mut input_requests,
                resize: &mut resize_requests,
                open: &mut open_requests,
            },
        )
        .await;
        if *shutdown.borrow() {
            return;
        }
        if matches!(requested_action, Some(DesktopSessionAction::CloseView(..)))
            && !matches!(result, Err(DesktopConnectionError::ViewClosed))
        {
            let mut view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            view.close_pending = None;
            view.close_error = Some(format!("未能确认会话已关闭：{}。请核对会话列表后重试。",
                result.as_ref().err().map_or_else(|| "后台连接已结束".into(), ToString::to_string)));
            let resume = view.session_id.filter(|current| !matches!(
                requested_action, Some(DesktopSessionAction::CloseView(target, _)) if *current == target
            ));
            drop(view);
            // Restore an unrelated terminal, never replay an uncertain close.
            // Attaching verifies that the process still exists without creating one.
            pending_action = Some(resume.map_or(DesktopSessionAction::Idle, DesktopSessionAction::Attach));
            continue;
        }
        if let Err(DesktopConnectionError::SwitchSession(action)) = &result {
            pending_action = Some(action.clone());
            continue;
        }
        if let Err(DesktopConnectionError::ProfileLaunch(message)) = &result {
            shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .launch_error = Some(message.clone());
        }
        if matches!(&result,Err(DesktopConnectionError::LaunchCancelled)) {
            let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            view.launch_cancelled=true;
            view.launch_error=None;
        }
        if matches!(requested_action, Some(DesktopSessionAction::Idle)) {
            pending_action = requested_action.clone();
        }
        if matches!(
            &result,
            Err(DesktopConnectionError::Connect(_)
                | DesktopConnectionError::Discovery(_)
                | DesktopConnectionError::IpcSetupTimeout
                | DesktopConnectionError::Handshake(_))
        ) {
            pending_action = requested_action;
        }
        if matches!(
            &result,
            Err(DesktopConnectionError::ProfileLaunch(_)
                | DesktopConnectionError::LaunchCancelled
                | DesktopConnectionError::ViewClosed
                | DesktopConnectionError::SessionCloseUncertain
                | DesktopConnectionError::SessionUnavailable
                | DesktopConnectionError::SessionCreationUncertain
                | DesktopConnectionError::ProfileSessionNotNegotiated
                | DesktopConnectionError::TerminalControlNotNegotiated
                | DesktopConnectionError::LogPagingNotNegotiated)
        ) {
            {
                let mut view = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Closing can be acknowledged while the GUI queues an explicit
                // reconnect. Inspect the watch while holding the same lock used
                // by open_profile_action, so we cannot erase its opening state.
                if matches!(&result, Err(DesktopConnectionError::ViewClosed))
                    && open_requests.has_changed().unwrap_or(false)
                {
                    pending_action = open_requests.borrow_and_update().clone();
                    continue;
                }
                view.session_opening = false;
                view.terminal_detail = result
                    .as_ref()
                    .err()
                    .map_or_else(String::new, ToString::to_string);
            }
            update_view(
                &shared,
                false,
                result
                    .as_ref()
                    .err()
                    .map_or_else(String::new, ToString::to_string),
                None,
            );
            tokio::select! {
                changed = open_requests.changed() => {
                    if changed.is_ok() { pending_action = open_requests.borrow_and_update().clone(); }
                }
                _ = shutdown.changed() => { return; }
            }
            continue;
        }
        let was_connected = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connected;
        if was_connected {
            attempt = 0;
        }
        let start_error = result.as_ref().err().and_then(|error| {
            let cooldown_elapsed = last_daemon_start.is_none_or(|started: std::time::Instant| {
                started.elapsed() >= Duration::from_secs(5)
            });
            if config.should_start_daemon_after(error) && cooldown_elapsed {
                last_daemon_start = Some(std::time::Instant::now());
                config.start_companion_daemon().err()
            } else {
                None
            }
        });
        let mut detail = result.map_or_else(
            |error| format!("daemon offline: {error}"),
            |()| "daemon disconnected".to_owned(),
        );
        if let Some(error) = start_error {
            detail.push_str(&format!("; companion launch failed: {error}"));
        }
        update_view(&shared, false, detail, None);
        let delay = delays[attempt.min(delays.len() - 1)];
        attempt = attempt.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
            changed = open_requests.changed() => {
                if changed.is_ok() { pending_action = open_requests.borrow_and_update().clone(); }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

#[cfg(windows)]
fn daemon_executable_name() -> &'static str {
    "cshelld.exe"
}

#[cfg(unix)]
fn daemon_executable_name() -> &'static str {
    "cshelld"
}

async fn connect_once(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<DesktopDaemonView>>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    log_delivery_revision: &mut u64,
    requested_action: Option<DesktopSessionAction>,
    requests: DesktopRequestReceivers<'_>,
) -> Result<(), DesktopConnectionError> {
    while requests.input.try_recv().is_ok() {}
    let diagnostic_epoch=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).connection_attempt;
    let resolved = config.resolve()?;
    let mut stream = tokio::time::timeout(Duration::from_secs(5), transport::connect(&resolved.endpoint))
        .await.map_err(|_| DesktopConnectionError::IpcSetupTimeout)?
        .map_err(DesktopConnectionError::Connect)?;

    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::VAULT_CONTROL
        | features::FULL_FRAME_RECOVERY
        | features::PRIORITY_STREAMS
        | features::LOG_PAGING
        | features::TERMINAL_CONTROL
        | features::SSH_PROFILE_SESSION
        | features::SSH_SESSION_STATUS
        | features::LOCAL_PROFILE
        | features::LOCAL_LAUNCH_OPTIONS
        | features::TERMINAL_CONTEXT_LAUNCH
        | features::COMPOSE_INPUT;
    let negotiated = tokio::time::timeout(Duration::from_secs(5), client_handshake(&mut stream, 1, handshake))
        .await.map_err(|_| DesktopConnectionError::IpcSetupTimeout)??;
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .compose_supported = negotiated.feature_bits & features::COMPOSE_INPUT != 0;
    if config.request_log_pages && negotiated.feature_bits & features::LOG_PAGING == 0 {
        return Err(DesktopConnectionError::LogPagingNotNegotiated);
    }
    if negotiated.feature_bits & features::TERMINAL_CONTROL == 0 {
        return Err(DesktopConnectionError::TerminalControlNotNegotiated);
    }

    if matches!(requested_action, Some(DesktopSessionAction::Idle)) {
        {
            let mut view = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A new tab can queue its launch while this initial Idle handshake
            // is in flight. Do not clear the launch state before consuming it.
            if requests.open.has_changed().unwrap_or(false)
                && let Some(action) = requests.open.borrow_and_update().clone()
            {
                return Err(DesktopConnectionError::SwitchSession(action));
            }
            view.session_id = None;
            view.session_profile_id = None;
            view.session_title = None;
            view.snapshot = None;
            view.log_page = None;
            view.history_search = None;
            view.session_running = false;
            view.session_reconnecting = false;
            view.session_opening = false;
        }
        update_view(
            shared,
            false,
            "Select an active session or explicitly open a restored tab".into(),
            None,
        );
        let mut id = 2_u64;
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                changed = requests.open.changed() => {
                    if changed.is_ok() && let Some(action) = requests.open.borrow_and_update().clone() {
                        return Err(DesktopConnectionError::SwitchSession(action));
                    }
                }
                result = async {
                    write_envelope(&mut stream, &Envelope { request_id: id,
                        payload: Some(envelope::Payload::SessionListRequest(cshell_ipc::SessionListRequest {})),
                        ..Default::default() }).await?;
                    let response = read_envelope(&mut stream).await?;
                    if response.request_id != id { return Err(DesktopConnectionError::UnexpectedControlResponse); }
                    let Some(envelope::Payload::SessionListResponse(list)) = response.payload else {
                        return Err(DesktopConnectionError::UnexpectedControlResponse);
                    };
                    shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sessions = Some(list.sessions);
                    id = id.saturating_add(1);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Ok::<_, DesktopConnectionError>(())
                } => { result?; }
            }
        }
    }
    let mut request_id = 2_u64;
    if let Some(DesktopSessionAction::CloseView(session_id, terminate)) = requested_action.as_ref()
    {
        if !terminate && negotiated.feature_bits & features::LOCAL_LAUNCH_OPTIONS == 0 {
            return Err(DesktopConnectionError::ProfileSessionNotNegotiated);
        }
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            // A restarted daemon may no longer know the captured runtime ID.
            // Confirm absence before closing, so explicit reconnect can recover
            // without treating an already-gone session as an uncertain close.
            if *terminate {
                write_envelope(&mut stream, &Envelope {
                    request_id,
                    payload: Some(envelope::Payload::SessionListRequest(cshell_ipc::SessionListRequest {})),
                    ..Default::default()
                }).await?;
                let response = read_envelope(&mut stream).await?;
                if response.request_id != request_id { return Err(DesktopConnectionError::UnexpectedControlResponse); }
                let Some(envelope::Payload::SessionListResponse(list)) = response.payload else {
                    return Err(DesktopConnectionError::UnexpectedControlResponse);
                };
                if !list.sessions.iter().any(|session| session.session_id == session_id.as_uuid().as_bytes()) {
                    return Ok(list.sessions);
                }
                request_id += 1;
            }
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id,
                    payload: Some(envelope::Payload::SessionCloseRequest(
                        cshell_ipc::SessionCloseRequest {
                            session_id: session_id.as_uuid().as_bytes().to_vec(),
                            apply_view_policy: !terminate,
                        },
                    )),
                    ..Envelope::default()
                },
            )
            .await?;
            let response = read_envelope(&mut stream).await?;
            if response.request_id != request_id
                || !matches!(
                response.payload, Some(envelope::Payload::SessionCloseResponse(ref closed))
                if closed.session_id == session_id.as_uuid().as_bytes())
            {
                return Err(DesktopConnectionError::UnexpectedControlResponse);
            }
            request_id += 1;
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id,
                    payload: Some(envelope::Payload::SessionListRequest(
                        cshell_ipc::SessionListRequest {},
                    )),
                    ..Envelope::default()
                },
            )
            .await?;
            let response = read_envelope(&mut stream).await?;
            if response.request_id != request_id {
                return Err(DesktopConnectionError::UnexpectedControlResponse);
            }
            let Some(envelope::Payload::SessionListResponse(list)) = response.payload else {
                return Err(DesktopConnectionError::UnexpectedControlResponse);
            };
            Ok::<_, DesktopConnectionError>(list.sessions)
        })
        .await
        .map_err(|_| DesktopConnectionError::SessionCloseUncertain)?
        .map_err(|_| DesktopConnectionError::SessionCloseUncertain);
        let sessions = closed?;
        if *terminate && sessions.iter().any(|session| session.session_id == session_id.as_uuid().as_bytes()) {
            return Err(DesktopConnectionError::SessionCloseUncertain);
        }
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_id = None;
        view.session_profile_id = None;
        view.session_title = None;
        view.session_running = false;
            view.session_reconnecting = false;
        view.session_opening = false;
        view.invalidate_input();
        view.snapshot = None;
        view.sessions = Some(sessions);
        view.closed_session = Some(*session_id);
        view.close_pending = None;
        view.close_error = None;
        view.log_page = None;
        view.history_search = None;
        return Err(DesktopConnectionError::ViewClosed);
    }
    let existing_session = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .session_id;
    let launch = match requested_action.as_ref() {
        Some(DesktopSessionAction::OpenProfile(id) | DesktopSessionAction::NewShell(id)) => {
            Some((*id, None, None))
        }
        Some(DesktopSessionAction::OpenLocal(id, options)) => {
            Some((*id, Some(options.as_ref().clone()), None))
        }
        Some(DesktopSessionAction::OpenContext(id, report)) => {
            Some((*id, None, Some(report.clone())))
        }
        _ => None,
    };
    let (session_id, session_title) = if let Some((profile_id, options, context)) = launch {
        if options.is_some() && negotiated.feature_bits & features::LOCAL_LAUNCH_OPTIONS == 0 {
            return Err(DesktopConnectionError::ProfileSessionNotNegotiated);
        }
        if negotiated.feature_bits & (features::SSH_PROFILE_SESSION | features::LOCAL_PROFILE)
            != (features::SSH_PROFILE_SESSION | features::LOCAL_PROFILE)
        {
            return Err(DesktopConnectionError::ProfileSessionNotNegotiated);
        }
        let size = *requests.resize.borrow();
        let trace_enabled=negotiated.protocol_minor>=77;
        let creation = async { if let Some(report) = context {
            if negotiated.feature_bits & features::TERMINAL_CONTEXT_LAUNCH == 0 {
                return Err(DesktopConnectionError::ProfileSessionNotNegotiated);
            }
            create_terminal_context_session(
                &mut stream,
                &mut request_id,
                &report,
                size.map_or(24, |value| u32::from(value.rows)),
                size.map_or(80, |value| u32::from(value.cols)),
            )
            .await
        } else {
            create_saved_profile_session(
                &mut stream,
                &mut request_id,
                profile_id,
                size.map_or(24, |value| u32::from(value.rows)),
                size.map_or(80, |value| u32::from(value.cols)),
                options,
            )
            .await
        }};
        let capture=shared.clone();
        let creation=crate::connection_diagnostics::observe(trace_enabled,move|trace|{let mut view=capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.connection_attempt==diagnostic_epoch{view.connection_trace=Some(trace);}},creation);
        let created=tokio::select! {biased;_=shutdown.changed()=>return Ok(()), result=creation=>result};
        if created.is_err(){let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.connection_attempt==diagnostic_epoch{crate::connection_diagnostics::uncertain(&mut view.connection_trace);}}
        let created=created
        .map_err(|error| match error {
            DesktopConnectionError::ProfileLaunch(_) | DesktopConnectionError::LaunchCancelled => error,
            _ => DesktopConnectionError::SessionCreationUncertain,
        })?;
        shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .launch_error = None;
        created
    } else if let Some(DesktopSessionAction::DuplicateSshChannel(source, profile)) = requested_action.as_ref() {
        if negotiated.protocol_minor < 63 || negotiated.feature_bits & features::SSH_PROFILE_SESSION == 0 {
            return Err(DesktopConnectionError::ProfileLaunch("后台需要更新后才能复制 SSH 渠道".into()));
        }
        let size=*requests.resize.borrow();
        let capture=shared.clone();
        let result=crate::connection_diagnostics::observe(negotiated.protocol_minor>=77,move|trace|{let mut view=capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.connection_attempt==diagnostic_epoch{view.connection_trace=Some(trace);}},create_ssh_channel_session(&mut stream,&mut request_id,*source,*profile,size.unwrap_or(TerminalSize::cells(24,80)))).await;
        if result.is_err(){let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.connection_attempt==diagnostic_epoch{crate::connection_diagnostics::uncertain(&mut view.connection_trace);}}result?
    } else if matches!(requested_action, Some(DesktopSessionAction::NewDefault)) {
        write_envelope(
            &mut stream,
            &Envelope {
                request_id,
                payload: Some(envelope::Payload::SessionCreateRequest(
                    cshell_ipc::SessionCreateRequest {connection_diagnostics:false,
                        source_ssh_session_id: None,
                        rows: 24,
                        cols: 80,
                        profile_id: None,
                        local_launch: None,
                    },
                )),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| DesktopConnectionError::SessionCreationUncertain)?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| DesktopConnectionError::SessionCreationUncertain)?;
        if response.request_id != request_id {
            return Err(DesktopConnectionError::SessionCreationUncertain);
        }
        request_id += 1;
        let Some(envelope::Payload::SessionCreateResponse(response)) = response.payload else {
            return Err(DesktopConnectionError::SessionCreationUncertain);
        };
        let session = response
            .session
            .ok_or(DesktopConnectionError::SessionCreationUncertain)?;
        (
            SessionId::from_bytes(
                session
                    .session_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| DesktopConnectionError::SessionCreationUncertain)?,
            ),
            session.title,
        )
    } else if let Some(DesktopSessionAction::Attach(session_id)) = requested_action.as_ref() {
        discover_or_create_session(&mut stream, &mut request_id, Some(*session_id), false).await?
    } else if let Some(session_id) = config.session_id {
        discover_or_create_session(&mut stream, &mut request_id, Some(session_id), false).await?
    } else {
        discover_or_create_session(
            &mut stream,
            &mut request_id,
            existing_session,
            existing_session.is_none(),
        )
        .await?
    };
    update_session(shared, session_id, session_title);
    {
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_profile_id = None;
        view.session_opening = negotiated.feature_bits & features::SSH_SESSION_STATUS != 0;
        if view.session_opening {
            view.session_running = false;
            view.session_reconnecting = false;
        }
    }
    if let Some(
        DesktopSessionAction::OpenProfile(id)
        | DesktopSessionAction::NewShell(id)
        | DesktopSessionAction::OpenLocal(id, _)
        | DesktopSessionAction::OpenContext(id, _),
    ) = requested_action.as_ref()
    {
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_profile_id = Some(*id);
        if matches!(requested_action, Some(DesktopSessionAction::NewShell(_))) {
            view.session_title = view
                .session_title
                .as_ref()
                .map(|title| format!("{title} · New Shell"));
            view.terminal_detail = "New Shell opened; the previous process is not restored".into();
        }
    }
    if let Some(DesktopSessionAction::DuplicateSshChannel(_, profile)) = requested_action.as_ref() {
        let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_profile_id=*profile;
        view.terminal_detail="已在原 SSH 连接打开独立渠道，无需再次认证".into();
    }
    let mut replica = TerminalSubscriptionReplica::new(session_id);
    write_snapshot_request(&mut stream, request_id, replica.snapshot_request()).await?;
    request_id = request_id.saturating_add(1);
    if let Some(size) = *requests.resize.borrow_and_update() {
        write_resize_request(&mut stream, request_id, session_id, size).await?;
        request_id = request_id.saturating_add(1);
    }
    update_view(shared, true, "daemon connected".to_owned(), None);
    let log_shutdown = shutdown.clone();
    let log_pages = serve_log_pages_if_enabled(
        config.request_log_pages,
        &resolved,
        session_id,
        shared,
        log_shutdown,
        requests.log_pages,
        log_delivery_revision,
    );
    tokio::pin!(log_pages);
    let history_search = serve_history_search_if_enabled(
        config.request_log_pages,
        &resolved,
        session_id,
        shared,
        shutdown.clone(),
        requests.history_search,
    );
    tokio::pin!(history_search);
    let mut lifecycle_tick = tokio::time::interval(Duration::from_secs(1));
    lifecycle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut lifecycle_request_id = None;
    let (mut reader, mut writer) = tokio::io::split(stream);
    loop {
        // Keep a partial frame read alive while lifecycle and input writes run.
        // read_envelope uses read_exact and cannot be restarted after cancellation.
        let envelope = {
            let incoming = read_envelope(&mut reader);
            tokio::pin!(incoming);
            loop {
                tokio::select! {
                envelope = &mut incoming => break envelope?,
                result = &mut log_pages => return result,
                result = &mut history_search => return result,
                Some(input) = requests.input.recv() => {
                    if let Err(status) = input.validate(shared, session_id) {
                        let mut view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        view.control_error = Some(format!("queued terminal input rejected: {status:?}"));
                        continue;
                    }
                    write_input_request(&mut writer, request_id, session_id, &input.action, input.observed_generation).await?;
                    request_id = request_id.saturating_add(1);
                    continue;
                }
                _ = lifecycle_tick.tick(), if negotiated.feature_bits & features::SSH_SESSION_STATUS != 0 && lifecycle_request_id.is_none() => {
                    write_envelope(&mut writer, &Envelope {
                        request_id, deadline_unix_ms: 0,
                        payload: Some(envelope::Payload::SessionListRequest(cshell_ipc::SessionListRequest {})),
                    }).await?;
                    lifecycle_request_id = Some(request_id);
                    request_id = request_id.saturating_add(1);
                    continue;
                }
                changed = requests.resize.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    if let Some(size) = *requests.resize.borrow_and_update() {
                        write_resize_request(&mut writer, request_id, session_id, size).await?;
                        request_id = request_id.saturating_add(1);
                    }
                    continue;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                    continue;
                }
                changed = requests.open.changed() => {
                    if changed.is_ok()
                        && let Some(id) = requests.open.borrow_and_update().clone() {
                            return Err(DesktopConnectionError::SwitchSession(id));
                        }
                    continue;
                }
                }
            }
        };
        if let Some(envelope::Payload::SessionListResponse(list)) = envelope.payload.as_ref() {
            if lifecycle_request_id != Some(envelope.request_id) {
                return Err(DesktopConnectionError::UnexpectedControlResponse);
            }
            lifecycle_request_id = None;
            shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sessions = Some(list.sessions.clone());
            update_session_lifecycle(shared, session_id, list)?;
            continue;
        }
        if let Some(envelope::Payload::TerminalControlResponse(response)) =
            envelope.payload.as_ref()
        {
            update_terminal_control(shared, session_id, response)?;
            continue;
        }
        match replica.apply_envelope(envelope)? {
            ClientFrameUpdate::Applied { generation } => {
                let snapshot = replica.snapshot().cloned();
                update_view(
                    shared,
                    true,
                    format!("daemon connected · generation {generation}"),
                    snapshot,
                );
            }
            ClientFrameUpdate::IgnoredStale => {}
            ClientFrameUpdate::RequestFull(request) => {
                write_snapshot_request(&mut writer, request_id, request).await?;
                request_id = request_id.saturating_add(1);
            }
        }
    }
}

async fn create_terminal_context_session<S>(
    stream: &mut S,
    request_id: &mut u64,
    report: &cshell_application::TerminalResolutionReport,
    rows: u32,
    cols: u32,
) -> Result<(SessionId, String), DesktopConnectionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(report)
        .map_err(|_| DesktopConnectionError::UnexpectedControlResponse)?;
    cshell_application::decode_terminal_resolution_report(&bytes)
        .map_err(|_| DesktopConnectionError::UnexpectedControlResponse)?;
    let id = *request_id;
    let outcome = tokio::time::timeout(Duration::from_secs(300), async {
        write_envelope(
            stream,
            &Envelope {
                request_id: id,
                payload: Some(envelope::Payload::TerminalContextLaunchRequest(
                    cshell_ipc::TerminalContextLaunchRequest {
                        connection_diagnostics:crate::connection_diagnostics::requested(),
                        report_json: bytes,
                        rows,
                        cols,
                        confirmed: true,
                    },
                )),
                ..Default::default()
            },
        )
        .await?;
        let response = crate::connection_diagnostics::read_response(stream,id).await?;
        if response.request_id != id {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        }
        let Some(envelope::Payload::TerminalContextLaunchResponse(response)) = response.payload
        else {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        };
        let result = response
            .result
            .ok_or(DesktopConnectionError::UnexpectedControlResponse)?;
        if result.detail.len() > 4096 {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        }
        let Some(session) = result.session else {
            if !response.report_json.is_empty() {
                return Err(DesktopConnectionError::UnexpectedControlResponse);
            }
            return Err(profile_creation_failure(result.failure_code,result.detail));
        };
        if result.failure_code != SessionFailureCode::None as i32
            || !result.detail.is_empty()
            || session.profile_id.as_deref()
                != Some(report.context.profile_id.as_uuid().as_bytes().as_slice())
            || session.title != report.profile.name
        {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        }
        let receipt = cshell_application::decode_terminal_resolution_report(&response.report_json)
            .map_err(|_| DesktopConnectionError::UnexpectedControlResponse)?;
        if receipt != *report {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        }
        let id = SessionId::from_bytes(
            session
                .session_id
                .as_slice()
                .try_into()
                .map_err(|_| DesktopConnectionError::InvalidSessionId)?,
        );
        if id.as_uuid().is_nil() {
            return Err(DesktopConnectionError::InvalidSessionId);
        }
        Ok((id, session.title))
    })
    .await
    .map_err(|_| DesktopConnectionError::SessionCreationUncertain)?;
    *request_id = id.saturating_add(1);
    outcome
}

async fn create_ssh_channel_session<S>(stream:&mut S,request_id:&mut u64,source:SessionId,profile:Option<ProfileId>,size:TerminalSize)->Result<(SessionId,String),DesktopConnectionError>
where S:tokio::io::AsyncRead+tokio::io::AsyncWrite+Unpin {
    let id=*request_id;
    let result=tokio::time::timeout(Duration::from_secs(30),async {
        write_envelope(stream,&Envelope {request_id:id,payload:Some(envelope::Payload::SessionCreateRequest(cshell_ipc::SessionCreateRequest {
            connection_diagnostics:crate::connection_diagnostics::requested(),rows:u32::from(size.rows),cols:u32::from(size.cols),source_ssh_session_id:Some(source.as_uuid().as_bytes().to_vec()),..Default::default()
        })),..Default::default()}).await?;
        let reply=crate::connection_diagnostics::read_response(stream,id).await?;
        if reply.request_id!=id {return Err(DesktopConnectionError::UnexpectedControlResponse);}
        let Some(envelope::Payload::SessionCreateResponse(created))=reply.payload else {return Err(DesktopConnectionError::UnexpectedControlResponse);};
        let Some(session)=created.session else {return Err(profile_creation_failure(created.failure_code,created.detail));};
        if created.failure_code!=SessionFailureCode::None as i32 || !created.detail.is_empty() || !session.can_duplicate_ssh_channel || session.profile_id!=profile.map(|p|p.as_uuid().as_bytes().to_vec()) {return Err(DesktopConnectionError::UnexpectedControlResponse);}
        let bytes:[u8;16]=session.session_id.as_slice().try_into().map_err(|_|DesktopConnectionError::InvalidSessionId)?;
        let session_id=SessionId::from_bytes(bytes);
        if session_id.as_uuid().is_nil() || session_id==source {return Err(DesktopConnectionError::UnexpectedControlResponse);}
        Ok((session_id,session.title))
    }).await.map_err(|_|DesktopConnectionError::SessionCreationUncertain)?;
    *request_id=id.saturating_add(1);
    result.map_err(|error|match error {DesktopConnectionError::ProfileLaunch(_)|DesktopConnectionError::LaunchCancelled=>error,_=>DesktopConnectionError::SessionCreationUncertain})
}

async fn create_saved_profile_session<S>(
    stream: &mut S,
    request_id: &mut u64,
    profile_id: ProfileId,
    rows: u32,
    cols: u32,
    local_launch: Option<cshell_ipc::LocalLaunchOptions>,
) -> Result<(SessionId, String), DesktopConnectionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let id = *request_id;
    write_envelope(
        stream,
        &Envelope {
            request_id: id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::SessionCreateRequest(
                cshell_ipc::SessionCreateRequest {connection_diagnostics:crate::connection_diagnostics::requested(),
                    source_ssh_session_id: None,
                    local_launch,
                    rows,
                    cols,
                    profile_id: Some(profile_id.as_uuid().as_bytes().to_vec()),
                },
            )),
        },
    )
    .await?;
    let response = crate::connection_diagnostics::read_response(stream,id).await?;
    *request_id = id.saturating_add(1);
    if response.request_id != id {
        return Err(DesktopConnectionError::UnexpectedControlResponse);
    }
    let Some(envelope::Payload::SessionCreateResponse(created)) = response.payload else {
        return Err(DesktopConnectionError::UnexpectedControlResponse);
    };
    let session = created.session.ok_or_else(|| profile_creation_failure(created.failure_code,created.detail))?;
    let bytes: [u8; 16] = session
        .session_id
        .as_slice()
        .try_into()
        .map_err(|_| DesktopConnectionError::InvalidSessionId)?;
    Ok((SessionId::from_bytes(bytes), session.title))
}

fn profile_creation_failure(code:i32,detail:String)->DesktopConnectionError {
    let code=SessionFailureCode::try_from(code).unwrap_or(SessionFailureCode::ProtocolRejected);
    // Exact compatibility with pre-66 daemons; never infer cancellation from arbitrary peer text.
    if code==SessionFailureCode::AuthenticationCancelled || code==SessionFailureCode::AuthenticationRejected && detail=="SSH 验证已取消，连接未建立。" {
        DesktopConnectionError::LaunchCancelled
    } else {
        DesktopConnectionError::ProfileLaunch(if detail.is_empty(){"daemon did not return a session".into()}else{format!("{code:?}: {detail}")})
    }
}
async fn discover_or_create_session<S>(
    stream: &mut S,
    request_id: &mut u64,
    preferred: Option<SessionId>,
    allow_fallback: bool,
) -> Result<(SessionId, String), DesktopConnectionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let list_request_id = *request_id;
    write_envelope(
        stream,
        &Envelope {
            request_id: list_request_id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::SessionListRequest(
                cshell_ipc::SessionListRequest {},
            )),
        },
    )
    .await?;
    let response = read_envelope(stream).await?;
    *request_id = list_request_id.saturating_add(1);
    if response.request_id != list_request_id {
        return Err(DesktopConnectionError::UnexpectedControlResponse);
    }
    let Some(envelope::Payload::SessionListResponse(list)) = response.payload else {
        return Err(DesktopConnectionError::UnexpectedControlResponse);
    };
    let session = preferred
        .and_then(|id| {
            list.sessions
                .iter()
                .find(|session| session.session_id == id.as_uuid().as_bytes())
                .cloned()
        })
        .or_else(|| {
            allow_fallback
                .then(|| {
                    list.sessions
                        .iter()
                        .find(|session| session.running)
                        .cloned()
                })
                .flatten()
        })
        .or_else(|| {
            allow_fallback
                .then(|| list.sessions.into_iter().next())
                .flatten()
        });
    let session = match session {
        Some(session) => session,
        None if !allow_fallback => {
            return Err(DesktopConnectionError::SessionUnavailable);
        }
        None => {
            let create_request_id = *request_id;
            write_envelope(
                stream,
                &Envelope {
                    request_id: create_request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionCreateRequest(
                        cshell_ipc::SessionCreateRequest {connection_diagnostics:false,
                            source_ssh_session_id: None,
                            local_launch: None,
                            rows: 24,
                            cols: 80,
                            profile_id: None,
                        },
                    )),
                },
            )
            .await?;
            let response = read_envelope(stream).await?;
            *request_id = create_request_id.saturating_add(1);
            if response.request_id != create_request_id {
                return Err(DesktopConnectionError::UnexpectedControlResponse);
            }
            let Some(envelope::Payload::SessionCreateResponse(created)) = response.payload else {
                return Err(DesktopConnectionError::UnexpectedControlResponse);
            };
            created
                .session
                .ok_or(DesktopConnectionError::UnexpectedControlResponse)?
        }
    };
    let bytes: [u8; 16] = session
        .session_id
        .as_slice()
        .try_into()
        .map_err(|_| DesktopConnectionError::InvalidSessionId)?;
    Ok((SessionId::from_bytes(bytes), session.title))
}

async fn serve_log_pages_if_enabled(
    enabled: bool,
    resolved: &ResolvedDesktopEndpoint,
    session_id: SessionId,
    shared: &Arc<Mutex<DesktopDaemonView>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    requests: &mut tokio::sync::watch::Receiver<Option<DesktopLogPageRequest>>,
    delivery_revision: &mut u64,
) -> Result<(), DesktopConnectionError> {
    if !enabled {
        return std::future::pending().await;
    }
    let delays = [100_u64, 250, 500, 1_000, 2_000, 5_000];
    let mut attempt = 0_usize;
    loop {
        while requests.borrow().is_none() {
            tokio::select! {
                changed=requests.changed()=>{if changed.is_err(){return Ok(());}},
                _=shutdown.changed()=>return Ok(()),
            }
        }
        let result = log_page_loop(
            resolved,
            session_id,
            shared,
            shutdown.clone(),
            requests,
            delivery_revision,
        )
        .await;
        if *shutdown.borrow() {
            return Ok(());
        }
        if let Err(error) = result {
            tracing::warn!(%error, "dedicated log paging connection failed; retrying");
        } else {
            attempt = 0;
        }
        let delay = delays[attempt.min(delays.len() - 1)];
        attempt = attempt.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

async fn serve_history_search_if_enabled(
    enabled: bool,
    resolved: &ResolvedDesktopEndpoint,
    session_id: SessionId,
    shared: &Arc<Mutex<DesktopDaemonView>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    requests: &mut tokio::sync::watch::Receiver<Option<DesktopHistorySearchRequest>>,
) -> Result<(), DesktopConnectionError> {
    if !enabled {
        return std::future::pending().await;
    }
    let delays = [100_u64, 250, 500, 1_000, 2_000, 5_000];
    let mut attempt = 0_usize;
    loop {
        let result =
            history_search_loop(resolved, session_id, shared, shutdown.clone(), requests).await;
        if *shutdown.borrow() {
            return Ok(());
        }
        match result {
            Ok(HistorySearchLoopExit::Stopped) => return Ok(()),
            Ok(HistorySearchLoopExit::Superseded) => {
                attempt = 0;
                while requests.borrow_and_update().is_none() {
                    tokio::select! {
                        changed = requests.changed() => {
                            if changed.is_err() {
                                return Ok(());
                            }
                        }
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return Ok(());
                            }
                        }
                    }
                }
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "dedicated history search connection failed; retrying");
            }
        }
        let delay = delays[attempt.min(delays.len() - 1)];
        attempt = attempt.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum HistorySearchLoopExit {
    Stopped,
    Superseded,
}

async fn history_search_loop(
    resolved: &ResolvedDesktopEndpoint,
    session_id: SessionId,
    shared: &Arc<Mutex<DesktopDaemonView>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    requests: &mut tokio::sync::watch::Receiver<Option<DesktopHistorySearchRequest>>,
) -> Result<HistorySearchLoopExit, DesktopConnectionError> {
    #[cfg(windows)]
    let mut stream = transport::connect(&resolved.endpoint)
        .await
        .map_err(DesktopConnectionError::Connect)?;
    #[cfg(unix)]
    let mut stream = transport::connect(&resolved.endpoint)
        .await
        .map_err(DesktopConnectionError::Connect)?;
    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::HISTORY_SEARCH;
    let negotiated = client_handshake(&mut stream, 1, handshake).await?;
    if negotiated.feature_bits & features::HISTORY_SEARCH == 0 {
        return Err(DesktopConnectionError::HistorySearchNotNegotiated);
    }

    let mut request_id = 2_u64;
    loop {
        let desired = loop {
            if let Some(request) = requests.borrow_and_update().clone() {
                break request;
            }
            tokio::select! {
                changed = requests.changed() => {
                    if changed.is_err() {
                        return Ok(HistorySearchLoopExit::Stopped);
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(HistorySearchLoopExit::Stopped);
                    }
                }
            }
        };
        write_history_search_request(&mut stream, request_id, session_id, &desired).await?;
        let mut response = tokio::select! {
            response = read_envelope(&mut stream) => response?,
            changed = requests.changed() => {
                if changed.is_err() {
                    return Ok(HistorySearchLoopExit::Stopped);
                }
                // A partial read_exact cannot be resumed. Drop this dedicated
                // connection and reconnect for the latest query or cancellation.
                return Ok(HistorySearchLoopExit::Superseded);
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(HistorySearchLoopExit::Stopped);
                }
                return Ok(HistorySearchLoopExit::Superseded);
            }
        };
        if response.request_id != request_id {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        }
        let Some(envelope::Payload::HistorySearchResult(result)) = response.payload.take() else {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        };
        result.validate()?;
        if result.session_id.as_slice() != session_id.as_uuid().as_bytes() {
            return Err(DesktopConnectionError::InvalidSessionId);
        }
        if requests
            .borrow()
            .as_ref()
            .is_some_and(|current| current == &desired)
        {
            update_history_search(
                shared,
                DesktopHistorySearchResponse {
                    revision: desired.revision,
                    result,
                },
            );
        }
        request_id = request_id.saturating_add(1);
        tokio::select! {
            changed = requests.changed() => {
                if changed.is_err() {
                    return Ok(HistorySearchLoopExit::Stopped);
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(HistorySearchLoopExit::Stopped);
                }
            }
        }
    }
}

async fn log_page_loop(
    resolved: &ResolvedDesktopEndpoint,
    session_id: SessionId,
    shared: &Arc<Mutex<DesktopDaemonView>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    requests: &mut tokio::sync::watch::Receiver<Option<DesktopLogPageRequest>>,
    delivery_revision: &mut u64,
) -> Result<(), DesktopConnectionError> {
    #[cfg(windows)]
    let mut stream = transport::connect(&resolved.endpoint)
        .await
        .map_err(DesktopConnectionError::Connect)?;
    #[cfg(unix)]
    let mut stream = transport::connect(&resolved.endpoint)
        .await
        .map_err(DesktopConnectionError::Connect)?;
    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::LOG_PAGING | features::TERMINAL_SCROLLBACK;
    let negotiated = client_handshake(&mut stream, 1, handshake).await?;
    if negotiated.feature_bits & features::LOG_PAGING == 0 {
        return Err(DesktopConnectionError::LogPagingNotNegotiated);
    }

    let mut desired = requests
        .borrow_and_update()
        .as_ref()
        .copied()
        .unwrap_or_else(DesktopLogPageRequest::initial_tail);
    let mut request_id = 2_u64;
    loop {
        if desired.terminal_history && negotiated.feature_bits & features::TERMINAL_SCROLLBACK==0 {return Err(DesktopConnectionError::LogPagingNotNegotiated);}
        write_log_page_request(&mut stream, request_id, session_id, desired).await?;
        let mut response = tokio::select! {
            response = read_envelope(&mut stream) => response?,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
        };
        if response.request_id != request_id {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        }
        let Some(envelope::Payload::LogPage(page)) = response.payload.take() else {
            return Err(DesktopConnectionError::UnexpectedControlResponse);
        };
        *delivery_revision = delivery_revision.saturating_add(1);
        // A cancelled view may already have been reopened at the tail. Never
        // publish an old anchored response into that new scrollback view.
        if requests.borrow().as_ref() == Some(&desired) {
            update_log_page(shared, convert_log_page(page, *delivery_revision)?, desired.terminal_history, desired.epoch);
        }
        request_id = request_id.saturating_add(1);

        if desired.anchor_line_id.is_none() {
            tokio::select! {
                changed = requests.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        } else {
            tokio::select! {
                changed = requests.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        }
        if let Some(latest) = requests.borrow_and_update().as_ref().copied() {
            desired = latest;
        } else { return Ok(()); }
    }
}

impl DesktopLogPageRequest {
    const fn initial_tail() -> Self {
        Self {
            epoch: 0,
            terminal_history: false,
            anchor_line_id: None,
            cell_offset: 0,
            rows_before: 256,
            rows_after: 0,
        }
    }
}

async fn write_snapshot_request<S>(
    stream: &mut S,
    request_id: u64,
    request: cshell_ipc::SnapshotRequest,
) -> Result<(), cshell_ipc::IpcError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    write_envelope(
        stream,
        &Envelope {
            request_id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::SnapshotRequest(request)),
        },
    )
    .await
}

async fn write_input_request<S>(
    stream: &mut S,
    request_id: u64,
    session_id: SessionId,
    action: &InputAction,
    observed_generation: Option<u64>,
) -> Result<(), DesktopConnectionError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mut request = TerminalInputRequest::from_action(session_id, action)?;
    request.observed_generation = observed_generation;
    write_envelope(
        stream,
        &Envelope {
            request_id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::TerminalInputRequest(request)),
        },
    )
    .await?;
    Ok(())
}

async fn write_resize_request<S>(
    stream: &mut S,
    request_id: u64,
    session_id: SessionId,
    size: TerminalSize,
) -> Result<(), DesktopConnectionError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    write_envelope(
        stream,
        &Envelope {
            request_id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::TerminalResizeRequest(
                TerminalResizeRequest::from_size(session_id, size),
            )),
        },
    )
    .await?;
    Ok(())
}

async fn write_log_page_request<S>(
    stream: &mut S,
    request_id: u64,
    session_id: SessionId,
    request: DesktopLogPageRequest,
) -> Result<(), cshell_ipc::IpcError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    write_envelope(
        stream,
        &Envelope {
            request_id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::LogPageRequest(IpcLogPageRequest {
                session_id: session_id.as_uuid().as_bytes().to_vec(),
                terminal_history: request.terminal_history,
                anchor_line_id: request.anchor_line_id,
                cell_offset: request.cell_offset,
                rows_before: u32::from(request.rows_before),
                rows_after: u32::from(request.rows_after),
            })),
        },
    )
    .await
}

async fn write_history_search_request<S>(
    stream: &mut S,
    request_id: u64,
    session_id: SessionId,
    request: &DesktopHistorySearchRequest,
) -> Result<(), DesktopConnectionError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let (cursor_line_id, cursor_byte_offset) = request
        .cursor
        .map_or((None, None), |cursor| (Some(cursor.0), Some(cursor.1)));
    let request = IpcHistorySearchRequest {
        session_id: session_id.as_uuid().as_bytes().to_vec(),
        query: request.query.clone(),
        case_sensitive: request.case_sensitive,
        whole_word: request.whole_word,
        regex: request.regex,
        direction: request.direction as i32,
        cursor_line_id,
        cursor_byte_offset,
        max_scan_lines: cshell_ipc::MAX_HISTORY_SEARCH_SCAN_LINES as u32,
        max_matches: 1,
    };
    request.validate()?;
    write_envelope(
        stream,
        &Envelope {
            request_id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::HistorySearchRequest(request)),
        },
    )
    .await?;
    Ok(())
}

fn convert_log_page(
    page: IpcLogPage,
    delivery_revision: u64,
) -> Result<Arc<RenderLogPage>, DesktopConnectionError> {
    page.validate()?;
    let session_bytes: [u8; 16] = page
        .session_id
        .as_slice()
        .try_into()
        .map_err(|_| DesktopConnectionError::InvalidSessionId)?;
    let source_id = LogSourceId(u64::from_le_bytes(
        session_bytes[..8]
            .try_into()
            .map_err(|_| DesktopConnectionError::InvalidSessionId)?,
    ));
    let rows = page
        .rows
        .into_iter()
        .map(|row| {
            let style_spans = row
                .style_spans
                .into_iter()
                .map(|span| {
                    let style = span.style.ok_or(LogPageCodecError::MissingStyle {
                        line_id: row.line_id,
                    })?;
                    Ok(RenderLogStyleSpan {
                        byte_range: span.start..span.end,
                        style: Style::try_from(style)?,
                    })
                })
                .collect::<Result<Vec<_>, DesktopConnectionError>>()?;
            Ok(RenderLogRow {
            soft_wrap: row.soft_wrap,
                line_id: row.line_id,
                text: Arc::from(row.text),
                style_spans: style_spans.into(),
                truncated: row.truncated,
            })
        })
        .collect::<Result<Vec<_>, DesktopConnectionError>>()?;
    Ok(Arc::new(RenderLogPage {
        first_line_id: page.first_line_id.max(1),
        source_id,
        revision: delivery_revision,
        anchor_line_id: page.anchor_line_id,
        rows: rows.into(),
        total_line_count: page.total_line_count,
        has_before: page.has_before,
        has_after: page.has_after,
    }))
}

fn update_view(
    shared: &Arc<Mutex<DesktopDaemonView>>,
    connected: bool,
    detail: String,
    snapshot: Option<FrameSnapshot>,
) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ready = connected && (view.session_id.is_none() || view.session_running);
    if !ready && view.connected {
        view.invalidate_input();
    }
    view.connected = ready;
    if !connected {
        view.control_error = None;
    }
    let control_error = view
        .control_error
        .as_deref()
        .map(|error| format!(" · {error}"))
        .unwrap_or_default();
    let launch_error = view
        .launch_error
        .as_deref()
        .map(|error| format!(" · SSH: {error}"))
        .unwrap_or_default();
    let lifecycle = if view.terminal_detail.is_empty() {
        String::new()
    } else {
        format!(" · {}", view.terminal_detail)
    };
    view.detail = format!("{detail}{lifecycle}{control_error}{launch_error}");
    if let Some(snapshot) = snapshot {
        view.snapshot = Some(Arc::new(snapshot));
    }
}

fn update_log_page(shared: &Arc<Mutex<DesktopDaemonView>>, page: Arc<RenderLogPage>, terminal_history: bool, epoch: u64) {
    let mut view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.log_page_epoch == epoch {
        view.log_page = Some(page);
        view.log_page_terminal_history = terminal_history;
    }
}

fn update_history_search(
    shared: &Arc<Mutex<DesktopDaemonView>>,
    response: DesktopHistorySearchResponse,
) {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .history_search = Some(Arc::new(response));
}

fn update_terminal_control(
    shared: &Arc<Mutex<DesktopDaemonView>>,
    session_id: SessionId,
    response: &TerminalControlResponse,
) -> Result<(), DesktopConnectionError> {
    if response.session_id.as_slice() != session_id.as_uuid().as_bytes() {
        return Err(DesktopConnectionError::InvalidSessionId);
    }
    let status = response.decoded_status()?;
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if status == TerminalControlStatus::Accepted {
        view.control_error = None;
    } else {
        if status == TerminalControlStatus::SessionClosed {
            view.invalidate_input();
            view.session_running = false;
            view.terminal_detail =
                "Terminal session is closed; reconnecting opens a New Shell".into();
        }
        let error = format!("terminal control rejected: {status:?}");
        view.control_error = Some(error.clone());
        view.detail = format!("daemon connected · {error}");
    }
    Ok(())
}

fn update_session(shared: &Arc<Mutex<DesktopDaemonView>>, id: SessionId, title: String) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(id) {
        view.invalidate_input();
        if view.input_security == DesktopInputSecurity::Unknown {
            view.input_security = DesktopInputSecurity::Terminal;
        }
        view.snapshot = None;
        view.log_page = None;
        view.history_search = None;
        view.session_profile_id = None;
        view.terminal_detail.clear();
        view.session_running = true;
        view.session_reconnecting = false;
    }
    view.session_id = Some(id);
    view.session_title = Some(title);
}

fn update_session_lifecycle(
    shared: &Arc<Mutex<DesktopDaemonView>>,
    id: SessionId,
    list: &cshell_ipc::SessionListResponse,
) -> Result<(), DesktopConnectionError> {
    {
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_opening = false;
        if let Some(session) = list
            .sessions
            .iter()
            .find(|session| session.session_id == id.as_uuid().as_bytes())
        {
            view.session_profile_id = session
                .profile_id
                .as_ref()
                .map(|bytes| {
                    let bytes: [u8; 16] = bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| DesktopConnectionError::InvalidSessionId)?;
                    Ok::<_, DesktopConnectionError>(ProfileId::from_bytes(bytes))
                })
                .transpose()?;
            view.session_running = session.running;
            view.session_reconnecting = session.reconnecting;
            if session.running && !session.terminal_detail.is_empty() {
                view.terminal_detail = session.terminal_detail.clone();
            } else if !session.running {
                view.terminal_detail = if session.terminal_detail.is_empty() {
                    "Terminal disconnected; reconnecting opens a New Shell".into()
                } else {
                    session.terminal_detail.clone()
                };
            }
        } else {
            view.session_running = false;
            view.session_reconnecting = false;
            view.terminal_detail = "Previous session is no longer available".into();
        }
    }
    update_view(shared, true, "daemon connected".into(), None);
    Ok(())
}

fn required_hex<const N: usize>(name: &'static str) -> Result<[u8; N], DesktopConnectionError> {
    let value =
        std::env::var(name).map_err(|_| DesktopConnectionError::MissingEnvironment(name))?;
    decode_hex::<N>(&value).ok_or(DesktopConnectionError::InvalidHex { name, expected: N })
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 {
        return None;
    }
    let mut decoded = [0_u8; N];
    for (index, slot) in decoded.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&value[start..start + 2], 16).ok()?;
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_authentication_cancel_is_not_a_launch_error_but_rejection_and_timeout_are() {
        use super::*;
        for (code,detail) in [(SessionFailureCode::AuthenticationCancelled,"cancelled"),(SessionFailureCode::AuthenticationRejected,"SSH 验证已取消，连接未建立。")] {
            assert!(matches!(profile_creation_failure(code as i32,detail.into()),DesktopConnectionError::LaunchCancelled));
        }
        for detail in ["wrong password","SSH 验证输入已超时，请重新连接后填写新的验证码。","peer text contains cancelled"] {
            assert!(matches!(profile_creation_failure(SessionFailureCode::AuthenticationRejected as i32,detail.into()),DesktopConnectionError::ProfileLaunch(_)));
        }
    }
    #[tokio::test(flavor="multi_thread",worker_threads=2)]
    async fn cancelled_profile_worker_stops_without_replay_or_default_shell() {
        let _root=tempfile::tempdir().unwrap();
        #[cfg(windows)] let endpoint=format!(r"\\.\pipe\cshell-cancel-profile-{}",SessionId::new());
        #[cfg(unix)] let endpoint=_root.path().join("cancel.sock");
        let listener=transport::LocalListener::bind(&endpoint).unwrap();
        let config=DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint{endpoint,instance_token:[91;32],daemon_instance_id:[92;16]});
        let calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));let created=calls.clone();
        let (stop,mut stopped)=tokio::sync::watch::channel(false);
        let server=tokio::spawn(async move {
            let mut connections=tokio::task::JoinSet::new();
            loop {
                let mut stream=tokio::select!{stream=listener.accept()=>stream.unwrap(),_=stopped.changed()=>break};let created=created.clone();
                connections.spawn(async move {
                    if server_handshake(&mut stream,&HandshakePolicy::with_instance_id([91;32],[92;16],u64::MAX)).await.is_err(){return;}
                    while let Ok(query)=read_envelope(&mut stream).await {
                        let payload=match query.payload {
                            Some(envelope::Payload::SessionListRequest(_))=>envelope::Payload::SessionListResponse(SessionListResponse{sessions:vec![]}),
                            Some(envelope::Payload::SessionCreateRequest(request))=>{
                                assert!(request.profile_id.is_some(),"cancel must not create a local fallback");created.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                                envelope::Payload::SessionCreateResponse(SessionCreateResponse{session:None,failure_code:cshell_ipc::SessionFailureCode::AuthenticationCancelled as i32,detail:"cancelled by user".into()})
                            },
                            _=>panic!("unexpected control request"),
                        };
                        if write_envelope(&mut stream,&Envelope{request_id:query.request_id,payload:Some(payload),..Default::default()}).await.is_err(){return;}
                    }
                });
            }
            connections.abort_all();while connections.join_next().await.is_some(){}
        });
        let connection=DesktopDaemonConnection::start_idle(config).unwrap();let profile=ProfileId::new();assert!(connection.open_profile(profile));
        tokio::time::timeout(Duration::from_secs(10),async {loop {if connection.view().launch_cancelled && !connection.view().session_opening {break;}tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
        let view=connection.view();assert_eq!(view.session_profile_id,Some(profile));assert!(view.session_id.is_none());assert!(view.launch_error.is_none());assert!(!view.connected);
        tokio::time::sleep(Duration::from_millis(350)).await;assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst),1);
        drop(connection);let _=stop.send(true);server.await.unwrap();
    }
    use super::{
        DesktopConnectionConfig, DesktopDaemonConnection, DesktopEndpointSource,
        DesktopHistorySearchRequest, DesktopLogPageRequest, ResolvedDesktopEndpoint,
        convert_log_page, create_saved_profile_session, decode_hex, discover_or_create_session,
        log_page_loop, serve_history_search_if_enabled, update_terminal_control,
    };
    use cshell_domain::{InputAction, ProfileId, SessionId, TerminalSize};
    use cshell_ipc::{
        DiscoveryPublication, DiscoveryRecord, Envelope, HandshakePolicy, HistorySearchDirection,
        HistorySearchMatch, HistorySearchResult, LogPage, LogRow, LogStyleSpan, RuntimePaths,
        SessionCreateResponse, SessionListResponse, SessionSummary, TerminalControlResponse,
        TerminalControlStatus, TerminalStyle, envelope, features, read_envelope, server_handshake,
        transport, write_envelope,
    };
    use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
    use cshell_terminal::{Color, Style};
    use cshelld::{LocalSessionRegistry, SessionIpcServer, SessionIpcService};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn remove_retained_session_uses_confirmed_id_and_preserves_current_input() {
        let active = SessionId::new();
        let retained = SessionId::new();
        let shared = Arc::new(Mutex::new(super::DesktopDaemonView {
            session_id: Some(active), connected: true,
            sessions: Some(vec![SessionSummary {
                can_duplicate_ssh_channel: false,
                reconnecting: false, session_id: retained.as_uuid().as_bytes().to_vec(), ..Default::default() }]),
            ..Default::default()
        }));
        let epoch = shared.lock().unwrap().input_epoch;
        let (input, _input_receiver) = tokio::sync::mpsc::channel(1);
        let (open_requests, mut requests) = tokio::sync::watch::channel(None);
        let connection = DesktopDaemonConnection {
            input: crate::daemon_input::DesktopInputSender::new(shared.clone(), input),
            shared,
            shutdown: tokio::sync::watch::channel(false).0,
            log_requests: tokio::sync::watch::channel(None).0,
            history_search_requests: tokio::sync::watch::channel(None).0,
            resize_requests: tokio::sync::watch::channel(None).0,
            open_requests, worker: None,
        };
        assert!(!connection.terminate_confirmed_session(SessionId::new()));
        assert!(!requests.has_changed().unwrap());
        assert!(connection.terminate_confirmed_session(retained));
        assert!(!connection.attach_session(active), "an attach must not overwrite a queued termination");
        assert!(!connection.new_default_shell(), "a new shell must not overwrite a queued termination");
        assert!(!connection.close_session_view(active));
        assert_eq!(*requests.borrow_and_update(), Some(super::DesktopSessionAction::CloseView(retained, true)));
        assert_eq!(connection.view().session_id, Some(active));
        assert_eq!(connection.view().input_epoch, epoch);
    }

    #[test]
    fn verified_host_key_retry_reuses_the_failed_profile_tab() {
        let profile = ProfileId::new();
        let shared = Arc::new(Mutex::new(super::DesktopDaemonView {
            session_profile_id: Some(profile),
            launch_error: Some("HostKeyUnknown: test".into()),
            ..Default::default()
        }));
        let connection = DesktopDaemonConnection {
            input: crate::daemon_input::DesktopInputSender::new(shared.clone(), tokio::sync::mpsc::channel(1).0),
            shared, shutdown: tokio::sync::watch::channel(false).0,
            log_requests: tokio::sync::watch::channel(None).0,
            history_search_requests: tokio::sync::watch::channel(None).0,
            resize_requests: tokio::sync::watch::channel(None).0,
            open_requests: tokio::sync::watch::channel(None).0,
            worker: None,
        };
        let mut app = crate::DesktopApp::default();
        let tab = app.workspace.add(Some(profile), "Saved SSH".into()).unwrap();
        app.current_workspace_tab = Some(tab);
        app.daemon = Some(connection);
        let before = app.workspace.document.clone();
        assert!(app.prepare_profile_tab(profile));
        assert_eq!(app.workspace.pending_launch, Some(tab));
        assert_eq!(app.current_workspace_tab, Some(tab));
        assert_eq!(app.workspace.document, before, "retry must not create another placeholder");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_with_or_without_saved_tabs_only_lists_live_sessions() {
        for restore_exists in [false, true] {
            let _directory = tempfile::tempdir().unwrap();
            let live = SessionId::new();
            #[cfg(windows)]
            let endpoint = format!(r"\\.\pipe\cshell-empty-start-{live}");
            #[cfg(unix)]
            let endpoint = _directory.path().join("empty-start.sock");
            let listener = transport::LocalListener::bind(&endpoint).unwrap();
            let token = [0x27; 32];
            let instance = [0x38; 16];
            let server = tokio::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                server_handshake(&mut stream, &HandshakePolicy::with_instance_id(token, instance, features::TERMINAL_CONTROL)).await.unwrap();
                for _ in 0..2 {
                    let request = read_envelope(&mut stream).await.unwrap();
                    assert!(matches!(request.payload, Some(envelope::Payload::SessionListRequest(_))), "startup must not create or attach any terminal");
                    write_envelope(&mut stream, &Envelope {
                        request_id: request.request_id,
                        payload: Some(envelope::Payload::SessionListResponse(SessionListResponse {
                            sessions: vec![SessionSummary {
                                can_duplicate_ssh_channel: false,
                                reconnecting: false, session_id: live.as_uuid().as_bytes().to_vec(), title: "Retained session".into(), running: true, ..Default::default() }],
                        })), ..Default::default()
                    }).await.unwrap();
                }
            });
            let mut old = crate::workspace_model::DesktopWorkspace::default();
            old.add(Some(ProfileId::new()), "Previous SSH".into()).unwrap();
            let snapshot = restore_exists.then_some(cshell_application::WorkspaceSnapshot { revision: 9, document: old.document });
            let mut app = crate::DesktopApp::default();
            app.workspace_config = Some(DesktopConnectionConfig {
                source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint { endpoint, instance_token: token, daemon_instance_id: instance }),
                // Even an inherited old target must not be attached at startup.
                session_id: Some(live), request_log_pages: false,
            });
            app.initialize_workspace(&crate::workspace_connection::WorkspaceView { ready: true, snapshot, ..Default::default() });
            tokio::time::timeout(Duration::from_secs(5), async {
                while app.daemon.as_ref().unwrap().view().sessions.is_none() {
                    app.sync_workspace();
                    app.poll_primary_terminal(std::time::Instant::now());
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            assert!(app.workspace.document.bindings.is_empty());
            assert!(app.view_model.tabs.is_empty());
            assert!(app.view_model.sessions.is_empty(), "previous-run sessions must not appear in the opened list");
            assert!(app.current_workspace_tab.is_none());
            assert!(app.daemon.as_ref().unwrap().view().session_id.is_none());
            tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
            drop(app);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_last_tab_terminates_session_and_empty_window_can_remove_retained_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let first = SessionId::new();
        let retained = SessionId::new();
        let later = SessionId::new();
        let summary = |id: SessionId| SessionSummary {
            can_duplicate_ssh_channel: false,
            reconnecting: false,
            session_id: id.as_uuid().as_bytes().to_vec(), title: "Test session".into(),
            running: true, ..Default::default()
        };
        let sessions = Arc::new(Mutex::new(vec![summary(first), summary(retained)]));
        let server_sessions = sessions.clone();
        let closed = Arc::new(Mutex::new(Vec::new()));
        let server_closed = closed.clone();
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-close-tabs-{first}");
        #[cfg(unix)]
        let endpoint = directory.path().join("close-tabs.sock");
        let _directory = directory;
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        let token = [0x47; 32];
        let instance = [0x58; 16];
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    stream = listener.accept() => {
                        let mut stream = stream.unwrap();
                        let sessions = server_sessions.clone();
                        let closed = server_closed.clone();
                        clients.spawn(async move {
                            let policy = HandshakePolicy::with_instance_id(token, instance, features::TERMINAL_CONTROL);
                            if server_handshake(&mut stream, &policy).await.is_err() { return; }
                            while let Ok(request) = read_envelope(&mut stream).await {
                                let payload = match request.payload {
                                    Some(envelope::Payload::SessionCloseRequest(close)) => {
                                        assert!(!close.apply_view_policy, "tab close must terminate even a keep-running session");
                                        closed.lock().unwrap().push(close.session_id.clone());
                                        sessions.lock().unwrap().retain(|session| session.session_id != close.session_id);
                                        envelope::Payload::SessionCloseResponse(cshell_ipc::SessionCloseResponse {
                                            session_id: close.session_id,
                                        })
                                    }
                                    Some(envelope::Payload::SessionListRequest(_)) => envelope::Payload::SessionListResponse(SessionListResponse {
                                        sessions: sessions.lock().unwrap().clone(),
                                    }),
                                    other => panic!("unexpected request: {other:?}"),
                                };
                                if write_envelope(&mut stream, &Envelope { request_id: request.request_id, payload: Some(payload), ..Default::default() }).await.is_err() { break; }
                            }
                        });
                    }
                    result = clients.join_next(), if !clients.is_empty() => { result.unwrap().unwrap(); }
                }
            }
        });
        let config = DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint { endpoint, instance_token: token, daemon_instance_id: instance }),
            session_id: None, request_log_pages: false,
        };
        let mut app = crate::DesktopApp::default();
        app.workspace_loaded = true;
        app.workspace.writable = true;
        app.daemon_config = Some(config);
        // A session opened during this GUI run may remain without a tab after
        // explicit layout changes; it is still manageable in this run's list.
        app.opened_sessions.lock().unwrap().insert(retained);
        let tab = app.workspace.add(None, "Last tab".into()).unwrap();
        app.workspace.sessions.insert(tab, first);
        app.current_workspace_tab = Some(tab);
        // Close before a metadata handshake/list has completed.
        app.close_workspace_tab(tab);
        assert_eq!(app.closing_workspace_tab, Some(tab));
        tokio::time::timeout(Duration::from_secs(8), async {
            while app.workspace.sessions.contains_key(&tab) {
                app.sync_workspace();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert!(app.current_workspace_tab.is_none());
        assert!(app.workspace.document.bindings.is_empty());
        assert!(app.daemon.is_some(), "last-tab cleanup must retain metadata access");
        assert!(app.view_model.sessions.iter().any(|session| session.id == retained));
        // Reproduce a previous version's missing connection with no open tabs.
        app.daemon.take().unwrap().retire();
        app.view_model.terminate_target = Some(retained);
        app.view_model.confirm_terminate = true;
        app.request_termination(retained);
        assert!(app.view_model.terminate_pending);
        assert!(app.view_model.terminate_error.is_none());
        tokio::time::timeout(Duration::from_secs(8), async {
            while app.view_model.terminate_pending {
                app.sync_workspace();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert!(!app.view_model.confirm_terminate);
        assert!(app.view_model.sessions.is_empty());
        // The empty window must keep polling after a manual removal, too.
        sessions.lock().unwrap().push(summary(later));
        app.opened_sessions.lock().unwrap().insert(later);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !app.view_model.sessions.iter().any(|session| session.id == later) {
                app.sync_workspace();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert_eq!(*closed.lock().unwrap(), vec![first.as_uuid().as_bytes().to_vec(), retained.as_uuid().as_bytes().to_vec()]);
        drop(app);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_click_launch_survives_initial_idle_and_binds_the_original_tab() {
        use cshell_terminal::{ProbeTerminalEngine, TerminalEngine};
        let _directory = tempfile::tempdir().unwrap();
        let profile = ProfileId::new();
        let session = SessionId::new();
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-first-click-{profile}");
        #[cfg(unix)]
        let endpoint = _directory.path().join("first-click.sock");
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        let (accepted, accepted_rx) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let token = [0x39; 32];
        let instance = [0x61; 16];
        let creates = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = creates.clone();
        let server = tokio::spawn(async move {
            let policy = HandshakePolicy::with_instance_id(token, instance,
                features::TERMINAL_CONTROL | features::FULL_FRAME_RECOVERY | features::SSH_PROFILE_SESSION | features::LOCAL_PROFILE);
            let mut initial = listener.accept().await.unwrap();
            accepted.send(()).unwrap();
            released.await.unwrap();
            server_handshake(&mut initial, &policy).await.unwrap();
            let mut stream = listener.accept().await.unwrap();
            // The UI must keep its pending launch throughout both handshakes.
            tokio::time::sleep(Duration::from_millis(150)).await;
            server_handshake(&mut stream, &policy).await.unwrap();
            let summary = SessionSummary {
                can_duplicate_ssh_channel: false,
                reconnecting: false,
                session_id: session.as_uuid().as_bytes().to_vec(),
                profile_id: Some(profile.as_uuid().as_bytes().to_vec()),
                title: "One-click SSH".into(), running: true, ..Default::default()
            };
            let mut terminal = ProbeTerminalEngine::new(TerminalSize::cells(24, 80));
            terminal.feed(b"ONE_CLICK_CONNECTED");
            while let Ok(request) = read_envelope(&mut stream).await {
                let payload = match request.payload {
                    Some(envelope::Payload::SessionCreateRequest(create)) => {
                        assert_eq!(create.profile_id.as_deref(), Some(profile.as_uuid().as_bytes().as_slice()));
                        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        envelope::Payload::SessionCreateResponse(SessionCreateResponse {
                            session: Some(summary.clone()), ..Default::default()
                        })
                    }
                    Some(envelope::Payload::SessionListRequest(_)) => envelope::Payload::SessionListResponse(SessionListResponse { sessions: vec![summary.clone()] }),
                    Some(envelope::Payload::SnapshotRequest(_)) => envelope::Payload::FullFrame(cshell_ipc::FullFrame::from_terminal_snapshot(session, &terminal.snapshot())),
                    other => panic!("unexpected request: {other:?}"),
                };
                if write_envelope(&mut stream, &Envelope {
                    request_id: request.request_id, payload: Some(payload), ..Default::default()
                }).await.is_err() { break; }
            }
        });
        let config = DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint { endpoint, instance_token: token, daemon_instance_id: instance }),
            session_id: None, request_log_pages: false,
        };
        let connection = DesktopDaemonConnection::start_idle(config.clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), accepted_rx).await.unwrap().unwrap();
        let mut app = crate::DesktopApp::default();
        let tab = app.workspace.add(Some(profile), "Saved SSH".into()).unwrap();
        app.current_workspace_tab = Some(tab);
        app.workspace.pending_launch = Some(tab);
        app.workspace_loaded = true;
        app.workspace.writable = true;
        app.daemon_config = Some(config);
        assert!(connection.open_profile(profile));
        app.daemon = Some(connection);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            while app.workspace.sessions.get(&tab) != Some(&session) {
                app.sync_workspace();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        assert_eq!(app.current_workspace_tab, Some(tab));
        assert_eq!(app.workspace.document.bindings.len(), 1);
        assert_eq!(creates.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(app.daemon.as_ref().unwrap().view().connected);
        drop(app);
        tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
    }

    #[test]
    fn clipboard_connection_configuration_debug_hides_instance_token() {
        let resolved = ResolvedDesktopEndpoint {
            endpoint: "test-endpoint".into(),
            instance_token: [173; 32],
            daemon_instance_id: [1; 16],
        };
        let config = DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(resolved),
            session_id: None,
            request_log_pages: false,
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("173"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_termination_removes_retained_and_current_real_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path().join("journals"), 32).unwrap());
        let active = registry.spawn_local(&interactive_profile(), TerminalSize::cells(24, 80)).unwrap().session_id();
        let retained = registry.spawn_local(&interactive_profile(), TerminalSize::cells(24, 80)).unwrap().session_id();
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-close-test-{}-{active}", std::process::id());
        #[cfg(unix)]
        let endpoint = directory.path().join("close-test.sock");
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        let token = [0x71; 32];
        let instance = [0x42; 16];
        let server = Arc::new(SessionIpcServer::new(listener,
            HandshakePolicy::with_instance_id(token, instance,
                features::FULL_FRAME_RECOVERY | features::PRIORITY_STREAMS
                    | features::TERMINAL_CONTROL | features::SSH_SESSION_STATUS | features::LOCAL_LAUNCH_OPTIONS),
            SessionIpcService::new(Arc::clone(&registry))));
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move { server.run_until(receiver).await });
        let config = DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint { endpoint, instance_token: token, daemon_instance_id: instance }),
            session_id: Some(active), request_log_pages: false,
        };
        let connection = DesktopDaemonConnection::start(config.clone()).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !connection.view().connected || !connection.view().sessions.as_ref().is_some_and(|sessions| sessions.iter().any(|s| s.session_id == retained.as_uuid().as_bytes())) {
            assert!(tokio::time::Instant::now() < deadline, "session setup failed: {}", connection.view().detail);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(connection.terminate_confirmed_session(retained));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while connection.view().closed_session != Some(retained) {
            assert!(tokio::time::Instant::now() < deadline, "retained termination did not complete: {}", connection.view().detail);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(registry.list().unwrap().iter().all(|s| s.session_id != retained));
        assert!(registry.list().unwrap().iter().any(|s| s.session_id == active));
        assert!(connection.attach_session(active));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !connection.view().connected {
            assert!(tokio::time::Instant::now() < deadline, "active session did not reattach: {}", connection.view().detail);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut app = crate::DesktopApp::default();
        let tab = app.workspace.add(None, "Close while layout is paused".into()).unwrap();
        app.workspace.sessions.insert(tab, active);
        app.workspace.select(tab);
        app.current_workspace_tab = Some(tab);
        app.workspace.writable = false;
        app.workspace_loaded = true;
        app.workspace_connection = Some(Arc::new(crate::workspace_connection::WorkspaceConnection::paused_for_test()));
        app.daemon_config = Some(config);
        app.daemon = Some(connection);
        app.view_model.confirm_terminate = true;
        app.view_model.terminate_target = Some(active);
        let document = app.workspace.document.clone();
        let stale = SessionId::new();
        app.daemon.as_ref().unwrap().shared.lock().unwrap().sessions.as_mut().unwrap()
            .push(SessionSummary {
                can_duplicate_ssh_channel: false,
                reconnecting: false, session_id: stale.as_uuid().as_bytes().to_vec(), ..Default::default() });
        app.view_model.terminate_target = Some(stale);
        app.request_termination(stale);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while app.view_model.terminate_pending {
            app.sync_workspace();
            assert!(tokio::time::Instant::now() < deadline, "rejected termination never produced a result");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(app.view_model.confirm_terminate, "a failure must stay visible");
        assert!(app.view_model.terminate_error.is_some());
        assert!(registry.list().unwrap().iter().any(|session| session.session_id == active));
        tokio::time::timeout(Duration::from_secs(8), async {
            while !app.daemon.as_ref().unwrap().view().connected {
                app.sync_workspace();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        assert!(app.daemon.as_ref().unwrap().send_input(InputAction::Text("echo RECOVERED\r".into())),
            "failed removal must restore input to the unrelated current terminal");
        app.view_model.terminate_target = Some(active);
        app.request_termination(active);
        assert!(app.view_model.terminate_pending);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while app.view_model.terminate_pending {
            app.sync_workspace();
            assert!(tokio::time::Instant::now() < deadline, "current termination did not complete: {:?}", app.view_model.terminate_error);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!app.view_model.confirm_terminate);
        assert_eq!(app.workspace.document, document, "paused layout must not be changed by process termination");
        assert!(!app.workspace.sessions.contains_key(&tab));
        assert!(registry.list().unwrap().is_empty());
        assert!(app.daemon.is_some(), "paused cleanup must retain a metadata connection");
        assert_eq!(app.current_workspace_tab, Some(tab));
        assert!(app.view_model.sessions.is_empty(), "confirmed removal must immediately update the list");
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.daemon.as_ref().unwrap().view().sessions.is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        assert!(app.daemon.as_ref().unwrap().view().sessions.unwrap().is_empty());
        drop(app);
        shutdown.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server_task).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn termination_timeout_or_stale_ack_is_visible_and_never_replayed() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for acknowledge_without_removing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let target = SessionId::new();
            #[cfg(windows)]
            let endpoint = format!(r"\\.\pipe\cshell-close-timeout-{target}");
            #[cfg(unix)]
            let endpoint = directory.path().join("close-timeout.sock");
            let listener = transport::LocalListener::bind(&endpoint).unwrap();
            let token = [0x69; 32];
            let instance = [0x32; 16];
            let close_count = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&close_count);
            let server = tokio::spawn(async move {
                let mut clients = tokio::task::JoinSet::new();
                // Keep the pending pipe accept alive when an older client exits,
                // matching SessionIpcServer's cancellation-safe accept loop.
                let pending_accept = listener.accept();
                tokio::pin!(pending_accept);
                loop {
                    tokio::select! {
                        accepted = &mut pending_accept => {
                            let mut stream = accepted.unwrap();
                            pending_accept.set(listener.accept());
                            let count = Arc::clone(&count);
                            clients.spawn(async move {
                                // The idle view may retire a connection during handshake.
                                // A dropped client must not tear down this test's listener.
                                if server_handshake(&mut stream, &HandshakePolicy::with_instance_id(
                                    token, instance, features::TERMINAL_CONTROL,
                                )).await.is_err() { return; }
                                while let Ok(request) = read_envelope(&mut stream).await {
                                    let payload = match request.payload {
                                        Some(envelope::Payload::SessionListRequest(_)) => {
                                            envelope::Payload::SessionListResponse(SessionListResponse {
                                                sessions: vec![SessionSummary {
                                                    can_duplicate_ssh_channel: false,
                                                    reconnecting: false,
                                                    session_id: target.as_uuid().as_bytes().to_vec(),
                                                    running: true,
                                                    ..Default::default()
                                                }],
                                            })
                                        }
                                        Some(envelope::Payload::SessionCloseRequest(close)) => {
                                            assert_eq!(close.session_id, target.as_uuid().as_bytes());
                                            assert!(!close.apply_view_policy);
                                            count.fetch_add(1, Ordering::SeqCst);
                                            if !acknowledge_without_removing {
                                                std::future::pending::<()>().await;
                                            }
                                            envelope::Payload::SessionCloseResponse(cshell_ipc::SessionCloseResponse {
                                                session_id: close.session_id,
                                            })
                                        }
                                        unexpected => panic!("unexpected request: {unexpected:?}"),
                                    };
                                    if write_envelope(&mut stream, &Envelope {
                                        request_id: request.request_id,
                                        payload: Some(payload),
                                        ..Default::default()
                                    }).await.is_err() { break; }
                                }
                            });
                        }
                        joined = clients.join_next(), if !clients.is_empty() => {
                            joined.unwrap().unwrap();
                        }
                    }
                }
            });
            let connection = DesktopDaemonConnection::start_idle(DesktopConnectionConfig {
                source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint {
                    endpoint, instance_token: token, daemon_instance_id: instance,
                }),
                session_id: None, request_log_pages: false,
            }).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while connection.view().sessions.is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            assert!(connection.terminate_confirmed_session(target));
            tokio::time::timeout(Duration::from_secs(8), async {
                while connection.view().close_pending.is_some() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            assert!(connection.view().close_error.is_some());
            assert_eq!(connection.view().closed_session, None);
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert_eq!(close_count.load(Ordering::SeqCst), 1, "never replay an uncertain termination; {:?}", connection.view().close_error);
            assert!(connection.view().sessions.unwrap().iter()
                .any(|session| session.session_id == target.as_uuid().as_bytes()));
            drop(connection);
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            drop(directory);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn profile_action_survives_handshake_timeout_without_opening_a_default_shell() {
        let _directory = tempfile::tempdir().unwrap();
        let profile = ProfileId::new();
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-handshake-timeout-{profile}");
        #[cfg(unix)]
        let endpoint = _directory.path().join("handshake-timeout.sock");
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        let token = [0x27; 32];
        let instance = [0x56; 16];
        let server = tokio::spawn(async move {
            let policy = HandshakePolicy::with_instance_id(token, instance, u64::MAX);
            let mut initial = listener.accept().await.unwrap();
            server_handshake(&mut initial, &policy).await.unwrap();
            let list = read_envelope(&mut initial).await.unwrap();
            write_envelope(&mut initial, &Envelope {
                request_id: list.request_id,
                payload: Some(envelope::Payload::SessionListResponse(SessionListResponse { sessions: vec![] })),
                ..Default::default()
            }).await.unwrap();
            let mut stalled = listener.accept().await.unwrap();
            // Receive the client handshake but deliberately omit its reply.
            read_envelope(&mut stalled).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(7), read_envelope(&mut stalled)).await.unwrap().is_err());
            let mut retry = listener.accept().await.unwrap();
            server_handshake(&mut retry, &policy).await.unwrap();
            let request = read_envelope(&mut retry).await.unwrap();
            let Some(envelope::Payload::SessionCreateRequest(create)) = request.payload else {
                panic!("retry must preserve the requested profile rather than rediscovering another session");
            };
            assert_eq!(create.profile_id.as_deref(), Some(profile.as_uuid().as_bytes().as_slice()));
        });
        let connection = DesktopDaemonConnection::start_idle(DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint { endpoint, instance_token: token, daemon_instance_id: instance }),
            session_id: None, request_log_pages: false,
        }).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while connection.view().sessions.is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert!(connection.open_profile(profile));
        tokio::time::timeout(Duration::from_secs(12), server).await.unwrap().unwrap();
        drop(connection);
    }

    fn interactive_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "Desktop connection input probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/Q".to_owned(),
                "/K".to_owned(),
                "echo CSHELL_DESKTOP_READY".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "Desktop connection input probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-lc".to_owned(),
                "printf 'CSHELL_DESKTOP_READY\\n'; IFS= read -r command; eval $command; sleep 2"
                    .to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    fn snapshot_has_executed_line(snapshot: &cshell_terminal::FrameSnapshot, marker: &str) -> bool {
        (0..snapshot.rows).any(|row| {
            let text = snapshot.row(row).map_or_else(String::new, |cells| {
                cells
                    .iter()
                    .flat_map(cshell_terminal::Cell::characters)
                    .collect()
            });
            text.trim_start().starts_with(marker) && !text.contains("echo ")
        })
    }

    #[test]
    fn cancelled_history_cache_and_late_responses_cannot_reenter_a_new_view() {
        use super::{DesktopDaemonView, DesktopInputSender, RenderLogPage, RenderLogRow, RenderLogPageRequest, LogSourceId, update_log_page};
        let page = Arc::new(RenderLogPage { first_line_id: 1,
            source_id: LogSourceId(1), revision: 1, anchor_line_id: 10,
            rows: vec![RenderLogRow {
            soft_wrap: false,line_id: 10, text: "old command".into(), style_spans: Arc::from([]), truncated: false}].into(),
            total_line_count: 100, has_before: true, has_after: true,
        });
        let shared = Arc::new(Mutex::new(DesktopDaemonView::default()));
        let connection = DesktopDaemonConnection {
            input: DesktopInputSender::new(Arc::clone(&shared), tokio::sync::mpsc::channel(1).0),
            shared,
            shutdown: tokio::sync::watch::channel(false).0,
            log_requests: tokio::sync::watch::channel(None).0,
            history_search_requests: tokio::sync::watch::channel(None).0,
            resize_requests: tokio::sync::watch::channel(None).0,
            open_requests: tokio::sync::watch::channel(None).0,
            worker: None,
        };
        update_log_page(&connection.shared, Arc::clone(&page), true, 0);
        assert!(connection.view().log_page.is_some());
        connection.cancel_log_page();
        assert!(connection.view().log_page.is_none());
        update_log_page(&connection.shared, Arc::clone(&page), true, 0);
        assert!(connection.view().log_page.is_none(), "cancelled IPC response must be dropped");
        let request = RenderLogPageRequest {source_id: LogSourceId(1), follow_tail: true, anchor_line_id: 0, cell_offset: 0, rows_before: 128, rows_after: 0};
        assert!(connection.request_terminal_history_page(request));
        let old = connection.log_requests.borrow().unwrap();
        connection.cancel_log_page();
        assert!(connection.request_terminal_history_page(request));
        let new = connection.log_requests.borrow().unwrap();
        assert_ne!(old.epoch, new.epoch);
        update_log_page(&connection.shared, Arc::clone(&page), true, old.epoch);
        assert!(connection.view().log_page.is_none(), "same request in a new view must reject old response");
        update_log_page(&connection.shared, page, true, new.epoch);
        assert!(connection.view().log_page.is_some());
    }

    #[test]
    fn fixed_length_hex_configuration_is_strict() {
        assert_eq!(decode_hex::<2>("00fF"), Some([0, 255]));
        assert_eq!(decode_hex::<2>("00f"), None);
        assert_eq!(decode_hex::<2>("00xz"), None);
    }

    #[test]
    fn terminal_control_rejection_is_visible_without_marking_the_daemon_disconnected() {
        let session_id = SessionId::new();
        let shared = Arc::new(Mutex::new(super::DesktopDaemonView {
            connected: true,
            ..super::DesktopDaemonView::default()
        }));
        update_terminal_control(
            &shared,
            session_id,
            &TerminalControlResponse::new(
                session_id.as_uuid().as_bytes().to_vec(),
                TerminalControlStatus::Backpressure,
            ),
        )
        .unwrap_or_else(|error| panic!("control response must be accepted: {error}"));
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(view.connected);
        assert_eq!(
            view.control_error.as_deref(),
            Some("terminal control rejected: Backpressure")
        );
    }

    #[tokio::test]
    async fn desktop_worker_paste_executes_in_a_real_pty_and_updates_its_snapshot() {
        const READY_MARKER: &str = "CSHELL_DESKTOP_READY";
        const MARKER: &str = "CSHELL_DESKTOP_EXECUTED";
        // Native CI runners can spend several seconds starting the shell and
        // publishing its first PTY frame, especially on Intel macOS hosts.
        const PTY_E2E_TIMEOUT: Duration = Duration::from_secs(30);
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary runtime must be created: {error}"));
        let registry = Arc::new(
            LocalSessionRegistry::new(directory.path().join("journals"), 32)
                .unwrap_or_else(|error| panic!("session registry must be created: {error}")),
        );
        let attachment = registry
            .spawn_local(&interactive_profile(), TerminalSize::cells(24, 80))
            .unwrap_or_else(|error| panic!("interactive PTY must start: {error}"));
        let session_id = attachment.session_id();
        drop(attachment);

        #[cfg(windows)]
        let endpoint = format!(
            r"\\.\pipe\cshell-desktop-input-test-{}-{session_id}",
            std::process::id()
        );
        #[cfg(windows)]
        let listener = transport::LocalListener::bind(&endpoint)
            .unwrap_or_else(|error| panic!("test pipe must bind: {error}"));
        #[cfg(unix)]
        let endpoint = directory.path().join("desktop-input.sock");
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&endpoint)
            .unwrap_or_else(|error| panic!("test socket must bind: {error}"));

        let token = [0x71; 32];
        let daemon_instance_id = [0x42; 16];
        let supported_features = features::FULL_FRAME_RECOVERY
            | features::PRIORITY_STREAMS
            | features::LOG_PAGING
            | features::TERMINAL_SCROLLBACK
            | features::TERMINAL_CONTROL
            | features::HISTORY_SEARCH
            | features::SSH_SESSION_STATUS
            | features::LOCAL_LAUNCH_OPTIONS;
        let server = Arc::new(SessionIpcServer::new(
            listener,
            HandshakePolicy::with_instance_id(token, daemon_instance_id, supported_features),
            SessionIpcService::new(Arc::clone(&registry)),
        ));
        let (server_shutdown, server_shutdown_receiver) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run_until(server_shutdown_receiver).await })
        };
        let connection = DesktopDaemonConnection::start(DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint {
                endpoint: endpoint.clone(),
                instance_token: token,
                daemon_instance_id,
            }),
            session_id: Some(session_id),
            request_log_pages: true,
        })
        .unwrap_or_else(|error| panic!("desktop connection worker must start: {error}"));

        let connected_deadline = tokio::time::Instant::now() + PTY_E2E_TIMEOUT;
        while !connection.view().connected {
            assert!(
                tokio::time::Instant::now() < connected_deadline,
                "desktop worker did not connect: {}",
                connection.view().detail
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let ready_deadline = tokio::time::Instant::now() + PTY_E2E_TIMEOUT;
        loop {
            let view = connection.view();
            if view
                .snapshot
                .as_deref()
                .is_some_and(|snapshot| snapshot_has_executed_line(snapshot, READY_MARKER))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "desktop worker shell did not publish its readiness marker: {}",
                view.detail
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let second_attachment = registry
            .spawn_local(&interactive_profile(), TerminalSize::cells(18, 60))
            .unwrap_or_else(|e| panic!("second PTY: {e}"));
        let second_id = second_attachment.session_id();
        drop(second_attachment);
        let second = DesktopDaemonConnection::start(DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint {
                endpoint,
                instance_token: token,
                daemon_instance_id,
            }),
            session_id: Some(second_id),
            request_log_pages: false,
        })
        .unwrap_or_else(|e| panic!("second pane connection: {e}"));
        tokio::time::timeout(PTY_E2E_TIMEOUT, async {
            while !second
                .view()
                .snapshot
                .as_deref()
                .is_some_and(|s| snapshot_has_executed_line(s, READY_MARKER))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|e| panic!("second pane readiness: {e}"));
        assert_ne!(session_id, second_id);
        assert!(second.send_input(InputAction::Paste {
            text: "echo CSHELL_SECOND_PANE_ONLY\r".into(),
            bracketed: true
        }));
        assert!(second.request_resize(18, 60, 600, 360));
        tokio::time::timeout(PTY_E2E_TIMEOUT, async {
            while !second.view().snapshot.as_deref().is_some_and(|s| {
                s.rows == 18
                    && s.cols == 60
                    && snapshot_has_executed_line(s, "CSHELL_SECOND_PANE_ONLY")
            }) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|e| panic!("second pane output/resize: {e}"));
        assert!(
            !connection
                .view()
                .snapshot
                .as_deref()
                .is_some_and(|s| snapshot_has_executed_line(s, "CSHELL_SECOND_PANE_ONLY"))
        );
        assert!(connection.send_input(InputAction::Paste {
            text: format!("echo {MARKER}\r"),
            bracketed: true,
        }));

        let output_deadline = tokio::time::Instant::now() + PTY_E2E_TIMEOUT;
        loop {
            let view = connection.view();
            if view
                .snapshot
                .as_deref()
                .is_some_and(|snapshot| snapshot_has_executed_line(snapshot, MARKER))
            {
                assert!(view.control_error.is_none());
                break;
            }
            assert!(
                tokio::time::Instant::now() < output_deadline,
                "desktop worker did not publish the executed command output: {}",
                view.detail
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            !second
                .view()
                .snapshot
                .as_deref()
                .is_some_and(|s| snapshot_has_executed_line(s, MARKER))
        );
        second.retire();
        registry
            .close(second_id)
            .unwrap_or_else(|e| panic!("close second pane: {e}"));

        assert!(
            connection.request_history_search(DesktopHistorySearchRequest {
                revision: 77,
                query: MARKER.to_owned(),
                case_sensitive: true,
                whole_word: false,
                regex: false,
                direction: HistorySearchDirection::Backward,
                cursor: None,
            })
        );
        let search_deadline = tokio::time::Instant::now() + PTY_E2E_TIMEOUT;
        loop {
            let view = connection.view();
            if view.history_search.as_ref().is_some_and(|response| {
                response.revision == 77
                    && response
                        .result
                        .matches
                        .first()
                        .is_some_and(|matched| matched.line_id > 0)
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < search_deadline,
                "desktop history search did not find executed PTY output: {}",
                view.detail
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(connection.cancel_history_search());

        #[cfg(windows)]
        let connection = {
            // Exercise the real desktop connection, RAM paging service, async
            // reflow, and repeated wheel input across several page boundaries.
            assert!(connection.send_input(InputAction::Paste {
                text: "(for /L %i in (1,1,180) do @echo CSHELL_HISTORY_%i) & echo CSHELL_HISTORY_DONE\r".into(),
                bracketed: true,
            }));
            tokio::time::timeout(PTY_E2E_TIMEOUT, async {
                while !connection.view().snapshot.as_deref().is_some_and(|snapshot| {
                    snapshot_has_executed_line(snapshot, "CSHELL_HISTORY_DONE")
                }) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("long output must finish");
            assert!(!connection.view().snapshot.as_deref().is_some_and(|snapshot| {
                (0..snapshot.rows).any(|row| snapshot.row(row).is_some_and(|cells| {
                    cells.iter().flat_map(cshell_terminal::Cell::characters).collect::<String>().trim() == "CSHELL_HISTORY_1"
                }))
            }), "old output must have left the live viewport");
            let mut app = crate::DesktopApp::default();
            app.daemon = Some(connection);
            app.view_model.selected = Some(session_id);
            app.viewport_rows = 24;
            app.terminal_viewport = Some(cshell_render::TerminalViewport { x: 0, y: 0, width: 800, height: 480 });
            let mut first_top = None;
            let mut reached_old_output = false;
            for step in 0..80 {
                app.handle_terminal_wheel(winit::event::MouseScrollDelta::LineDelta(0.0, 1.0));
                for _ in 0..10 {
                    app.poll_primary_terminal(std::time::Instant::now());
                    while let Some(completed) = app.log_reflow.as_ref().and_then(|worker| worker.results.try_recv().ok()) {
                        app.apply_log_reflow(completed);
                    }
                    if let Some(surface) = &mut app.log_surface {
                        if let Some(request) = surface.request_reflow(80) {
                            assert!(app.log_reflow.as_ref().expect("history worker").submit(request, None));
                        }
                        if let Some(request) = surface.take_page_request(24) {
                            app.daemon.as_ref().unwrap().request_terminal_history_page(request);
                        }
                        if let Some(frame) = surface.prepare_frame(24) {
                            if first_top.is_none() && step == 0 {
                                first_top = Some(frame.page.rows[frame.visual_rows[frame.visible_rows.start + frame.first_viewport_row].logical_row_index as usize].line_id);
                            }
                            reached_old_output |= frame.visual_rows[frame.visible_rows.clone()].iter().any(|row| {
                                frame.page.rows[row.logical_row_index as usize].text.trim() == "CSHELL_HISTORY_1"
                            });
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if reached_old_output { break; }
            }
            assert!(first_top.is_some(), "the first wheel must load a history frame");
            assert!(reached_old_output, "repeated upward wheels must display output beyond the first history page");
            assert!(app.terminal_scrollback);
            for cycle in 0..3 {
                let marker = format!("CSHELL_NEW_OUTPUT_DONE_{cycle}");
                assert!(app.submit_terminal_action(InputAction::Paste {
                    text: format!("(for /L %i in (1,1,80) do @echo NEW_OUTPUT_%i) & echo {marker}\r"),
                    bracketed: true,
                }));
                assert!(app.log_surface.is_none());
                assert!(app.daemon.as_ref().unwrap().view().log_page.is_none());
                tokio::time::timeout(PTY_E2E_TIMEOUT, async {
                    while !app.daemon.as_ref().unwrap().view().snapshot.as_deref().is_some_and(|snapshot| {
                        snapshot_has_executed_line(snapshot, &marker)
                    }) { tokio::time::sleep(Duration::from_millis(10)).await; }
                }).await.expect("new command must finish");
                app.handle_terminal_wheel(winit::event::MouseScrollDelta::LineDelta(0.0, 1.0));
                let mut top_after_reflow = None;
                for _ in 0..100 {
                    app.poll_primary_terminal(std::time::Instant::now());
                    while let Some(completed) = app.log_reflow.as_ref().and_then(|worker| worker.results.try_recv().ok()) {
                        app.apply_log_reflow(completed);
                    }
                    let surface = app.log_surface.as_mut().unwrap();
                    if let Some(request) = surface.request_reflow(80) {
                        assert!(app.log_reflow.as_ref().unwrap().submit(request, None));
                    }
                    if let Some(request) = surface.take_page_request(24) {
                        app.daemon.as_ref().unwrap().request_terminal_history_page(request);
                    }
                    if let Some(frame) = surface.prepare_frame(24) {
                        let top = frame.page.rows[frame.visual_rows[frame.visible_rows.start + frame.first_viewport_row].logical_row_index as usize].line_id;
                        let expected = frame.page.total_line_count.saturating_sub(26);
                        assert!(top >= expected && top <= expected + 3,
                            "one wheel after command {cycle} jumped: top={top}, expected={expected}");
                        if matches!(surface.mode(), cshell_render::ScrollMode::Anchored { .. }) {
                            assert_eq!(top, expected, "one notch must move exactly three rows");
                            top_after_reflow = Some(top);
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(top_after_reflow.is_some());
            }
            app.return_to_live_terminal();
            assert!(app.log_surface.is_none());
            app.daemon.take().unwrap()
        };

        assert!(connection.close_current_view(false));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while connection.view().session_id.is_some() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{}",
                connection.view().detail
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!connection.view().connected);
        assert!(connection.view().snapshot.is_none());
        assert!(
            registry
                .session_info(session_id)
                .unwrap_or_else(|error| panic!("{error}"))
                .running
        );
        assert!(connection.view().sessions.as_ref().is_some_and(|sessions| {
            sessions
                .iter()
                .any(|item| item.session_id == session_id.as_uuid().as_bytes())
        }));
        assert!(!connection.send_input(InputAction::Text("must not send while detached".into())));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            connection.view().session_id.is_none(),
            "closed view was automatically reopened"
        );
        assert!(connection.attach_session(session_id));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !connection.view().connected {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{}",
                connection.view().detail
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(connection.view().session_id, Some(session_id));
        assert!(connection.view().snapshot.is_some());

        registry
            .close(session_id)
            .unwrap_or_else(|error| panic!("{error}"));
        let disconnected_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while connection.view().connected {
            assert!(
                tokio::time::Instant::now() < disconnected_deadline,
                "closed terminal still accepts input"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(connection.view().snapshot.is_some());
        assert!(!connection.send_input(InputAction::Paste {
            text: "must not replay".into(),
            bracketed: true
        }));
        assert!(
            registry
                .list()
                .unwrap_or_else(|error| panic!("{error}"))
                .is_empty()
        );

        drop(connection);
        server_shutdown
            .send(true)
            .unwrap_or_else(|_| panic!("server shutdown must be delivered"));
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap_or_else(|_| panic!("server must stop before timeout"))
            .unwrap_or_else(|error| panic!("server task must join: {error}"))
            .unwrap_or_else(|error| panic!("server must stop cleanly: {error}"));
    }

    #[test]
    fn discovery_is_resolved_again_after_daemon_restart() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary runtime must be created: {error}"));
        let paths = RuntimePaths::prepare(directory.path().join("runtime"))
            .unwrap_or_else(|error| panic!("runtime paths must be prepared: {error}"));
        let config = DesktopConnectionConfig {
            source: DesktopEndpointSource::Discovery(paths.clone()),
            session_id: None,
            request_log_pages: false,
        };
        let first = DiscoveryRecord::generate(&paths);
        let first_publication = DiscoveryPublication::publish(&paths, &first)
            .unwrap_or_else(|error| panic!("first discovery must publish: {error}"));
        assert_eq!(
            config
                .resolve()
                .unwrap_or_else(|error| panic!("first discovery must resolve: {error}"))
                .instance_token,
            first.instance_token
        );
        drop(first_publication);

        let second = DiscoveryRecord::generate(&paths);
        assert_ne!(first.instance_token, second.instance_token);
        let _second_publication = DiscoveryPublication::publish(&paths, &second)
            .unwrap_or_else(|error| panic!("second discovery must publish: {error}"));
        assert_eq!(
            config
                .resolve()
                .unwrap_or_else(|error| panic!("second discovery must resolve: {error}"))
                .instance_token,
            second.instance_token
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::unwrap_used)]
    async fn lifecycle_poll_preserves_a_fragmented_frame_and_disables_closed_session_input() {
        use cshell_terminal::{ProbeTerminalEngine, TerminalEngine};
        use tokio::io::AsyncWriteExt;

        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let profile_id = ProfileId::new();
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-fragmented-status-{session_id}");
        #[cfg(unix)]
        let endpoint = directory.path().join("fragmented-status.sock");
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        let token = [0x73; 32];
        let daemon_instance_id = [0x45; 16];
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id(
                    token,
                    daemon_instance_id,
                    features::FULL_FRAME_RECOVERY
                        | features::TERMINAL_CONTROL
                        | features::SSH_SESSION_STATUS,
                ),
            )
            .await
            .unwrap();
            let summary = |running| SessionSummary {
                can_duplicate_ssh_channel: false,
                reconnecting: false,
                session_id: session_id.as_uuid().as_bytes().to_vec(),
                profile_id: Some(profile_id.as_uuid().as_bytes().to_vec()),
                title: "saved SSH".into(),
                running,
                terminal_detail: if running {
                    String::new()
                } else {
                    "Remote shell exited with status 42".into()
                },
                ..SessionSummary::default()
            };
            let list = read_envelope(&mut stream).await.unwrap();
            assert!(matches!(
                list.payload,
                Some(envelope::Payload::SessionListRequest(_))
            ));
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id: list.request_id,
                    payload: Some(envelope::Payload::SessionListResponse(
                        SessionListResponse {
                            sessions: vec![summary(true)],
                        },
                    )),
                    ..Envelope::default()
                },
            )
            .await
            .unwrap();
            let snapshot_request = read_envelope(&mut stream).await.unwrap();
            assert!(matches!(
                snapshot_request.payload,
                Some(envelope::Payload::SnapshotRequest(_))
            ));
            let first_poll = read_envelope(&mut stream).await.unwrap();
            assert!(matches!(
                first_poll.payload,
                Some(envelope::Payload::SessionListRequest(_))
            ));
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id: first_poll.request_id,
                    payload: Some(envelope::Payload::SessionListResponse(
                        SessionListResponse {
                            sessions: vec![summary(true)],
                        },
                    )),
                    ..Envelope::default()
                },
            )
            .await
            .unwrap();
            let mut terminal = ProbeTerminalEngine::new(TerminalSize::cells(2, 20));
            terminal.feed(b"preserved output");
            let mut frame = Default::default();
            cshell_ipc::FrameCodec::default()
                .encode(
                    &Envelope {
                        request_id: snapshot_request.request_id,
                        payload: Some(envelope::Payload::FullFrame(
                            cshell_ipc::FullFrame::from_terminal_snapshot(
                                session_id,
                                &terminal.snapshot(),
                            ),
                        )),
                        ..Envelope::default()
                    },
                    &mut frame,
                )
                .unwrap();
            // The next poll must be writable while the frame reader owns a partial payload.
            stream.write_all(&frame[..7]).await.unwrap();
            let second_poll =
                tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut stream))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(matches!(
                second_poll.payload,
                Some(envelope::Payload::SessionListRequest(_))
            ));
            stream.write_all(&frame[7..]).await.unwrap();
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id: second_poll.request_id,
                    payload: Some(envelope::Payload::SessionListResponse(
                        SessionListResponse {
                            sessions: vec![summary(false)],
                        },
                    )),
                    ..Envelope::default()
                },
            )
            .await
            .unwrap();
            while let Ok(request) = read_envelope(&mut stream).await {
                assert!(matches!(
                    request.payload,
                    Some(envelope::Payload::SessionListRequest(_))
                ));
                write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: request.request_id,
                        payload: Some(envelope::Payload::SessionListResponse(
                            SessionListResponse {
                                sessions: vec![summary(false)],
                            },
                        )),
                        ..Envelope::default()
                    },
                )
                .await
                .unwrap();
            }
        });
        let connection = DesktopDaemonConnection::start(DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint {
                endpoint,
                instance_token: token,
                daemon_instance_id,
            }),
            session_id: Some(session_id),
            request_log_pages: false,
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let view = connection.view();
                if !view.connected && view.detail.contains("status 42") {
                    assert_eq!(view.session_profile_id, Some(profile_id));
                    assert!(view.snapshot.as_ref().is_some_and(|snapshot| {
                        snapshot
                            .cells
                            .iter()
                            .map(|cell| cell.character)
                            .collect::<String>()
                            .contains("preserved output")
                    }));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!connection.send_input(InputAction::Text("stale input".into())));
        drop(connection);
        server.await.unwrap();
        drop(directory);
    }

    #[tokio::test]
    async fn workspace_autosave_uses_native_ipc_and_stops_after_revision_conflict() {
        use cshell_application::{WorkspaceSnapshot, encode_workspace};
        use cshell_domain::{TabId, WorkspaceBinding, WorkspaceDocument};
        use cshell_ipc::{WorkspaceOperation, WorkspaceResponse, WorkspaceStatus};
        let token = [0x73; 32];
        let daemon_instance_id = [0x74; 16];
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-workspace-save-{}", std::process::id());
        #[cfg(unix)]
        let directory = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        let endpoint = directory.path().join("save.sock");
        #[cfg(windows)]
        let listener =
            transport::LocalListener::bind(endpoint.clone()).unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&endpoint).unwrap_or_else(|e| panic!("{e}"));
        let initial = WorkspaceDocument::default();
        let server_initial = initial.clone();
        let server = tokio::spawn(async move {
            for (operation, expected, status, revision) in [
                (WorkspaceOperation::Load, 0, WorkspaceStatus::Ok, 1),
                (WorkspaceOperation::Save, 1, WorkspaceStatus::Ok, 2),
                (WorkspaceOperation::Save, 2, WorkspaceStatus::Conflict, 0),
            ] {
                let mut stream = listener.accept().await.unwrap_or_else(|e| panic!("{e}"));
                server_handshake(
                    &mut stream,
                    &HandshakePolicy::with_instance_id(
                        token,
                        daemon_instance_id,
                        features::WORKSPACE_CONTROL,
                    ),
                )
                .await
                .unwrap_or_else(|e| panic!("{e}"));
                let request = read_envelope(&mut stream)
                    .await
                    .unwrap_or_else(|e| panic!("{e}"));
                let Some(envelope::Payload::WorkspaceRequest(workspace)) = request.payload else {
                    panic!("workspace request")
                };
                assert_eq!(workspace.operation, operation as i32);
                assert_eq!(workspace.expected_revision, expected);
                let json = if operation == WorkspaceOperation::Load {
                    encode_workspace(&server_initial).unwrap_or_else(|e| panic!("{e}"))
                } else if status == WorkspaceStatus::Ok {
                    workspace.document_json
                } else {
                    vec![]
                };
                write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: request.request_id,
                        payload: Some(envelope::Payload::WorkspaceResponse(WorkspaceResponse {
                            status: status as i32,
                            revision,
                            document_json: json,
                            detail: "competing editor".into(),
                        })),
                        ..Default::default()
                    },
                )
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(500), listener.accept())
                    .await
                    .is_err()
            );
        });
        let connection =
            crate::workspace_connection::WorkspaceConnection::start(DesktopConnectionConfig {
                source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint {
                    endpoint,
                    instance_token: token,
                    daemon_instance_id,
                }),
                session_id: None,
                request_log_pages: false,
            })
            .unwrap_or_else(|e| panic!("{e}"));
        tokio::time::timeout(Duration::from_secs(3), async {
            while !connection.view().ready {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            connection.view().snapshot,
            Some(WorkspaceSnapshot {
                revision: 1,
                document: initial.clone()
            })
        );
        let mut changed = initial;
        let tab = TabId::new();
        changed.tab_groups[0].tabs.push(tab);
        changed.tab_groups[0].active_tab = Some(tab);
        changed.bindings.push(WorkspaceBinding {
            tab_id: tab,
            profile_id: None,
            title: "saved".into(),
        });
        connection.save(&changed);
        tokio::time::timeout(Duration::from_secs(3), async {
            while connection.view().snapshot.as_ref().map(|s| s.revision) != Some(2) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        changed.bindings[0].title = "conflict".into();
        connection.save(&changed);
        tokio::time::timeout(Duration::from_secs(3), async {
            while connection.view().error.is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            connection
                .view()
                .error
                .is_some_and(|e| e.contains("autosave paused"))
        );
        changed.bindings[0].title = "must not replay".into();
        connection.save(&changed);
        server.await.unwrap_or_else(|e| panic!("{e}"));
        drop(connection);
    }

    #[tokio::test]
    async fn restored_idle_connection_lists_only_and_never_replays_a_lost_create_reply() {
        let token = [0x71; 32];
        let daemon_instance_id = [0x72; 16];
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-workspace-idle-{}", std::process::id());
        #[cfg(unix)]
        let directory = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        let endpoint = directory.path().join("idle.sock");
        #[cfg(windows)]
        let listener =
            transport::LocalListener::bind(endpoint.clone()).unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&endpoint).unwrap_or_else(|e| panic!("{e}"));
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap_or_else(|e| panic!("{e}"));
            server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id(token, daemon_instance_id, u64::MAX),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            for _ in 0..2 {
                let request = read_envelope(&mut stream)
                    .await
                    .unwrap_or_else(|e| panic!("{e}"));
                assert!(matches!(
                    request.payload,
                    Some(envelope::Payload::SessionListRequest(_))
                ));
                write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: request.request_id,
                        payload: Some(envelope::Payload::SessionListResponse(
                            SessionListResponse { sessions: vec![] },
                        )),
                        ..Default::default()
                    },
                )
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            }
            // Explicit activation closes the idle connection before opening the creation connection.
            assert!(read_envelope(&mut stream).await.is_err());
            drop(stream);
            let mut stream = listener.accept().await.unwrap_or_else(|e| panic!("{e}"));
            server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id(token, daemon_instance_id, u64::MAX),
            )
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            let request = read_envelope(&mut stream)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            assert!(matches!(
                request.payload,
                Some(envelope::Payload::SessionCreateRequest(_))
            ));
            // Losing the reply must park the client until another explicit action.
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(600), listener.accept())
                    .await
                    .is_err()
            );
        });
        let connection = DesktopDaemonConnection::start_idle(DesktopConnectionConfig {
            source: DesktopEndpointSource::Explicit(ResolvedDesktopEndpoint {
                endpoint,
                instance_token: token,
                daemon_instance_id,
            }),
            session_id: None,
            request_log_pages: false,
        })
        .unwrap_or_else(|e| panic!("{e}"));
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if connection.view().sessions.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(!connection.view().connected);
        assert!(connection.view().session_id.is_none());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(connection.new_default_shell());
        server.await.unwrap_or_else(|e| panic!("{e}"));
        assert!(!connection.view().connected);
        assert!(!connection.view().session_opening);
        assert!(connection.view().detail.contains("outcome is unknown"));
        drop(connection);
    }

    #[tokio::test]
    async fn empty_session_list_creates_a_default_terminal() {
        let (mut client, mut server) = tokio::io::duplex(16 * 1024);
        let session_id = SessionId::new();
        let server_task = tokio::spawn(async move {
            let request = cshell_ipc::read_envelope(&mut server)
                .await
                .unwrap_or_else(|error| panic!("list request must decode: {error}"));
            assert!(matches!(
                request.payload,
                Some(envelope::Payload::SessionListRequest(_))
            ));
            cshell_ipc::write_envelope(
                &mut server,
                &Envelope {
                    request_id: request.request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionListResponse(
                        SessionListResponse { sessions: vec![] },
                    )),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("list response must encode: {error}"));

            let request = cshell_ipc::read_envelope(&mut server)
                .await
                .unwrap_or_else(|error| panic!("create request must decode: {error}"));
            let Some(envelope::Payload::SessionCreateRequest(create)) = request.payload else {
                panic!("client must create a session after an empty list");
            };
            assert_eq!((create.rows, create.cols), (24, 80));
            cshell_ipc::write_envelope(
                &mut server,
                &Envelope {
                    request_id: request.request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionCreateResponse(
                        SessionCreateResponse {
                            session: Some(SessionSummary {
                                can_duplicate_ssh_channel: false,
                                reconnecting: false,
                                session_id: session_id.as_uuid().as_bytes().to_vec(),
                                title: "Local shell".to_owned(),
                                running: true,
                                generation: 1,
                                profile_id: None,
                                terminal_detail: String::new(),
                            }),
                            detail: String::new(),
                            failure_code: 0,
                        },
                    )),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("create response must encode: {error}"));
        });

        let mut request_id = 2;
        let discovered = discover_or_create_session(&mut client, &mut request_id, None, true)
            .await
            .unwrap_or_else(|error| panic!("session discovery must succeed: {error}"));
        assert_eq!(discovered, (session_id, "Local shell".to_owned()));
        assert_eq!(request_id, 4);
        server_task
            .await
            .unwrap_or_else(|error| panic!("fake daemon task must complete: {error}"));
    }

    #[tokio::test]
    async fn dedicated_log_connection_delivers_new_anchor_at_the_same_journal_revision() {
        let session_id = SessionId::new();
        let token = [0x5a; 32];
        let daemon_instance_id = [0x33; 16];
        #[cfg(windows)]
        let listener = transport::LocalListener::bind(format!(
            r"\\.\pipe\cshell-log-page-test-{}",
            std::process::id()
        ))
        .unwrap_or_else(|error| panic!("test pipe must bind: {error}"));
        #[cfg(windows)]
        let resolved = ResolvedDesktopEndpoint {
            endpoint: format!(r"\\.\pipe\cshell-log-page-test-{}", std::process::id()),
            instance_token: token,
            daemon_instance_id,
        };
        #[cfg(unix)]
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must be created: {error}"));
        #[cfg(unix)]
        let socket_path = directory.path().join("log-pages.sock");
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&socket_path)
            .unwrap_or_else(|error| panic!("test socket must bind: {error}"));
        #[cfg(unix)]
        let resolved = ResolvedDesktopEndpoint {
            endpoint: socket_path,
            instance_token: token,
            daemon_instance_id,
        };

        let server = tokio::spawn(async move {
            let mut stream = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("log client must connect: {error}"));
            server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id(token, daemon_instance_id, features::LOG_PAGING),
            )
            .await
            .unwrap_or_else(|error| panic!("log handshake must pass: {error}"));
            for expected_anchor in [10_u64, 20] {
                let request = read_envelope(&mut stream)
                    .await
                    .unwrap_or_else(|error| panic!("log request must decode: {error}"));
                let Some(envelope::Payload::LogPageRequest(page_request)) = request.payload else {
                    panic!("dedicated connection must carry log page requests");
                };
                assert_eq!(page_request.anchor_line_id, Some(expected_anchor));
                write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: request.request_id,
                        deadline_unix_ms: 0,
                        payload: Some(envelope::Payload::LogPage(LogPage { first_line_id: 1,
                            session_id: session_id.as_uuid().as_bytes().to_vec(),
                            revision: 77,
                            anchor_line_id: expected_anchor,
                            rows: vec![LogRow {
            soft_wrap: false,
                                line_id: expected_anchor,
                                text: format!("line {expected_anchor}"),
                                style_spans: vec![],
                                truncated: false,
                            }],
                            total_line_count: 100,
                            has_before: true,
                            has_after: true,
                        })),
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("log response must encode: {error}"));
            }
        });

        let first = DesktopLogPageRequest {
            epoch: 0,
            terminal_history: false,
            anchor_line_id: Some(10),
            cell_offset: 3,
            rows_before: 20,
            rows_after: 20,
        };
        let second = DesktopLogPageRequest {
            terminal_history: false,
            anchor_line_id: Some(20),
            ..first
        };
        let (request_sender, mut request_receiver) = tokio::sync::watch::channel(Some(first));
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let shared = Arc::new(Mutex::new(super::DesktopDaemonView::default()));
        let client_shared = Arc::clone(&shared);
        let client = tokio::spawn(async move {
            let mut delivery_revision = 0;
            log_page_loop(
                &resolved,
                session_id,
                &client_shared,
                shutdown_receiver,
                &mut request_receiver,
                &mut delivery_revision,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .log_page
                    .as_ref()
                    .is_some_and(|page| page.anchor_line_id == 10)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("first anchored page was not delivered"));
        request_sender
            .send(Some(second))
            .unwrap_or_else(|_| panic!("second page request receiver must remain open"));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .log_page
                    .as_ref()
                    .is_some_and(|page| page.anchor_line_id == 20 && page.revision == 2)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("second anchored page was not delivered"));
        let _result = shutdown_sender.send(true);
        client
            .await
            .unwrap_or_else(|error| panic!("log client task must join: {error}"))
            .unwrap_or_else(|error| panic!("log client must stop cleanly: {error}"));
        server
            .await
            .unwrap_or_else(|error| panic!("log server task must join: {error}"));
    }

    #[tokio::test]
    async fn dedicated_history_connection_supersede_and_cancel_drop_in_flight_reads() {
        let session_id = SessionId::new();
        let token = [0x6b; 32];
        let daemon_instance_id = [0x44; 16];
        #[cfg(windows)]
        let listener = transport::LocalListener::bind(format!(
            r"\\.\pipe\cshell-history-search-test-{}",
            std::process::id()
        ))
        .unwrap_or_else(|error| panic!("test pipe must bind: {error}"));
        #[cfg(windows)]
        let resolved = ResolvedDesktopEndpoint {
            endpoint: format!(
                r"\\.\pipe\cshell-history-search-test-{}",
                std::process::id()
            ),
            instance_token: token,
            daemon_instance_id,
        };
        #[cfg(unix)]
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must be created: {error}"));
        #[cfg(unix)]
        let socket_path = directory.path().join("history-search.sock");
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&socket_path)
            .unwrap_or_else(|error| panic!("test socket must bind: {error}"));
        #[cfg(unix)]
        let resolved = ResolvedDesktopEndpoint {
            endpoint: socket_path,
            instance_token: token,
            daemon_instance_id,
        };

        let (first_received, mut first_observed) = tokio::sync::watch::channel(false);
        let (cancel_request_received, mut cancel_request_observed) =
            tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            let mut old_stream = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("history client must connect: {error}"));
            server_handshake(
                &mut old_stream,
                &HandshakePolicy::with_instance_id(
                    token,
                    daemon_instance_id,
                    features::HISTORY_SEARCH,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("history handshake must pass: {error}"));
            let old_request = read_envelope(&mut old_stream)
                .await
                .unwrap_or_else(|error| panic!("old history request must decode: {error}"));
            let Some(envelope::Payload::HistorySearchRequest(old_search)) = old_request.payload
            else {
                panic!("dedicated connection must carry a history search request");
            };
            assert_eq!(old_search.query, "old");
            first_received
                .send(true)
                .unwrap_or_else(|_| panic!("test observer must remain available"));
            let mut new_stream = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("replacement history client must connect: {error}"));
            server_handshake(
                &mut new_stream,
                &HandshakePolicy::with_instance_id(
                    token,
                    daemon_instance_id,
                    features::HISTORY_SEARCH,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("replacement history handshake must pass: {error}"));
            let request = read_envelope(&mut new_stream)
                .await
                .unwrap_or_else(|error| panic!("new history request must decode: {error}"));
            let Some(envelope::Payload::HistorySearchRequest(search)) = request.payload else {
                panic!("replacement connection must carry a history search request");
            };
            assert_eq!(search.query, "new");
            write_envelope(
                &mut new_stream,
                &Envelope {
                    request_id: request.request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::HistorySearchResult(
                        HistorySearchResult {
                            session_id: session_id.as_uuid().as_bytes().to_vec(),
                            revision: 9,
                            matches: vec![HistorySearchMatch {
                                line_id: 20,
                                byte_start: 0,
                                byte_end: 3,
                            }],
                            scanned_lines: 1,
                            next_line_id: None,
                            next_byte_offset: None,
                            incomplete: false,
                        },
                    )),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("new history response must encode: {error}"));
            let cancel_request = read_envelope(&mut new_stream)
                .await
                .unwrap_or_else(|error| panic!("cancelled history request must decode: {error}"));
            let Some(envelope::Payload::HistorySearchRequest(cancel_search)) =
                cancel_request.payload
            else {
                panic!("dedicated connection must carry the cancellable history request");
            };
            assert_eq!(cancel_search.query, "cancel-me");
            cancel_request_received
                .send(true)
                .unwrap_or_else(|_| panic!("cancel request observer must remain available"));
            let read_result =
                tokio::time::timeout(Duration::from_secs(2), read_envelope(&mut new_stream))
                    .await
                    .unwrap_or_else(|_| {
                        panic!("cancellation did not close the active search connection")
                    });
            assert!(
                read_result.is_err(),
                "cancellation must close the active search connection"
            );
        });

        let request = |revision, query: &str| DesktopHistorySearchRequest {
            revision,
            query: query.to_owned(),
            case_sensitive: false,
            whole_word: false,
            regex: false,
            direction: HistorySearchDirection::Forward,
            cursor: None,
        };
        let (request_sender, mut request_receiver) =
            tokio::sync::watch::channel(Some(request(1, "old")));
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let shared = Arc::new(Mutex::new(super::DesktopDaemonView::default()));
        let client_shared = Arc::clone(&shared);
        let client = tokio::spawn(async move {
            serve_history_search_if_enabled(
                true,
                &resolved,
                session_id,
                &client_shared,
                shutdown_receiver,
                &mut request_receiver,
            )
            .await
        });

        tokio::time::timeout(
            Duration::from_secs(2),
            first_observed.wait_for(|seen| *seen),
        )
        .await
        .unwrap_or_else(|_| panic!("first history request was not observed"))
        .unwrap_or_else(|_| panic!("history request observer closed"));
        request_sender
            .send(Some(request(2, "new")))
            .unwrap_or_else(|_| panic!("history request receiver must remain open"));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .history_search
                    .as_ref()
                    .is_some_and(|response| {
                        response.revision == 2 && response.result.matches[0].line_id == 20
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("latest history result was not delivered"));
        request_sender
            .send(Some(request(3, "cancel-me")))
            .unwrap_or_else(|_| panic!("history request receiver must remain open"));
        tokio::time::timeout(
            Duration::from_secs(2),
            cancel_request_observed.wait_for(|seen| *seen),
        )
        .await
        .unwrap_or_else(|_| panic!("cancellable history request was not observed"))
        .unwrap_or_else(|_| panic!("cancel request observer closed"));
        request_sender
            .send(None)
            .unwrap_or_else(|_| panic!("history request receiver must remain open"));
        server
            .await
            .unwrap_or_else(|error| panic!("history server task must join: {error}"));
        let _result = shutdown_sender.send(true);
        client
            .await
            .unwrap_or_else(|error| panic!("history client task must join: {error}"))
            .unwrap_or_else(|error| panic!("history client must stop cleanly: {error}"));
    }

    #[test]
    fn validated_ipc_log_page_converts_to_zero_copy_render_page_shape() {
        let session_id = SessionId::new();
        let page = convert_log_page(
            LogPage { first_line_id: 1,
                session_id: session_id.as_uuid().as_bytes().to_vec(),
                revision: 41,
                anchor_line_id: 11,
                rows: vec![LogRow {
                    line_id: 11,
                    soft_wrap: false,
                    text: "错误 e\u{301}".to_owned(),
                    style_spans: vec![LogStyleSpan {
                        start: 0,
                        end: 6,
                        style: Some(TerminalStyle::from(Style {
                            foreground: Color::Indexed(196),
                            ..Style::default()
                        })),
                    }],
                    truncated: true,
                }],
                total_line_count: 20,
                has_before: true,
                has_after: true,
            },
            7,
        )
        .unwrap_or_else(|error| panic!("valid IPC page must convert: {error}"));
        assert_eq!(page.revision, 7);
        assert_eq!(page.rows[0].text.as_ref(), "错误 e\u{301}");
        assert_eq!(page.rows[0].style_spans[0].byte_range, 0..6);
        assert!(page.rows[0].truncated);
    }

    #[tokio::test]
    async fn saved_profile_create_sends_only_the_profile_reference() {
        let profile_id = ProfileId::new();
        let session_id = SessionId::new();
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_work = async {
            let request = read_envelope(&mut server)
                .await
                .unwrap_or_else(|error| panic!("{error}"));
            let Some(envelope::Payload::SessionCreateRequest(create)) = request.payload else {
                panic!("missing session create request");
            };
            assert_eq!(
                create.profile_id.as_deref(),
                Some(profile_id.as_uuid().as_bytes().as_slice())
            );
            assert_eq!((create.rows, create.cols), (30, 90));
            write_envelope(
                &mut server,
                &Envelope {
                    request_id: request.request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionCreateResponse(
                        SessionCreateResponse {
                            session: Some(SessionSummary {
                                can_duplicate_ssh_channel: false,
                                reconnecting: false,
                                session_id: session_id.as_uuid().as_bytes().to_vec(),
                                title: "saved SSH".into(),
                                running: true,
                                generation: 0,
                                profile_id: Some(profile_id.as_uuid().as_bytes().to_vec()),
                                terminal_detail: String::new(),
                            }),
                            detail: String::new(),
                            failure_code: 0,
                        },
                    )),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        };
        let mut request_id = 2;
        let (created, ()) = tokio::join!(
            create_saved_profile_session(&mut client, &mut request_id, profile_id, 30, 90, None),
            server_work,
        );
        assert_eq!(
            created.unwrap_or_else(|error| panic!("{error}")),
            (session_id, "saved SSH".into())
        );
        assert_eq!(request_id, 3);
    }

    #[tokio::test]
    async fn missing_previous_session_does_not_create_or_select_a_fallback_shell() {
        let previous = SessionId::new();
        let unrelated = SessionId::new();
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_work = async {
            let request = read_envelope(&mut server)
                .await
                .unwrap_or_else(|error| panic!("{error}"));
            write_envelope(
                &mut server,
                &Envelope {
                    request_id: request.request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionListResponse(
                        SessionListResponse {
                            sessions: vec![SessionSummary {
                                can_duplicate_ssh_channel: false,
                                reconnecting: false,
                                session_id: unrelated.as_uuid().as_bytes().to_vec(),
                                title: "unrelated".into(),
                                running: true,
                                ..SessionSummary::default()
                            }],
                        },
                    )),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        };
        let mut request_id = 2;
        let (result, ()) = tokio::join!(
            discover_or_create_session(&mut client, &mut request_id, Some(previous), false),
            server_work,
        );
        assert!(matches!(
            result,
            Err(super::DesktopConnectionError::SessionUnavailable)
        ));
    }
    #[test]
    fn running_local_session_displays_working_directory_fallback() {
        let id = SessionId::new();
        let shared =
            std::sync::Arc::new(std::sync::Mutex::new(super::DesktopDaemonView::default()));
        let warning =
            "Configured working directory is unavailable; using home directory: /home/test";
        super::update_session_lifecycle(
            &shared,
            id,
            &cshell_ipc::SessionListResponse {
                sessions: vec![cshell_ipc::SessionSummary {
                    can_duplicate_ssh_channel: false,
                    reconnecting: false,
                    session_id: id.as_uuid().as_bytes().to_vec(),
                    running: true,
                    terminal_detail: warning.into(),
                    ..cshell_ipc::SessionSummary::default()
                }],
            },
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(view.session_running);
        assert!(view.detail.contains(warning));
    }
}

#[cfg(test)]
mod channel_tests;
