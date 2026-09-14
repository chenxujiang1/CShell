use cshell_domain::{InputAction, SessionId, TerminalSize};
use cshell_ipc::{
    ClientFrameUpdate, DiscoveryRecord, Envelope, Handshake, HistorySearchCodecError,
    HistorySearchDirection, HistorySearchRequest as IpcHistorySearchRequest,
    HistorySearchResult as IpcHistorySearchResult, LogPage as IpcLogPage, LogPageCodecError,
    LogPageRequest as IpcLogPageRequest, RuntimePaths, SnapshotCodecError, SubscriptionClientError,
    TerminalControlCodecError, TerminalControlResponse, TerminalControlStatus,
    TerminalInputRequest, TerminalResizeRequest, TerminalSubscriptionReplica, client_handshake,
    envelope, features, read_envelope, transport, write_envelope,
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

#[derive(Clone, Debug)]
struct ResolvedDesktopEndpoint {
    #[cfg(windows)]
    endpoint: String,
    #[cfg(unix)]
    endpoint: PathBuf,
    instance_token: [u8; 32],
    daemon_instance_id: [u8; 16],
}

#[derive(Clone, Debug, Default)]
pub struct DesktopDaemonView {
    pub connected: bool,
    pub detail: String,
    pub snapshot: Option<Arc<FrameSnapshot>>,
    pub session_id: Option<SessionId>,
    pub session_title: Option<String>,
    pub log_page: Option<Arc<RenderLogPage>>,
    pub control_error: Option<String>,
    pub history_search: Option<Arc<DesktopHistorySearchResponse>>,
}

#[derive(Debug)]
pub struct DesktopDaemonConnection {
    shared: Arc<Mutex<DesktopDaemonView>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    log_requests: tokio::sync::watch::Sender<Option<DesktopLogPageRequest>>,
    history_search_requests: tokio::sync::watch::Sender<Option<DesktopHistorySearchRequest>>,
    input_requests: tokio::sync::mpsc::Sender<InputAction>,
    resize_requests: tokio::sync::watch::Sender<Option<TerminalSize>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DesktopLogPageRequest {
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

struct DesktopRequestReceivers<'a> {
    log_pages: &'a mut tokio::sync::watch::Receiver<Option<DesktopLogPageRequest>>,
    history_search: &'a mut tokio::sync::watch::Receiver<Option<DesktopHistorySearchRequest>>,
    input: &'a mut tokio::sync::mpsc::Receiver<InputAction>,
    resize: &'a mut tokio::sync::watch::Receiver<Option<TerminalSize>>,
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
    #[error("daemon IPC transport failed: {0}")]
    Ipc(#[from] cshell_ipc::IpcError),
    #[error("terminal subscription failed: {0}")]
    Subscription(#[from] SubscriptionClientError),
    #[error("daemon returned an unexpected session control response")]
    UnexpectedControlResponse,
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

    fn resolve(&self) -> Result<ResolvedDesktopEndpoint, DesktopConnectionError> {
        match &self.source {
            DesktopEndpointSource::Explicit(endpoint) => Ok(endpoint.clone()),
            DesktopEndpointSource::Discovery(paths) => {
                let record = DiscoveryRecord::load(paths)?;
                Ok(ResolvedDesktopEndpoint {
                    #[cfg(windows)]
                    endpoint: record.endpoint.to_string_lossy().into_owned(),
                    #[cfg(unix)]
                    endpoint: PathBuf::from(record.endpoint),
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

    fn start_companion_daemon(&self) -> Result<(), DesktopConnectionError> {
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
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, DesktopConnectionError> {
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
                        worker_log_requests,
                        worker_history_search_requests,
                        worker_input_requests,
                        worker_resize_requests,
                    )),
                    Err(error) => update_view(&worker_shared, false, error.to_string(), None),
                }
            })
            .map_err(DesktopConnectionError::Thread)?;
        Ok(Self {
            shared,
            shutdown,
            log_requests,
            history_search_requests,
            input_requests,
            resize_requests,
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

    pub fn request_log_page(&self, request: RenderLogPageRequest) -> bool {
        let request = DesktopLogPageRequest {
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
        if !self.view().connected {
            return false;
        }
        self.input_requests.try_send(action).is_ok()
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
    mut log_requests: tokio::sync::watch::Receiver<Option<DesktopLogPageRequest>>,
    mut history_search_requests: tokio::sync::watch::Receiver<Option<DesktopHistorySearchRequest>>,
    mut input_requests: tokio::sync::mpsc::Receiver<InputAction>,
    mut resize_requests: tokio::sync::watch::Receiver<Option<TerminalSize>>,
) {
    let delays = [100_u64, 250, 500, 1_000, 2_000, 5_000];
    let mut attempt = 0_usize;
    let mut last_daemon_start = None;
    let mut log_delivery_revision = 0_u64;
    while !*shutdown.borrow() {
        update_view(
            &shared,
            false,
            format!("daemon connecting (attempt {})", attempt + 1),
            None,
        );
        let result = connect_once(
            &config,
            &shared,
            &mut shutdown,
            &mut log_delivery_revision,
            DesktopRequestReceivers {
                log_pages: &mut log_requests,
                history_search: &mut history_search_requests,
                input: &mut input_requests,
                resize: &mut resize_requests,
            },
        )
        .await;
        if *shutdown.borrow() {
            return;
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
    requests: DesktopRequestReceivers<'_>,
) -> Result<(), DesktopConnectionError> {
    while requests.input.try_recv().is_ok() {}
    let resolved = config.resolve()?;
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
    handshake.feature_bits = features::FULL_FRAME_RECOVERY
        | features::PRIORITY_STREAMS
        | features::LOG_PAGING
        | features::TERMINAL_CONTROL;
    let negotiated = client_handshake(&mut stream, 1, handshake).await?;
    if config.request_log_pages && negotiated.feature_bits & features::LOG_PAGING == 0 {
        return Err(DesktopConnectionError::LogPagingNotNegotiated);
    }
    if negotiated.feature_bits & features::TERMINAL_CONTROL == 0 {
        return Err(DesktopConnectionError::TerminalControlNotNegotiated);
    }

    let mut request_id = 2_u64;
    let (session_id, session_title) = match config.session_id {
        Some(session_id) => (session_id, "Terminal".to_owned()),
        None => discover_or_create_session(&mut stream, &mut request_id).await?,
    };
    update_session(shared, session_id, session_title);
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
    loop {
        let envelope = tokio::select! {
            envelope = read_envelope(&mut stream) => envelope?,
            result = &mut log_pages => return result,
            result = &mut history_search => return result,
            Some(action) = requests.input.recv() => {
                write_input_request(&mut stream, request_id, session_id, &action).await?;
                request_id = request_id.saturating_add(1);
                continue;
            }
            changed = requests.resize.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                if let Some(size) = *requests.resize.borrow_and_update() {
                    write_resize_request(&mut stream, request_id, session_id, size).await?;
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
        };
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
                write_snapshot_request(&mut stream, request_id, request).await?;
                request_id = request_id.saturating_add(1);
            }
        }
    }
}

async fn discover_or_create_session<S>(
    stream: &mut S,
    request_id: &mut u64,
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
    let session = list
        .sessions
        .iter()
        .find(|session| session.running)
        .cloned()
        .or_else(|| list.sessions.into_iter().next());
    let session = match session {
        Some(session) => session,
        None => {
            let create_request_id = *request_id;
            write_envelope(
                stream,
                &Envelope {
                    request_id: create_request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionCreateRequest(
                        cshell_ipc::SessionCreateRequest { rows: 24, cols: 80 },
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
        if let Err(error) = result {
            tracing::warn!(%error, "dedicated history search connection failed; retrying");
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

async fn history_search_loop(
    resolved: &ResolvedDesktopEndpoint,
    session_id: SessionId,
    shared: &Arc<Mutex<DesktopDaemonView>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    requests: &mut tokio::sync::watch::Receiver<Option<DesktopHistorySearchRequest>>,
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
                        return Ok(());
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        };
        write_history_search_request(&mut stream, request_id, session_id, &desired).await?;
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
            .is_some_and(|current| current.revision == desired.revision)
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
    handshake.feature_bits = features::LOG_PAGING;
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
        update_log_page(shared, convert_log_page(page, *delivery_revision)?);
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
        }
    }
}

impl DesktopLogPageRequest {
    const fn initial_tail() -> Self {
        Self {
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
) -> Result<(), DesktopConnectionError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let request = TerminalInputRequest::from_action(session_id, action)?;
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
                line_id: row.line_id,
                text: Arc::from(row.text),
                style_spans: style_spans.into(),
                truncated: row.truncated,
            })
        })
        .collect::<Result<Vec<_>, DesktopConnectionError>>()?;
    Ok(Arc::new(RenderLogPage {
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
    view.connected = connected;
    if !connected {
        view.control_error = None;
    }
    view.detail = view
        .control_error
        .as_ref()
        .map_or(detail.clone(), |error| format!("{detail} · {error}"));
    if let Some(snapshot) = snapshot {
        view.snapshot = Some(Arc::new(snapshot));
    }
}

fn update_log_page(shared: &Arc<Mutex<DesktopDaemonView>>, page: Arc<RenderLogPage>) {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .log_page = Some(page);
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
    view.session_id = Some(id);
    view.session_title = Some(title);
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
    use super::{
        DesktopConnectionConfig, DesktopDaemonConnection, DesktopEndpointSource,
        DesktopHistorySearchRequest, DesktopLogPageRequest, ResolvedDesktopEndpoint,
        convert_log_page, decode_hex, discover_or_create_session, history_search_loop,
        log_page_loop, update_terminal_control,
    };
    use cshell_domain::{InputAction, SessionId, TerminalSize};
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
    async fn desktop_worker_input_executes_in_a_real_pty_and_updates_its_snapshot() {
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
            | features::TERMINAL_CONTROL
            | features::HISTORY_SEARCH;
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
                endpoint,
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
        assert!(connection.send_input(InputAction::Text(format!("echo {MARKER}\r"))));

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

        drop(connection);
        server_shutdown
            .send(true)
            .unwrap_or_else(|_| panic!("server shutdown must be delivered"));
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap_or_else(|_| panic!("server must stop before timeout"))
            .unwrap_or_else(|error| panic!("server task must join: {error}"))
            .unwrap_or_else(|error| panic!("server must stop cleanly: {error}"));
        registry
            .close(session_id)
            .unwrap_or_else(|error| panic!("PTY session must close: {error}"));
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
                                session_id: session_id.as_uuid().as_bytes().to_vec(),
                                title: "Local shell".to_owned(),
                                running: true,
                                generation: 1,
                            }),
                        },
                    )),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("create response must encode: {error}"));
        });

        let mut request_id = 2;
        let discovered = discover_or_create_session(&mut client, &mut request_id)
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
                        payload: Some(envelope::Payload::LogPage(LogPage {
                            session_id: session_id.as_uuid().as_bytes().to_vec(),
                            revision: 77,
                            anchor_line_id: expected_anchor,
                            rows: vec![LogRow {
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
            anchor_line_id: Some(10),
            cell_offset: 3,
            rows_before: 20,
            rows_after: 20,
        };
        let second = DesktopLogPageRequest {
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
    async fn dedicated_history_connection_drops_a_superseded_response() {
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
        let (release_first, mut first_release) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            let mut stream = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("history client must connect: {error}"));
            server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id(
                    token,
                    daemon_instance_id,
                    features::HISTORY_SEARCH,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("history handshake must pass: {error}"));
            for (query, line_id) in [("old", 10_u64), ("new", 20_u64)] {
                let request = read_envelope(&mut stream)
                    .await
                    .unwrap_or_else(|error| panic!("history request must decode: {error}"));
                let Some(envelope::Payload::HistorySearchRequest(search)) = request.payload else {
                    panic!("dedicated connection must carry history search requests");
                };
                assert_eq!(search.query, query);
                if query == "old" {
                    first_received
                        .send(true)
                        .unwrap_or_else(|_| panic!("test observer must remain available"));
                    first_release
                        .wait_for(|released| *released)
                        .await
                        .unwrap_or_else(|_| panic!("first response release must remain available"));
                }
                write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: request.request_id,
                        deadline_unix_ms: 0,
                        payload: Some(envelope::Payload::HistorySearchResult(
                            HistorySearchResult {
                                session_id: session_id.as_uuid().as_bytes().to_vec(),
                                revision: 9,
                                matches: vec![HistorySearchMatch {
                                    line_id,
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
                .unwrap_or_else(|error| panic!("history response must encode: {error}"));
            }
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
            history_search_loop(
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
        release_first
            .send(true)
            .unwrap_or_else(|_| panic!("history server must remain available"));
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
        let _result = shutdown_sender.send(true);
        client
            .await
            .unwrap_or_else(|error| panic!("history client task must join: {error}"))
            .unwrap_or_else(|error| panic!("history client must stop cleanly: {error}"));
        server
            .await
            .unwrap_or_else(|error| panic!("history server task must join: {error}"));
    }

    #[test]
    fn validated_ipc_log_page_converts_to_zero_copy_render_page_shape() {
        let session_id = SessionId::new();
        let page = convert_log_page(
            LogPage {
                session_id: session_id.as_uuid().as_bytes().to_vec(),
                revision: 41,
                anchor_line_id: 11,
                rows: vec![LogRow {
                    line_id: 11,
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
}
