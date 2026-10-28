mod recovery;
mod directory;
#[cfg(test)]
mod recovery_tests;
use crate::{LatestSnapshot, PipelineIngress, TerminalFrameSubscription, TerminalPipeline};
use cshell_domain::{InputAction, ProfileId, SessionId, TerminalSize};
use cshell_ipc::{FullFrame, SessionSummary};
use cshell_output_store::JournalLineIndex;
use cshell_ssh::{KnownHostsVerifier, SshConnection, SshError, SshTerminalWriter, TerminalEvent};
use cshell_terminal::{InputEncoder, TerminalModes};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SshSessionError {
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error(transparent)]
    Pipeline(#[from] crate::PipelineError),
    #[error(transparent)]
    Input(#[from] cshell_terminal::InputEncodeError),
    #[error("SSH peer identity is unavailable")]
    PeerIdentity,
    #[error("{0}")]
    ChannelReuse(&'static str),
    #[error("SSH terminal pipeline is closed")]
    Closed,
    #[error("SSH terminal input queue is full")]
    Backpressure,
    #[error("SSH session {0} does not exist")]
    Unknown(SessionId),
}

pub struct SshConnect {
    pub username: String,
    pub verifier: KnownHostsVerifier,
    pub authentication: SshAuthentication,
}

pub use cshell_ssh::SshAuthentication;

impl std::fmt::Debug for SshConnect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshConnect")
            .field("username", &self.username)
            .field("verifier", &self.verifier)
            .field("authentication", &self.authentication)
            .finish()
    }
}

enum Command {
    Input(u64, u64, Vec<u8>),
    Resize(TerminalSize),
}

#[derive(Debug)]
pub struct SshSession {
    directory: Arc<Mutex<directory::DirectoryTracker>>,
    zmodem: Arc<Mutex<crate::zmodem::Transfer>>,
    active: Arc<RwLock<recovery::ActiveTransport>>,
    reconnecting: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    connection_epoch: Arc<std::sync::atomic::AtomicU64>,
    input_generation: Arc<std::sync::atomic::AtomicU64>,
    size: Arc<Mutex<TerminalSize>>,
    id: SessionId,
    title: String,
    terminal_type: String,
    logging: bool,
    profile_id: Option<ProfileId>,
    terminal_detail: Arc<Mutex<String>>,
    pipeline: Mutex<Option<TerminalPipeline>>,
    ingress: Mutex<Option<PipelineIngress>>,
    snapshots: LatestSnapshot,
    clipboard: crate::clipboard::ClipboardInbox,
    clipboard_host: Option<cshell_domain::ClipboardHost>,
    clipboard_trust: Option<cshell_domain::ClipboardTrust>,
    line_index: JournalLineIndex,
    commands: tokio::sync::mpsc::Sender<Command>,
    closed: Arc<AtomicBool>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SshSession {
    pub(crate) fn history_snapshots(&self)->LatestSnapshot {self.snapshots.clone()}
    pub(crate) fn zmodem_status(&self) -> Option<cshell_ipc::ZmodemTransfer> {
        self.zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner).status.clone().map(|mut s| {
            s.session_id = self.id.as_uuid().as_bytes().to_vec();
            s.profile_id = self.profile_id.map(|id| id.as_uuid().as_bytes().to_vec());
            s.session_title = self.title.clone();
            s
        })
    }
    pub(crate) fn zmodem_control(&self, request: &cshell_ipc::ZmodemRequest) -> Result<(), String> {
        if !self.running() { return Err("SSH 连接已断开".into()); }
        self.zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner).control(request)
    }
    pub(crate) async fn start_tunnel(
        &self,
        definition: &cshell_application::TunnelDefinition,
    ) -> Result<cshell_ssh::ForwardHandle, SshError> {
        crate::tunnel_ipc::start_forward(&self.active_transport().client, definition).await
    }
    pub(crate) async fn cancel_remote_tunnel(
        &self,
        address: &str,
        port: u16,
    ) -> Result<(), SshError> {
        self.active_transport().client.cancel_remote_forward(address, port).await
    }
    pub async fn connect(
        id: SessionId,
        title: String,
        connect: SshConnect,
        size: TerminalSize,
        journal_path: &Path,
        ingress_capacity: usize,
    ) -> Result<Self, SshSessionError> {
        let client = SshConnection::connect_known_hosts(
            connect.username,
            connect.authentication,
            connect.verifier,
        )
        .await?;
        Self::from_connection(
            id,
            title,
            crate::ssh_route::RoutedSshConnection { client, jump: None },
            size,
            journal_path,
            ingress_capacity,
        )
        .await
    }

    pub(crate) async fn from_connection(
        id: SessionId,
        title: String,
        connection: crate::ssh_route::RoutedSshConnection,
        size: TerminalSize,
        journal_path: &Path,
        ingress_capacity: usize,
    ) -> Result<Self, SshSessionError> {
        Self::from_connection_configured(
            id,
            title,
            connection,
            size,
            journal_path,
            ingress_capacity,
            "xterm-256color",
            true,
            None,
        )
        .await
    }

    pub(crate) async fn from_connection_configured(
        id: SessionId,
        title: String,
        connection: crate::ssh_route::RoutedSshConnection,
        size: TerminalSize,
        journal_path: &Path,
        ingress_capacity: usize,
        terminal_type: &str,
        logging: bool,
        recovery_config: Option<(crate::ssh_route::SshConnectionPlan, std::path::PathBuf, Arc<crate::vault_control::VaultController>)>,
    ) -> Result<Self, SshSessionError> {
        Self::from_transport_configured(id, title, crate::shared_ssh::TransportLease::new(connection), size, journal_path, ingress_capacity, terminal_type, logging, recovery_config).await
    }
    async fn from_transport_configured(
        id: SessionId, title: String, lease: crate::shared_ssh::TransportLease,
        size: TerminalSize, journal_path: &Path, ingress_capacity: usize,
        terminal_type: &str, logging: bool,
        recovery_config: Option<(crate::ssh_route::SshConnectionPlan, std::path::PathBuf, Arc<crate::vault_control::VaultController>)>,
    ) -> Result<Self, SshSessionError> {
        let client = lease.client();
        let jump = lease.jump();
        let terminal = client
            .open_terminal_tracked(u32::from(size.rows), u32::from(size.cols), terminal_type, Some(&id.to_string()))
            .await?;
        let (mut reader, writer) = terminal.split();
        let writer = Arc::new(writer);
        lease.register_writer(writer.clone());
        let clipboard = crate::clipboard::ClipboardInbox::default();
        let pipeline = TerminalPipeline::spawn_configured(
            journal_path,
            size,
            ingress_capacity,
            Some(clipboard.clone()),
            logging,
        )?;
        let ingress = pipeline.ingress().ok_or(SshSessionError::Closed)?;
        let snapshots = pipeline.snapshots();
        let line_index = pipeline.line_index();
        let responses = pipeline.responses();
        let closed = Arc::new(AtomicBool::new(false));
        let terminal_detail = Arc::new(Mutex::new(if logging {
            String::new()
        } else {
            "Output recording is disabled for this session".into()
        }));
        let active = Arc::new(RwLock::new(recovery::ActiveTransport {
            lease, client: Arc::clone(&client), jump: jump.clone(), writer: Arc::clone(&writer) }));
        let recovery_plan = recovery_config.map(|(plan, known_hosts, vault)| {
            let plan=plan.authenticated_plan(&client,jump.as_ref());
            let peer = plan.verified_peer(&client, jump.as_ref()).map_err(|_| SshSessionError::PeerIdentity)?;
            Ok::<_, SshSessionError>(recovery::RecoveryPlan { plan, known_hosts, vault, peer, terminal_type: terminal_type.to_owned(), directory_marker:id.to_string() })
        }).transpose()?;
        let reconnecting = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let connection_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let input_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let size = Arc::new(Mutex::new(size));
        let reader_detail = Arc::clone(&terminal_detail);
        let (commands, mut receiver) = tokio::sync::mpsc::channel(256);
        let zmodem = Arc::new(Mutex::new(crate::zmodem::Transfer::default()));
        let directory=Arc::new(Mutex::new(directory::DirectoryTracker::default()));
        let reader_directory=directory.clone();
        let reader_zmodem = Arc::clone(&zmodem);
        let reader_ingress = ingress.clone();
        let reader_closed = Arc::clone(&closed);
        let reader_active = Arc::clone(&active);
        let reader_recovery = recovery_plan;
        let reader_reconnecting = Arc::clone(&reconnecting);
        let reader_stopped = Arc::clone(&stopped);
        let reader_epoch = Arc::clone(&connection_epoch);
        let reader_input_generation = Arc::clone(&input_generation);
        let reader_size = Arc::clone(&size);
        let reader_clipboard = clipboard.clone();
        let io_finished = Arc::new(AtomicBool::new(false));
        let reader_finished = Arc::clone(&io_finished);
        let reader_task = tokio::spawn(async move {
            let mut attempts_used = 0;
            let mut connected_since = std::time::Instant::now();
            loop {
                let mut unexpected = true;
                while let Some(event) = reader.next_event().await {
                    match event {
                        TerminalEvent::Data { data, .. } => {
                            let ingress = reader_ingress.clone();
                            let transfer = Arc::clone(&reader_zmodem);
                            let directory=reader_directory.clone();
                            if !matches!(tokio::task::spawn_blocking(move || {
                                let data = transfer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).incoming(&data);
                                directory.lock().unwrap_or_else(std::sync::PoisonError::into_inner).feed(&data);
                                if data.is_empty() { Ok(()) } else { ingress.submit_blocking(data) }
                            }).await, Ok(Ok(()))) {
                                replace_terminal_detail(&reader_detail, "SSH output storage failed; the terminal is disconnected".into());
                                unexpected = false;
                                break;
                            }
                            // Let SSH's channel window provide backpressure when
                            // disk I/O is slower than the network. Do not abort
                            // healthy LAN downloads merely because they are fast.
                            while reader_zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner).buffered_bytes() >= 256 * 1024 {
                                if reader_stopped.load(Ordering::Acquire) || reader_closed.load(Ordering::Acquire) { break; }
                                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                            }
                        }
                        TerminalEvent::ExitStatus(code) => {
                            unexpected = false;
                            replace_terminal_detail(&reader_detail, format!("Remote shell exited with status {code}"));
                        }
                        TerminalEvent::ExitSignal { .. } => {
                            unexpected = false;
                            replace_terminal_detail(&reader_detail, "Remote shell exited due to a signal".into());
                        }
                        TerminalEvent::Closed => break,
                        _ => {}
                    }
                }
                reader_directory.lock().unwrap_or_else(std::sync::PoisonError::into_inner).reset();
                reader_closed.store(true, Ordering::Release);
                reader_zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner).disconnected();
                reader_epoch.fetch_add(1, Ordering::AcqRel);
                reader_clipboard.handle(cshell_ipc::ClipboardOperation::Poll, &[], false);
                let old = reader_active.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                old.lease.release().await;
                if !unexpected || reader_stopped.load(Ordering::Acquire) { break; }
                replace_terminal_detail(&reader_detail, "SSH disconnected without an exit status; remote process outcome is unknown".into());
                let plan = reader_recovery.clone();
                let Some(plan) = plan.filter(|p| p.plan.target.options.auto_reconnect) else { break; };
                if connected_since.elapsed() >= std::time::Duration::from_secs(30) { attempts_used = 0; }
                reader_reconnecting.store(true, Ordering::Release);
                let restored = recovery::restore(&plan, &reader_active, &reader_size, &reader_ingress, &reader_detail, &reader_closed, &reader_epoch, &reader_input_generation, &mut attempts_used).await;
                reader_reconnecting.store(false, Ordering::Release);
                let Some(next_reader) = restored else { break; };
                reader = next_reader;
                connected_since = std::time::Instant::now();
            }
            reader_finished.store(true, Ordering::Release);
        });
        let command_active = Arc::clone(&active);
        let command_closed = Arc::clone(&closed);
        let command_epoch = Arc::clone(&connection_epoch);
        let command_detail = Arc::clone(&terminal_detail);
        let command_clipboard = clipboard.clone();
        let command_zmodem = Arc::clone(&zmodem);
        let command_ingress = ingress.clone();
        let writer_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut clipboard_cleanup = std::time::Instant::now();
            loop {
                let command = tokio::select! {
                    biased;
                    _ = tick.tick() => {
                        if clipboard_cleanup.elapsed() >= std::time::Duration::from_secs(1) {
                            command_clipboard.expire_pending();
                            clipboard_cleanup = std::time::Instant::now();
                        }
                        None
                    }
                    command = receiver.recv() => {
                        let Some(command) = command else { break; };
                        Some(command)
                    }
                };
                if command_closed.load(Ordering::Acquire) {
                    if io_finished.load(Ordering::Acquire) { return; }
                    while receiver.try_recv().is_ok() {}
                    let _ = responses.drain();
                    continue;
                }
                let current = command_active.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                let transfer = Arc::clone(&command_zmodem);
                let ingress = command_ingress.clone();
                let needs_service = command_zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner).needs_service();
                let transfer_epoch = command_zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner).input_epoch();
                let (wire, transferring, transfer_epoch) = if needs_service { match tokio::task::spawn_blocking(move || {
                    let mut transfer = transfer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let was_busy = transfer.busy();
                    let wire = transfer.pump();
                    let mut tail = transfer.terminal_tail();
                    tail.extend(transfer.flush_candidate());
                    if !tail.is_empty() { let _ = ingress.submit_blocking(tail); }
                    (wire, was_busy || transfer.busy(), transfer.input_epoch())
                }).await { Ok(result)=>result, Err(_)=>break } } else {(Vec::new(),false,transfer_epoch)};
                let send = async {
                    if !wire.is_empty() { current.writer.send_input(wire).await?; }
                    for bytes in responses.drain() { if !transferring { current.writer.send_input(bytes).await?; } }
                    match command {
                        Some(Command::Input(epoch, input_transfer_epoch, bytes)) if !transferring && epoch == command_epoch.load(Ordering::Acquire) && input_transfer_epoch == transfer_epoch => current.writer.send_input(bytes).await,
                        Some(Command::Resize(size)) => current.writer.resize(u32::from(size.rows), u32::from(size.cols), u32::from(size.pixel_width), u32::from(size.pixel_height)).await,
                        _ => Ok(()),
                    }
                };
                if !matches!(tokio::time::timeout(std::time::Duration::from_secs(5), send).await, Ok(Ok(()))) {
                    let failed_current = {
                        let active = command_active.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if Arc::ptr_eq(&active.client, &current.client) {
                            replace_terminal_detail(&command_detail, "SSH input delivery failed; remote process outcome is unknown".into());
                            command_closed.store(true, Ordering::Release);
                            command_epoch.fetch_add(1, Ordering::AcqRel);
                            true
                        } else { false }
                    };
                    if failed_current { current.lease.release().await; }
                }
            }
        });
        Ok(Self {
            directory,
            zmodem,
            id,
            title,
            terminal_type: terminal_type.to_owned(),
            logging,
            profile_id: None,
            terminal_detail,
            active,
            reconnecting,
            stopped,
            connection_epoch,
            input_generation,
            size,
            pipeline: Mutex::new(Some(pipeline)),
            ingress: Mutex::new(Some(ingress)),
            snapshots,
            clipboard,
            clipboard_host: None,
            clipboard_trust: None,
            line_index,
            commands,
            closed,
            reader_task: Mutex::new(Some(reader_task)),
            writer_task: Mutex::new(Some(writer_task)),
        })
    }

    fn active_transport(&self) -> recovery::ActiveTransport {
        self.active.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
    pub fn id(&self) -> SessionId {
        self.id
    }
    pub(crate) fn profile_id(&self) -> Option<ProfileId> {
        self.profile_id
    }
    pub(crate) fn clipboard_request(
        &self,
        operation: cshell_ipc::ClipboardOperation,
        token: &[u8],
    ) -> cshell_ipc::ClipboardResponse {
        let response = self.clipboard.handle(operation, token, self.running());
        if operation != cshell_ipc::ClipboardOperation::Poll {
            tracing::info!(session_id = %self.id, operation = ?operation, status = response.status, "remote clipboard request decided");
        }
        response
    }
    #[must_use]
    pub fn with_profile_id(mut self, id: ProfileId) -> Self {
        self.profile_id = Some(id);
        self
    }
    pub(crate) fn with_clipboard_host(mut self, host: cshell_domain::ClipboardHost) -> Self {
        self.clipboard_host = Some(host);
        self
    }
    pub(crate) fn with_clipboard_trust(
        mut self,
        plan: &crate::ssh_route::SshConnectionPlan,
    ) -> Result<Self, serde_json::Error> {
        self.clipboard_trust = Some(plan.verified_peer(&self.active_transport().client, self.active_transport().jump.as_ref())?);
        Ok(self)
    }
    pub(crate) fn clipboard_trust(&self) -> Option<&cshell_domain::ClipboardTrust> {
        self.clipboard_trust.as_ref()
    }
    pub(crate) fn set_clipboard_policy(
        &self,
        document: &cshell_application::ClipboardPolicyDocument,
    ) {
        let blocked = self
            .clipboard_host
            .as_ref()
            .is_some_and(|host| document.blocked_hosts.contains(host));
        let automatic = self
            .clipboard_trust
            .as_ref()
            .is_some_and(|identity| document.automatic.contains(identity));
        self.clipboard.set_policy(blocked, automatic);
    }
    pub(crate) fn clipboard_host(&self) -> Option<&cshell_domain::ClipboardHost> {
        self.clipboard_host.as_ref()
    }
    pub(crate) fn set_host_clipboard_blocked(&self, blocked: bool) {
        self.clipboard.set_host_blocked(blocked);
    }
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.id.as_uuid().as_bytes().to_vec(),
            title: self.title.clone(),
            running: self.running(),
            can_duplicate_ssh_channel: self.running() && self.active_transport().lease.running(),
            reconnecting: self.reconnecting.load(Ordering::Acquire),
            generation: self.generation(),
            profile_id: self.profile_id.map(|id| id.as_uuid().as_bytes().to_vec()),
            terminal_detail: self
                .terminal_detail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
    pub(crate) fn fork_transport(&self) -> Result<crate::shared_ssh::TransportLease, SshSessionError> {
        if !self.running() { return Err(SshSessionError::Closed); }
        self.active_transport().lease.fork().map_err(SshSessionError::ChannelReuse)
    }
    pub(crate) fn transport_generation(&self)->u64 {self.connection_epoch.load(Ordering::Acquire)}
    pub(crate) async fn duplicate_channel(&self, id:SessionId, size:TerminalSize, journal:&Path) -> Result<Self,SshSessionError> {
        let lease = self.fork_transport()?;
        let title=format!("{} · SSH 渠道",self.title.strip_suffix(" · SSH 渠道").unwrap_or(&self.title));
        let mut session = Self::from_transport_configured(id, title, lease, size, journal, 256, &self.terminal_type, self.logging, None).await?;
        replace_terminal_detail(&session.terminal_detail,"SSH 渠道已连接：复用原 SSH 认证，Shell 与输入输出独立。".into());
        session.profile_id = self.profile_id;
        session.clipboard_host = self.clipboard_host.clone();
        session.clipboard_trust = self.clipboard_trust.clone();
        Ok(session)
    }
    pub async fn open_sftp(&self) -> Result<cshell_sftp::SftpClient, SshSessionError> {
        if !self.running() {
            return Err(SshSessionError::Closed);
        }
        Ok(self.active_transport().client.open_sftp().await?)
    }
    pub(crate) fn checksum_connection(&self)->Option<Arc<cshell_ssh::SshConnection>> {
        self.running().then(||self.active_transport().client.clone())
    }

    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn running(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && !self.stopped.load(Ordering::Acquire)
    }
    pub fn generation(&self) -> u64 {
        self.snapshots.latest().map_or(0, |frame| frame.generation)
    }
    pub fn line_index(&self) -> JournalLineIndex {
        self.line_index.clone()
    }
    pub fn full_frame(&self) -> Option<FullFrame> {
        self.snapshots
            .latest()
            .map(|frame| FullFrame::from_terminal_snapshot(self.id, &frame))
    }
    pub fn subscribe(&self) -> TerminalFrameSubscription {
        TerminalFrameSubscription::new(self.id, self.snapshots.clone())
    }
    pub fn send_input(&self, action: &InputAction) -> Result<(), SshSessionError> {
        self.send_input_observed(action, self.snapshots.latest().map(|snapshot| snapshot.generation))
    }
    pub(crate) fn send_input_observed(&self, action: &InputAction, observed_generation: Option<u64>) -> Result<(), SshSessionError> {
        let transfer_epoch = {
            let transfer = self.zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if transfer.busy() { return Err(SshSessionError::Backpressure); }
            transfer.input_epoch()
        };
        let epoch = self.connection_epoch.load(Ordering::Acquire);
        let minimum = self.input_generation.load(Ordering::Acquire);
        if !self.running() || (minimum > 0 && observed_generation.is_none_or(|generation| generation < minimum)) {
            return Err(SshSessionError::Closed);
        }
        let modes = self
            .snapshots
            .latest()
            .map_or_else(TerminalModes::default, |frame| frame.terminal_modes);
        let bytes = InputEncoder::encode(action, modes)?;
        if !bytes.is_empty() {
            self.commands
                .try_send(Command::Input(epoch, transfer_epoch, bytes))
                .map_err(|_| SshSessionError::Backpressure)?;
        }
        Ok(())
    }
    pub fn resize(&self, size: TerminalSize) -> Result<(), SshSessionError> {
        *self.size.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = size;
        if !self.running() {
            return Err(SshSessionError::Closed);
        }
        self.ingress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .ok_or(SshSessionError::Closed)?
            .resize_blocking(size)
            .map_err(|_| SshSessionError::Closed)?;
        self.commands
            .try_send(Command::Resize(size))
            .map_err(|_| SshSessionError::Backpressure)
    }
    pub async fn close(&self) -> Result<(), SshSessionError> {
        self.zmodem.lock().unwrap_or_else(std::sync::PoisonError::into_inner).disconnected();
        set_terminal_detail(&self.terminal_detail, "SSH session closed by user".into());
        self.stopped.store(true, Ordering::Release);
        self.reconnecting.store(false, Ordering::Release);
        self.closed.store(true, Ordering::Release);
        self.clipboard
            .handle(cshell_ipc::ClipboardOperation::Poll, &[], false);
        let reader = self
            .reader_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let writer = self
            .writer_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = &reader {
            task.abort();
        }
        if let Some(task) = &writer {
            task.abort();
        }
        if let Some(task) = reader {
            let _ = task.await;
        }
        if let Some(task) = writer {
            let _ = task.await;
        }
        let active = self.active_transport();
        active.lease.release().await;
        self.ingress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(pipeline) = self
            .pipeline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            pipeline.shutdown()?;
        }
        Ok(())
    }
}

fn replace_terminal_detail(detail: &Mutex<String>, value: String) {
    *detail.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

fn set_terminal_detail(detail: &Mutex<String>, value: String) {
    let mut detail = detail
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if detail.is_empty() {
        *detail = value;
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.reconnecting.store(false, Ordering::Release);
        self.closed.store(true, Ordering::Release);
        if let Some(task) = self
            .reader_task
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        if let Some(task) = self
            .writer_task
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        self.ingress
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

#[derive(Debug, Default)]
pub struct SshSessionRegistry {
    pub(crate) sftp_targets: RwLock<BTreeMap<SessionId, Arc<crate::sftp_lifecycle::SftpTarget>>>,
    pub(crate) sftp_lifecycle: crate::sftp_lifecycle::Lifecycle,
    sessions: RwLock<BTreeMap<SessionId, Arc<SshSession>>>,
}
impl SshSessionRegistry {
    pub fn insert(&self, session: SshSession) -> Arc<SshSession> {
        let session = Arc::new(session);
        self.sftp_targets.write().unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session.id(), Arc::new(crate::sftp_lifecycle::SftpTarget::Terminal(session.clone())));
        self.sessions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session.id(), Arc::clone(&session));
        session
    }
    pub fn get(&self, id: SessionId) -> Result<Arc<SshSession>, SshSessionError> {
        self.sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
            .ok_or(SshSessionError::Unknown(id))
    }
    pub fn contains(&self, id: SessionId) -> bool {
        self.sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id)
    }
    pub fn list(&self) -> Vec<Arc<SshSession>> {
        self.sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }
    pub fn remove(&self, id: SessionId) -> Result<Arc<SshSession>, SshSessionError> {
        self.sftp_targets.write().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        self.sessions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
            .ok_or(SshSessionError::Unknown(id))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    mod transfer_policy;
    use super::{SshConnect, SshSession};
    use cshell_domain::{InputAction, ProfileId, SessionId, TerminalSize};
    use cshell_ssh::KnownHostsVerifier;
    use rand::rng;
    use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey, PublicKey};
    use russh::server::{Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{
        Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[derive(Default)]
    pub(crate) struct EchoServer {
        zmodem_greeting: Option<Vec<u8>>,
        pub(super) received_input: Option<Arc<Mutex<Vec<u8>>>>,
        channels: HashMap<ChannelId, Channel<Msg>>,
        pub(crate) accepted_public_key: Option<PublicKey>,
        pub(crate) deny_terminal: bool,
        pub(crate) terminal_requests: Option<Arc<std::sync::atomic::AtomicUsize>>,
        sftp_state: Arc<Mutex<SftpFixtureState>>,
        pub(crate) mfa_codes: Option<Arc<std::sync::atomic::AtomicUsize>>,
        pub(crate) mfa_passwords: Option<Arc<std::sync::atomic::AtomicUsize>>,
        pub(crate) terminal_limit: Option<usize>,
        code_verified: bool,
        opened_shells: usize,
        directory_markers:HashMap<ChannelId,String>,
        shell_directories:HashMap<String,Arc<Mutex<String>>>,
    }
    impl EchoServer {
        pub(crate) fn enable_upload_test_commands(&mut self) {self.sftp_state.lock().unwrap().checksum_exec=true;}
    }
    impl russh::server::Handler for EchoServer {
        type Error = russh::Error;
        async fn auth_none(&mut self, _: &str) -> Result<Auth,Self::Error> {
            if self.mfa_codes.is_some() {return Ok(Auth::Reject{proceed_with_methods:Some((&[russh::MethodKind::KeyboardInteractive][..]).into()),partial_success:false});}
            Ok(Auth::reject())
        }
        async fn auth_keyboard_interactive<'a>(&'a mut self,_:&str,_:&str,response:Option<russh::server::Response<'a>>)->Result<Auth,Self::Error> {
            let Some(counter)=&self.mfa_codes else {return Ok(Auth::reject());};
            if let Some(mut response)=response {
                if response.next().is_some_and(|v|v.as_ref()==b"654321") {
                    self.code_verified=true;
                    return Ok(Auth::Reject{proceed_with_methods:Some((&[russh::MethodKind::Password][..]).into()),partial_success:true});
                }
                return Ok(Auth::reject());
            }
            counter.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            Ok(Auth::Partial{name:std::borrow::Cow::Borrowed("MFA"),instructions:std::borrow::Cow::Borrowed(""),prompts:std::borrow::Cow::Owned(vec![(std::borrow::Cow::Borrowed("Verification code: "),false)])})
        }
        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            if let Some(counter)=&self.mfa_passwords {counter.fetch_add(1,std::sync::atomic::Ordering::SeqCst);}
            Ok(if user == "cshell" && password == "phase1" && (self.mfa_codes.is_none() || self.code_verified) {
                Auth::Accept
            } else {
                Auth::reject()
            })
        }
        async fn auth_publickey(
            &mut self,
            user: &str,
            public_key: &PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(
                if user == "cshell" && self.accepted_public_key.as_ref() == Some(public_key) {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            )
        }
        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }
        async fn env_request(&mut self,channel:ChannelId,name:&str,value:&str,_session:&mut Session)->Result<(),Self::Error> {
            if name=="LC_CSHELL_SESSION" {self.directory_markers.insert(channel,value.to_owned());}
            Ok(())
        }
        async fn exec_request(&mut self,channel:ChannelId,data:&[u8],session:&mut Session)->Result<(),Self::Error> {
            let command=String::from_utf8_lossy(data);
            if let Some(directory)=command.strip_prefix("cd -- '").and_then(|s|s.strip_suffix("' && exec rz -E")) {
                let directory=directory.replace("'\\''","'");
                let Some(stream)=self.channels.remove(&channel) else {return Ok(());};
                session.channel_success(channel)?;
                let handle=session.handle();let state=self.sftp_state.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut stream=stream.into_stream();let mut receiver=zmodem2::Receiver::new().unwrap();
                    let mut pending=Vec::new();let mut current=Vec::new();let mut name=String::new();
                    for _ in 0..100_000 {
                        match receiver.poll() {
                            zmodem2::Action::WriteWire(bytes)=>{let bytes=bytes.to_vec();receiver.wire_written(bytes.len());if stream.write_all(&bytes).await.is_err(){break;}},
                            zmodem2::Action::WriteFile(bytes)=>{let length=bytes.len();current.extend_from_slice(bytes);receiver.file_written(length).unwrap();},
                            zmodem2::Action::Event(zmodem2::Event::FileStarted(info))=>{name=String::from_utf8(info.name.to_vec()).unwrap();current.clear();},
                            zmodem2::Action::Event(zmodem2::Event::FileCompleted)=>{state.lock().unwrap().files.insert(format!("{directory}/{name}"),std::mem::take(&mut current));},
                            zmodem2::Action::Event(zmodem2::Event::SessionCompleted)=>{
                                while let zmodem2::Action::WriteWire(bytes)=receiver.poll() {let bytes=bytes.to_vec();receiver.wire_written(bytes.len());if stream.write_all(&bytes).await.is_err(){break;}}
                                // Sender finishes after our ZFIN with OO; consume it before closing.
                                let mut tail=[0u8;32];let _=tokio::time::timeout(std::time::Duration::from_secs(2),stream.read(&mut tail)).await;
                                break;
                            },
                            zmodem2::Action::Idle=>{if pending.is_empty(){let mut buffer=vec![0;65536];let n=match stream.read(&mut buffer).await{Ok(n) if n>0=>n,_=>break};pending.extend_from_slice(&buffer[..n]);}let n=receiver.submit_wire(&pending).unwrap();pending.drain(..n);},
                            _=>{},
                        }
                    }
                    let _=handle.exit_status_request(channel,0).await;let _=handle.close(channel).await;
                });
                return Ok(());
            }
            let checksum_output={
                use sha2::Digest;
                let mut state=self.sftp_state.lock().unwrap();
                if state.checksum_exec {
                    if let Some(path)=command.strip_prefix("LC_ALL=C cat -- '").and_then(|v|v.strip_suffix('\'')) {
                        Some(if state.checksum_wrong_namespace {b"different filesystem".to_vec()}else{state.files.get(path).cloned().unwrap_or_default()})
                    } else if let Some(path)=command.strip_prefix("LC_ALL=C sha256sum < '").and_then(|v|v.strip_suffix('\'')) {
                        state.checksum_commands+=1;
                        state.files.get(path).map(|bytes| {
                            let mut digest:[u8;32]=sha2::Sha256::digest(bytes).into();
                            if state.checksum_corrupt {digest[0]^=1;}
                            format!("{}  -\n",digest.iter().map(|b|format!("{b:02x}")).collect::<String>()).into_bytes()
                        })
                    }else{None}
                }else{None}
            };
            if let Some(output)=checksum_output {
                session.channel_success(channel)?;session.data(channel,output)?;session.exit_status_request(channel,0)?;session.close(channel)?;
                self.channels.remove(&channel);return Ok(());
            }
            let path=self.shell_directories.iter().find(|(marker,_)|command.contains(marker.as_str())).map(|(_,cwd)|cwd.lock().unwrap().clone());
            if let Some(path)=path {
                session.channel_success(channel)?;
                session.data(channel,format!("\0CSHELL_CWD\0{path}\n").into_bytes())?;
                session.exit_status_request(channel,0)?;
                session.close(channel)?;
            } else {session.channel_failure(channel)?;session.close(channel)?;}
            self.channels.remove(&channel);
            Ok(())
        }
        async fn subsystem_request(
            &mut self,
            channel: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if name != "sftp" {
                session.channel_failure(channel)?;
                return Ok(());
            }
            let Some(channel_stream) = self.channels.remove(&channel) else {
                session.channel_failure(channel)?;
                return Ok(());
            };
            session.channel_success(channel)?;
            russh_sftp::server::run(
                channel_stream.into_stream(),
                SftpFixture {
                    listed: false,
                    state: Arc::clone(&self.sftp_state),
                },
            )
            .await;
            Ok(())
        }
        async fn pty_request(
            &mut self,
            channel: ChannelId,
            _term: &str,
            _cols: u32,
            _rows: u32,
            _width: u32,
            _height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if let Some(count)=&self.terminal_requests {count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);}
            if self.deny_terminal || self.terminal_limit.is_some_and(|limit| self.opened_shells>=limit) {session.channel_failure(channel)?;} else {session.channel_success(channel)?;}
            Ok(())
        }
        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if let Some(count)=&self.terminal_requests {count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);}
            if self.deny_terminal {session.channel_failure(channel)?;return Ok(());}
            self.opened_shells+=1;
            let cwd=Arc::new(Mutex::new("/home/cshell".to_owned()));
            if let Some(marker)=self.directory_markers.remove(&channel) {self.shell_directories.insert(marker,cwd.clone());}
            let channel = self.channels.remove(&channel).unwrap();
            session.channel_success(channel.id())?;
            let handle = session.handle();
            let channel_id = channel.id();
            let received_input = self.received_input.clone();
            if let Some(greeting) = self.zmodem_greeting.clone() {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let (mut reader, mut writer) = tokio::io::split(channel.into_stream());
                    let _ = writer.write_all(&greeting).await;
                    let mut bytes = [0;4096];
                    let mut cancelled = false;
                    loop {
                        let Ok(n) = reader.read(&mut bytes).await else {break;};
                        if n==0 {break;}
                        if let Some(capture)=&received_input {capture.lock().unwrap().extend_from_slice(&bytes[..n]);}
                        if !cancelled && bytes[..n].contains(&0x18) {cancelled=true;let _=writer.write_all(b"ready$ ").await;}
                        else if cancelled {let _=writer.write_all(&bytes[..n]).await;}
                    }
                });
                return Ok(());
            }
            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(channel.into_stream());
                let mut reader = BufReader::new(reader);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    let Ok(count) = reader.read_until(b'\n', &mut line).await else {
                        break;
                    };
                    if count == 0 {
                        break;
                    }
                    if let Some(received) = &received_input {
                        received.lock().unwrap().extend_from_slice(&line);
                    }
                    if let Some(path)=line.strip_prefix(b"cd ").and_then(|p|std::str::from_utf8(p).ok()).and_then(|p|p.strip_suffix('\n')) { *cwd.lock().unwrap()=path.to_owned(); }
                    if line == b"exit\n" {
                        let _ = handle.exit_status_request(channel_id, 42).await;
                        let _ = handle.close(channel_id).await;
                        break;
                    }
                    if writer.write_all(b"ACK:").await.is_err()
                        || writer.write_all(&line).await.is_err()
                    {
                        break;
                    }
                }
            });
            Ok(())
        }
    }

    #[derive(Default)]
    struct SftpFixtureState {
        read_bytes: u64,
        flushes: u64,
        checksum_commands: u64,
        checksum_exec: bool,
        checksum_extension: bool,
        checksum_wrong_namespace: bool,
        checksum_corrupt: bool,
        corrupt_writes: bool,
        files: HashMap<String, Vec<u8>>,
        handles: HashMap<String, String>,
        directories: HashMap<String, u32>,
        modes: HashMap<String, u32>,
        times: HashMap<String, u32>,
    }

    struct SftpFixture {
        listed: bool,
        state: Arc<Mutex<SftpFixtureState>>,
    }

    impl SftpFixture {
        fn status(id: u32) -> Status {
            Status {
                id,
                status_code: StatusCode::Ok,
                error_message: String::new(),
                language_tag: String::new(),
            }
        }
    }

    impl russh_sftp::server::Handler for SftpFixture {
        type Error = StatusCode;

        async fn init(
            &mut self,
            _version: u32,
            _extensions: HashMap<String, String>,
        ) -> Result<russh_sftp::protocol::Version, Self::Error> {
            let mut version = russh_sftp::protocol::Version::new();
            if self.state.lock().unwrap().checksum_extension {
                let mut supported=vec![0;28];supported.extend_from_slice(&1u32.to_be_bytes());supported.extend_from_slice(&10u32.to_be_bytes());supported.extend_from_slice(b"check-file");
                version.extensions.insert("supported2".into(),String::from_utf8(supported).unwrap());
            }
            for extension in [
                "posix-rename@openssh.com",
                russh_sftp::extensions::HARDLINK,
                russh_sftp::extensions::FSYNC,
            ] {
                version.extensions.insert(extension.into(), "1".into());
            }
            Ok(version)
        }

        async fn extended(
            &mut self,
            id: u32,
            request: String,
            data: Vec<u8>,
        ) -> Result<russh_sftp::protocol::Packet, Self::Error> {
            fn string(data: &mut &[u8]) -> Result<String, StatusCode> {
                let size = u32::from_be_bytes(
                    data.get(..4)
                        .ok_or(StatusCode::Failure)?
                        .try_into()
                        .unwrap(),
                ) as usize;
                let value =
                    String::from_utf8(data.get(4..4 + size).ok_or(StatusCode::Failure)?.to_vec())
                        .map_err(|_| StatusCode::Failure)?;
                *data = &data[4 + size..];
                Ok(value)
            }
            let mut data = data.as_slice();
            let mut state = self.state.lock().unwrap();
            match request.as_str() {
                "check-file-handle" if state.checksum_extension=>{
                    use sha2::Digest;
                    let handle=string(&mut data)?;let algorithm=string(&mut data)?;
                    if algorithm!="sha256" || data!=[0;20] {return Err(StatusCode::Failure);}
                    let path=state.handles.get(&handle).ok_or(StatusCode::Failure)?;
                    let bytes=state.files.get(path).ok_or(StatusCode::NoSuchFile)?;
                    let mut digest:[u8;32]=sha2::Sha256::digest(bytes).into();if state.checksum_corrupt {digest[0]^=1;}
                    state.checksum_commands+=1;
                    let mut reply=b"\0\0\0\x06sha256".to_vec();reply.extend_from_slice(&digest);
                    return Ok(russh_sftp::protocol::Packet::ExtendedReply(russh_sftp::protocol::ExtendedReply{id,data:reply}));
                },
                russh_sftp::extensions::FSYNC => {
                    state.flushes+=1;
                    let handle = string(&mut data)?;
                    if !state.handles.contains_key(&handle) {
                        return Err(StatusCode::Failure);
                    }
                }
                russh_sftp::extensions::HARDLINK => {
                    let source = string(&mut data)?;
                    let target = string(&mut data)?;
                    if state.files.contains_key(&target) {
                        return Err(StatusCode::Failure);
                    }
                    let bytes = state
                        .files
                        .get(&source)
                        .ok_or(StatusCode::NoSuchFile)?
                        .clone();
                    if let Some(mode)=state.modes.get(&source).copied(){state.modes.insert(target.clone(),mode);}
                    if let Some(time)=state.times.get(&source).copied(){state.times.insert(target.clone(),time);}
                    state.files.insert(target, bytes);
                }
                "posix-rename@openssh.com" => {
                    let source=string(&mut data)?;let target=string(&mut data)?;
                    let bytes=state.files.remove(&source).ok_or(StatusCode::NoSuchFile)?;
                    state.modes.remove(&target);state.times.remove(&target);
                    if let Some(mode)=state.modes.remove(&source){state.modes.insert(target.clone(),mode);}
                    if let Some(time)=state.times.remove(&source){state.times.insert(target.clone(),time);}
                    state.files.insert(target,bytes);
                }
                _ => return Err(StatusCode::OpUnsupported),
            }
            Ok(russh_sftp::protocol::Packet::Status(Self::status(id)))
        }

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn realpath(&mut self,id:u32,path:String)->Result<Name,Self::Error> {
            let path=if path=="." {"/home/cshell".into()} else {path};
            Ok(Name{id,files:vec![File::new(path,FileAttributes::default())]})
        }
        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
            if path != "/" {
                if !self.state.lock().unwrap().directories.contains_key(&path){return Err(StatusCode::NoSuchFile);}
                self.listed=false;return Ok(Handle{id,handle:format!("directory:{path}")});
            }
            Ok(Handle {
                id,
                handle: "directory".into(),
            })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
            if let Some(directory)=handle.strip_prefix("directory:") {
                if self.listed{return Err(StatusCode::Eof);}self.listed=true;
                let prefix=format!("{}/",directory.trim_end_matches('/'));let state=self.state.lock().unwrap();let mut files=Vec::new();
                for (path,bytes) in &state.files {if let Some(name)=path.strip_prefix(&prefix).filter(|p|!p.is_empty()&&!p.contains('/')) {files.push(File::new(name,FileAttributes{size:Some(bytes.len() as u64),permissions:Some(0o100000|state.modes.get(path).copied().unwrap_or(0o644)),mtime:state.times.get(path).copied(),atime:state.times.get(path).copied(),..Default::default()}));}}
                for (path,mode) in &state.directories {if let Some(name)=path.strip_prefix(&prefix).filter(|p|!p.is_empty()&&!p.contains('/')) {files.push(File::new(name,FileAttributes{permissions:Some(0o040000|mode),..Default::default()}));}}
                return Ok(Name{id,files});
            }
            if handle != "directory" {
                return Err(StatusCode::Failure);
            }
            if self.listed {
                return Err(StatusCode::Eof);
            }
            self.listed = true;
            Ok(Name {
                id,
                files: vec![File::new(
                    "hello.txt",
                    FileAttributes {
                        size: Some(5),
                        permissions: Some(0o100644),
                        uid: Some(1000),
                        gid: Some(100),
                        mtime: Some(1_700_000_000),
                        ..FileAttributes::default()
                    },
                )],
            })
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            flags: OpenFlags,
            _attrs: FileAttributes,
        ) -> Result<Handle, Self::Error> {
            let mut state = self.state.lock().unwrap();
            if flags.contains(OpenFlags::EXCLUDE) && state.files.contains_key(&filename) {
                return Err(StatusCode::Failure);
            }
            if flags.contains(OpenFlags::CREATE) {
                state.files.entry(filename.clone()).or_default();
            }
            if !state.files.contains_key(&filename) {
                return Err(StatusCode::NoSuchFile);
            }
            let handle = format!("file-{}", SessionId::new());
            state.handles.insert(handle.clone(), filename);
            Ok(Handle { id, handle })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            self.state.lock().unwrap().handles.remove(&handle);
            Ok(Self::status(id))
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            let mut state = self.state.lock().unwrap();
            let path = state.handles.get(&handle).ok_or(StatusCode::Failure)?;
            let bytes = state.files.get(path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
            if start >= bytes.len() {
                return Err(StatusCode::Eof);
            }
            let end = start.saturating_add(len as usize).min(bytes.len());
            let data=bytes[start..end].to_vec();state.read_bytes+=data.len() as u64;
            Ok(Data {id,data})
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<Status, Self::Error> {
            let mut state = self.state.lock().unwrap();
            let corrupt=state.corrupt_writes;
            let path = state
                .handles
                .get(&handle)
                .ok_or(StatusCode::Failure)?
                .clone();
            let bytes = state.files.get_mut(&path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
            let end = start.checked_add(data.len()).ok_or(StatusCode::Failure)?;
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[start..end].copy_from_slice(&data);
            if corrupt && !data.is_empty(){bytes[start]^=1;}
            Ok(Self::status(id))
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            if ["/","/home","/home/cshell","/srv","/srv/one","/srv/two"].contains(&path.as_str()) {
                return Ok(Attrs {
                    id,
                    attrs: FileAttributes {
                        permissions: Some(0o040755),
                        ..Default::default()
                    },
                });
            }
            let state = self.state.lock().unwrap();
            if let Some(mode) = state.directories.get(&path) {
                return Ok(Attrs { id, attrs: FileAttributes { permissions: Some(0o040000 | mode), ..Default::default() } });
            }
            let bytes = state.files.get(&path).ok_or(StatusCode::NoSuchFile)?;
            Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(bytes.len() as u64),
                    permissions: Some(0o100000 | state.modes.get(&path).copied().unwrap_or(0o644)),
                    mtime:state.times.get(&path).copied(),atime:state.times.get(&path).copied(),
                    ..FileAttributes::default()
                },
            })
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.stat(id, path).await
        }

        async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
            let path = self
                .state
                .lock()
                .unwrap()
                .handles
                .get(&handle)
                .cloned()
                .ok_or(StatusCode::Failure)?;
            self.stat(id, path).await
        }
        async fn fsetstat(&mut self,id:u32,handle:String,attrs:FileAttributes)->Result<Status,Self::Error> {
            let mut state=self.state.lock().unwrap();
            let path=state.handles.get(&handle).cloned().ok_or(StatusCode::Failure)?;
            if let Some(size)=attrs.size {state.files.get_mut(&path).ok_or(StatusCode::NoSuchFile)?.resize(size as usize,0);}
            if let Some(mode)=attrs.permissions {state.modes.insert(path.clone(),mode&0o7777);}
            if let Some(time)=attrs.mtime {state.times.insert(path,time);}
            Ok(Self::status(id))
        }

        async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
            if self.state.lock().unwrap().files.remove(&path).is_none() {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(Self::status(id))
        }

        async fn mkdir(&mut self, id:u32, path:String, attrs:FileAttributes) -> Result<Status,Self::Error> {
            let mut state=self.state.lock().unwrap();
            if state.files.contains_key(&path) || state.directories.contains_key(&path) { return Err(StatusCode::Failure); }
            state.directories.insert(path,attrs.permissions.unwrap_or(0o755)&0o7777);
            Ok(Self::status(id))
        }
        async fn setstat(&mut self,id:u32,path:String,attrs:FileAttributes)->Result<Status,Self::Error> {
            let mut state=self.state.lock().unwrap();
            let mode=attrs.permissions.ok_or(StatusCode::Failure)? & 0o7777;
            if let Some(current)=state.directories.get_mut(&path) { *current=mode; }
            else if state.files.contains_key(&path) {state.modes.insert(path,mode);}
            else {return Err(StatusCode::NoSuchFile);}
            Ok(Self::status(id))
        }
        async fn rename(
            &mut self,
            id: u32,
            oldpath: String,
            newpath: String,
        ) -> Result<Status, Self::Error> {
            let mut state = self.state.lock().unwrap();
            if state.files.contains_key(&newpath) || state.directories.contains_key(&newpath) {
                return Err(StatusCode::Failure);
            }
            if let Some(mode)=state.directories.remove(&oldpath) {state.directories.insert(newpath,mode);}
            else {
                let bytes = state.files.remove(&oldpath).ok_or(StatusCode::NoSuchFile)?;
                if let Some(mode)=state.modes.remove(&oldpath) {state.modes.insert(newpath.clone(),mode);}
                if let Some(time)=state.times.remove(&oldpath) {state.times.insert(newpath.clone(),time);}
                state.files.insert(newpath, bytes);
            }
            Ok(Self::status(id))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_ssh_session_lists_sftp_directory() {
        use crate::sftp_ipc::SftpIpcService;
        use cshell_ipc::{SftpOperation, SftpRequest, SftpStatus, SftpTransferState};
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let mut config = russh::server::Config::default();
        config.keys.push(key);
        config.auth_rejection_time = std::time::Duration::from_millis(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let running =
                russh::server::run_stream(Arc::new(config), stream, EchoServer::default())
                    .await
                    .unwrap();
            let _ = running.await;
        });
        let verifier = KnownHostsVerifier::parse(
            &format!("[127.0.0.1]:{} {public}\n", address.port()),
            "127.0.0.1",
            address.port(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let mut session = SshSession::connect(
            id,
            "sftp".into(),
            SshConnect {
                username: "cshell".into(),
                verifier,
                authentication: super::SshAuthentication::Password("phase1".into()),
            },
            TerminalSize::cells(24, 80),
            &directory.path().join("sftp.csjr"),
            16,
        )
        .await
        .unwrap();
        session.clipboard_trust = Some(cshell_domain::ClipboardTrust {
            host: cshell_domain::ClipboardHost {
                host: "127.0.0.1".into(),
                port: address.port(),
            },
            username: "cshell".into(),
            host_key_sha256: session.active_transport().client.verified_host_key_fingerprint().into(),
            route: "direct".into(),
        });
        let registry = super::SshSessionRegistry::default();
        registry.insert(session);
        let sftp = SftpIpcService::default();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            sftp.handle(
                SftpRequest {
                    operation: SftpOperation::List as i32,
                    session_id: id.as_uuid().as_bytes().to_vec(),
                    remote_path: "/".into(),
                    max_entries: 16,
                    ..Default::default()
                },
                &registry,
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status, SftpStatus::Ok as i32);
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].path, "/hello.txt");
        assert_eq!(response.entries[0].permissions, Some(0o100644));
        assert_eq!(response.entries[0].uid, Some(1000));
        assert_eq!(response.entries[0].gid, Some(100));
        assert_eq!(
            response.entries[0].modified_unix_seconds,
            Some(1_700_000_000)
        );
        let browser = sftp
            .handle(
                SftpRequest {
                    operation: SftpOperation::BrowseManaged as i32,
                    session_id: id.as_uuid().as_bytes().to_vec(),
                    max_entries: 16,
                    document_json: serde_json::to_vec(&cshell_application::SftpBrowseOptions {
                        local: false,
                        path: "/".into(),
                        links: cshell_application::SftpLinkPolicy::NoFollow,
                    })
                    .unwrap(),
                    ..Default::default()
                },
                &registry,
            )
            .await;
        assert_eq!(browser.status, SftpStatus::Ok as i32, "{}", browser.detail);
        assert_eq!(browser.entries, response.entries);

        let source = directory.path().join("source.bin");
        let destination = directory.path().join("downloaded.bin");
        let bytes = vec![0x5a_u8; cshell_sftp::SFTP_TRANSFER_CHUNK_BYTES * 2 + 17];
        tokio::fs::write(&source, &bytes).await.unwrap();
        for (operation, remote_path, local_path) in [
            (SftpOperation::Upload, "/uploaded.bin", source.as_path()),
            (
                SftpOperation::Download,
                "/uploaded.bin",
                destination.as_path(),
            ),
        ] {
            let started = sftp
                .handle(
                    SftpRequest {
                        operation: operation as i32,
                        session_id: id.as_uuid().as_bytes().to_vec(),
                        remote_path: remote_path.into(),
                        local_path: local_path.to_string_lossy().into_owned(),
                        ..Default::default()
                    },
                    &registry,
                )
                .await;
            assert_eq!(started.status, SftpStatus::Ok as i32);
            let transfer_id = started.transfer.unwrap().transfer_id;
            let control_request = |operation| SftpRequest {
                operation: operation as i32,
                session_id: id.as_uuid().as_bytes().to_vec(),
                transfer_id: transfer_id.clone(),
                ..Default::default()
            };
            assert!(
                sftp.handle(control_request(SftpOperation::PauseTransfer), &registry)
                    .await
                    .transfer
                    .unwrap()
                    .paused
            );
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let paused = sftp
                .handle(control_request(SftpOperation::TransferStatus), &registry)
                .await
                .transfer
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let still_paused = sftp
                .handle(control_request(SftpOperation::TransferStatus), &registry)
                .await
                .transfer
                .unwrap();
            assert!(still_paused.paused);
            assert_eq!(still_paused.bytes_transferred, paused.bytes_transferred);
            let resumed = sftp
                .handle(control_request(SftpOperation::ResumeTransfer), &registry)
                .await
                .transfer
                .unwrap();
            assert!(!resumed.paused);
            assert_eq!(resumed.transfer_id, transfer_id);
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let status = sftp
                        .handle(
                            SftpRequest {
                                operation: SftpOperation::TransferStatus as i32,
                                session_id: id.as_uuid().as_bytes().to_vec(),
                                transfer_id: transfer_id.clone(),
                                ..Default::default()
                            },
                            &registry,
                        )
                        .await;
                    let transfer = status.transfer.unwrap();
                    match SftpTransferState::try_from(transfer.state).unwrap() {
                        SftpTransferState::Running => {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        SftpTransferState::Succeeded => {
                            assert_eq!(transfer.bytes_transferred, bytes.len() as u64);
                            break;
                        }
                        other => panic!("transfer {other:?}: {}", transfer.detail),
                    }
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);
        if std::env::var_os("CSHELL_SFTP_TEST_LIVE_QUEUE").is_some() {
            // Explicit integration invocation supplies a disposable runtime;
            // never write queue fixtures into the user's normal runtime.
            assert!(std::env::var_os("CSHELL_RUNTIME_DIR").is_some());
            let query = |operation, revision, transfer: Vec<u8>, document: Vec<u8>| SftpRequest {
                operation: operation as i32,
                session_id: id.as_uuid().as_bytes().to_vec(),
                expected_revision: revision,
                transfer_id: transfer,
                document_json: document,
                confirmed: revision > 0,
                ..Default::default()
            };
            let draft = cshell_application::SftpQueueDraft {
                verified: true,
                rate_limit: Some(32768),
                local_path: source.to_string_lossy().into_owned(),
                remote_path: "/verified-uploaded.bin".into(),
                ..Default::default()
            };
            let added = sftp
                .handle(
                    query(
                        SftpOperation::QueueAddManaged,
                        1,
                        Vec::new(),
                        serde_json::to_vec(&draft).unwrap(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(added.status, SftpStatus::Ok as i32, "{}", added.detail);
            let started = sftp
                .handle(
                    query(SftpOperation::QueueStartManaged, 2, Vec::new(), Vec::new()),
                    &registry,
                )
                .await;
            assert_eq!(started.status, SftpStatus::Ok as i32, "{}", started.detail);
            let running = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let reply = sftp
                        .handle(
                            query(SftpOperation::QueueGet, 0, Vec::new(), Vec::new()),
                            &registry,
                        )
                        .await;
                    let queue =
                        cshell_application::decode_sftp_queue(&reply.document_json).unwrap();
                    let item = &queue.items[0];
                    if item.verified.as_ref().is_some_and(|v| v.bytes > 0) {
                        break item.transfer_id.unwrap();
                    }
                    assert!(
                        matches!(
                            item.state,
                            cshell_application::SftpQueueState::Pending
                                | cshell_application::SftpQueueState::Running
                        ),
                        "{item:?}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let transfer = running.as_uuid().as_bytes().to_vec();
            sftp.handle(
                query(
                    SftpOperation::PauseTransfer,
                    0,
                    transfer.clone(),
                    Vec::new(),
                ),
                &registry,
            )
            .await;
            let loaded = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                sftp.handle(
                    query(SftpOperation::QueueGet, 0, Vec::new(), Vec::new()),
                    &registry,
                ),
            )
            .await
            .expect("paused transfer must release queue storage lock");
            let queue = cshell_application::decode_sftp_queue(&loaded.document_json).unwrap();
            let paused = sftp
                .handle(
                    query(
                        SftpOperation::QueuePause,
                        queue.revision,
                        Vec::new(),
                        Vec::new(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(paused.status, SftpStatus::Ok as i32, "{}", paused.detail);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let at_pause = sftp
                .handle(
                    query(
                        SftpOperation::TransferStatus,
                        0,
                        transfer.clone(),
                        Vec::new(),
                    ),
                    &registry,
                )
                .await
                .transfer
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let still_paused = sftp
                .handle(
                    query(
                        SftpOperation::TransferStatus,
                        0,
                        transfer.clone(),
                        Vec::new(),
                    ),
                    &registry,
                )
                .await
                .transfer
                .unwrap();
            assert!(still_paused.paused);
            assert_eq!(at_pause.bytes_transferred, still_paused.bytes_transferred);
            let queue = cshell_application::decode_sftp_queue(&paused.document_json).unwrap();
            let resumed = sftp
                .handle(
                    query(
                        SftpOperation::QueueStartManaged,
                        queue.revision,
                        Vec::new(),
                        Vec::new(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(resumed.status, SftpStatus::Ok as i32, "{}", resumed.detail);
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let reply = sftp
                        .handle(
                            query(SftpOperation::QueueGet, 0, Vec::new(), Vec::new()),
                            &registry,
                        )
                        .await;
                    let queue =
                        cshell_application::decode_sftp_queue(&reply.document_json).unwrap();
                    if queue.items[0].state == cshell_application::SftpQueueState::Succeeded {
                        break;
                    }
                    assert_eq!(
                        queue.items[0].state,
                        cshell_application::SftpQueueState::Running,
                        "{:?}",
                        queue.items[0]
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let client = registry.get(id).unwrap().open_sftp().await.unwrap();
            let destination = directory.path().join("verified-result.bin");
            client
                .download_to_path_atomic(
                    "/verified-uploaded.bin",
                    &destination,
                    &cshell_sftp::TransferControl::default(),
                    |_| {},
                )
                .await
                .unwrap();
            client.close().await.unwrap();
            assert_eq!(tokio::fs::read(destination).await.unwrap(), bytes);

            // Larger than a checkpoint: exercise three independent SSH/SFTP channels,
            // durable batches, and real content verification in both directions.
            let large: Vec<u8> = (0..2 * cshell_sftp::SFTP_TRANSFER_BATCH_BYTES + 17)
                .map(|i| (i % 251) as u8)
                .collect();
            let large_source = directory.path().join("parallel-source.bin");
            tokio::fs::write(&large_source, &large).await.unwrap();
            for upload in [true, false] {
                // Wait for the prior runner's final paused checkpoint, so this
                // direct IPC fixture does not race its last revision update.
                let before = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let current = sftp
                            .handle(
                                query(SftpOperation::QueueGet, 0, Vec::new(), Vec::new()),
                                &registry,
                            )
                            .await;
                        let queue =
                            cshell_application::decode_sftp_queue(&current.document_json).unwrap();
                        if queue.paused {
                            break queue;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                let drafts: Vec<_> = (0..4)
                    .map(|index| cshell_application::SftpQueueDraft {
                        verified: true,
                        direction: if upload {
                            cshell_application::SftpQueueDirection::Upload
                        } else {
                            cshell_application::SftpQueueDirection::Download
                        },
                        local_path: if upload {
                            large_source.clone()
                        } else {
                            directory
                                .path()
                                .join(format!("parallel-result-{index}.bin"))
                        }
                        .to_string_lossy()
                        .into_owned(),
                        remote_path: format!("/parallel-{index}.bin"),
                        rate_limit: Some(1024 * 1024),
                        ..Default::default()
                    })
                    .collect();
                let added = sftp
                    .handle(
                        query(
                            SftpOperation::QueueEnqueueAndStart,
                            before.revision,
                            Vec::new(),
                            serde_json::to_vec(&drafts).unwrap(),
                        ),
                        &registry,
                    )
                    .await;
                assert_eq!(added.status, SftpStatus::Ok as i32, "{}", added.detail);
                let receipt =
                    cshell_application::decode_sftp_workspace(&added.document_json).unwrap();
                assert!(
                    matches!(receipt, cshell_application::SftpWorkspaceReply::Queue(q) if q.concurrency == Some(3))
                );
                let mut saw_three = false;
                let mut saw_waiting_fourth = false;
                let started = std::time::Instant::now();
                let final_queue = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    loop {
                        let reply = sftp
                            .handle(
                                query(SftpOperation::QueueGet, 0, Vec::new(), Vec::new()),
                                &registry,
                            )
                            .await;
                        let queue =
                            cshell_application::decode_sftp_queue(&reply.document_json).unwrap();
                        let rows = &queue.items[before.items.len()..];
                        let running = rows
                            .iter()
                            .filter(|row| row.state == cshell_application::SftpQueueState::Running)
                            .count();
                        assert!(running <= 3);
                        saw_three |= running == 3;
                        saw_waiting_fourth |= running == 3
                            && rows.iter().any(|row| {
                                row.state == cshell_application::SftpQueueState::Pending
                            });
                        assert!(
                            rows.iter().all(|row| matches!(
                                row.state,
                                cshell_application::SftpQueueState::Pending
                                    | cshell_application::SftpQueueState::Running
                                    | cshell_application::SftpQueueState::Succeeded
                            )),
                            "{rows:?}"
                        );
                        if rows
                            .iter()
                            .all(|row| row.state == cshell_application::SftpQueueState::Succeeded)
                            && queue.paused
                        {
                            break queue;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                assert!(
                    saw_three && saw_waiting_fourth,
                    "three slots must fill, with the fourth waiting"
                );
                let revisions = final_queue.revision - before.revision;
                assert!(revisions < 120, "excessive checkpoint writes: {revisions}");
                println!(
                    "verified {}: 4 x {} bytes, max 3 simultaneous, {} durable revisions, {:?}",
                    if upload { "upload" } else { "download" },
                    large.len(),
                    revisions,
                    started.elapsed()
                );
                if !upload {
                    for index in 0..4 {
                        assert_eq!(
                            tokio::fs::read(
                                directory
                                    .path()
                                    .join(format!("parallel-result-{index}.bin"))
                            )
                            .await
                            .unwrap(),
                            large
                        );
                    }
                }
            }
            // Retry the same durable item after an early source-open failure.
            // Then cancel one active file and resume its checked prefix while another
            // file is still running: retries must not wait for an idle queue.
            async fn wait_queue(
                service: &SftpIpcService,
                registry: &super::SshSessionRegistry,
                session: SessionId,
                predicate: impl Fn(&cshell_application::SftpQueueSnapshot) -> bool,
            ) -> cshell_application::SftpQueueSnapshot {
                tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    loop {
                        let reply = service
                            .handle(
                                SftpRequest {
                                    operation: SftpOperation::QueueGet as i32,
                                    session_id: session.as_uuid().as_bytes().to_vec(),
                                    ..Default::default()
                                },
                                registry,
                            )
                            .await;
                        let queue = cshell_application::decode_sftp_queue(&reply.document_json).unwrap();
                        if predicate(&queue) {
                            break queue;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap()
            }
            let idle = wait_queue(&sftp, &registry, id, |q| {
                q.paused
                    && !q
                        .items
                        .iter()
                        .any(|i| i.state == cshell_application::SftpQueueState::Running)
            })
            .await;
            let missing = directory.path().join("retry-missing-source.bin");
            tokio::fs::write(&missing, b"source present at enqueue")
                .await
                .unwrap();
            let retry_draft = cshell_application::SftpQueueDraft {
                verified: true,
                local_path: missing.to_string_lossy().into_owned(),
                remote_path: "/retry-fresh.bin".into(),
                ..Default::default()
            };
            let added = sftp
                .handle(
                    query(
                        SftpOperation::QueueAddMany,
                        idle.revision,
                        Vec::new(),
                        serde_json::to_vec(&vec![retry_draft]).unwrap(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(added.status, SftpStatus::Ok as i32, "{}", added.detail);
            let added_queue = match cshell_application::decode_sftp_workspace(&added.document_json).unwrap()
            {
                cshell_application::SftpWorkspaceReply::Queue(q) => q,
                _ => panic!("queue reply"),
            };
            tokio::fs::remove_file(&missing).await.unwrap();
            let started = sftp
                .handle(
                    query(
                        SftpOperation::QueueStartManaged,
                        added_queue.revision,
                        Vec::new(),
                        Vec::new(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(started.status, SftpStatus::Ok as i32, "{}", started.detail);
            let failed = wait_queue(&sftp, &registry, id, |q| {
                q.paused
                    && q.items
                        .last()
                        .is_some_and(|i| i.state == cshell_application::SftpQueueState::Unknown)
            })
            .await;
            let retry_id = failed.items.last().unwrap().id;
            assert!(failed.items.last().unwrap().can_restart_verified());
            tokio::fs::write(&missing, b"retry recovered content")
                .await
                .unwrap();
            let plan = cshell_application::SftpQueueRetry {
                items: vec![retry_id],
                start: true,
            };
            let accepted = sftp
                .handle(
                    query(
                        SftpOperation::QueueRetryMany,
                        failed.revision,
                        Vec::new(),
                        serde_json::to_vec(&plan).unwrap(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(
                accepted.status,
                SftpStatus::Ok as i32,
                "{}",
                accepted.detail
            );
            let recovered = wait_queue(&sftp, &registry, id, |q| {
                q.paused
                    && q.items.last().is_some_and(|i| {
                        i.id == retry_id && i.state == cshell_application::SftpQueueState::Succeeded
                    })
            })
            .await;
            assert_eq!(recovered.items.len(), failed.items.len());
            assert!(recovered.retry_items.is_empty());
            let drafts: Vec<_> = (0..2)
                .map(|index| cshell_application::SftpQueueDraft {
                    verified: true,
                    local_path: large_source.to_string_lossy().into_owned(),
                    remote_path: format!("/retry-resume-{index}.bin"),
                    rate_limit: Some(512 * 1024),
                    ..Default::default()
                })
                .collect();
            let added = sftp
                .handle(
                    query(
                        SftpOperation::QueueEnqueueAndStart,
                        recovered.revision,
                        Vec::new(),
                        serde_json::to_vec(&drafts).unwrap(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(added.status, SftpStatus::Ok as i32, "{}", added.detail);
            let at = recovered.items.len();
            let copying = wait_queue(&sftp, &registry, id, |q| {
                q.items.get(at).is_some_and(|i| {
                    i.verified
                        .as_ref()
                        .is_some_and(|v| v.bytes >= 1024 * 1024 && v.bytes < large.len() as u64)
                }) && q
                    .items
                    .get(at + 1)
                    .is_some_and(|i| i.state == cshell_application::SftpQueueState::Running)
            })
            .await;
            let old_transfer = copying.items[at].transfer_id.unwrap();
            let cancelled = sftp
                .handle(
                    SftpRequest {
                        operation: SftpOperation::CancelTransfer as i32,
                        session_id: id.as_uuid().as_bytes().to_vec(),
                        transfer_id: old_transfer.as_uuid().as_bytes().to_vec(),
                        ..Default::default()
                    },
                    &registry,
                )
                .await;
            assert_eq!(cancelled.status, SftpStatus::Ok as i32);
            let failed = wait_queue(&sftp, &registry, id, |q| {
                q.items[at].state == cshell_application::SftpQueueState::Unknown
            })
            .await;
            let original = failed.items[at].id;
            assert!(failed.items[at].can_resume_verified());
            let accepted = sftp
                .handle(
                    query(
                        SftpOperation::QueueRetryMany,
                        failed.revision,
                        Vec::new(),
                        serde_json::to_vec(&cshell_application::SftpQueueRetry {
                            items: vec![original],
                            start: true,
                        })
                        .unwrap(),
                    ),
                    &registry,
                )
                .await;
            assert_eq!(
                accepted.status,
                SftpStatus::Ok as i32,
                "{}",
                accepted.detail
            );
            let concurrent = wait_queue(&sftp, &registry, id, |q| {
                q.items[at].state == cshell_application::SftpQueueState::Running
                    && q.items[at + 1].state == cshell_application::SftpQueueState::Running
            })
            .await;
            assert_ne!(concurrent.items[at].transfer_id, Some(old_transfer));
            assert_eq!(concurrent.items[at].id, original);
            let finished = wait_queue(&sftp, &registry, id, |q| {
                q.paused
                    && q.items[at..]
                        .iter()
                        .all(|i| i.state == cshell_application::SftpQueueState::Succeeded)
            })
            .await;
            assert_eq!(finished.items.len(), failed.items.len());
            assert!(finished.retry_items.is_empty());
            let client = registry.get(id).unwrap().open_sftp().await.unwrap();
            let destination = directory.path().join("retried-result.bin");
            client
                .download_to_path_atomic(
                    "/retry-resume-0.bin",
                    &destination,
                    &cshell_sftp::TransferControl::default(),
                    |_| {},
                )
                .await
                .unwrap();
            client.close().await.unwrap();
            assert_eq!(tokio::fs::read(destination).await.unwrap(), large);
        }
        if std::env::var_os("CSHELL_SFTP_TEST_FILE_OPERATIONS").is_some() {
            assert!(std::env::var_os("CSHELL_RUNTIME_DIR").is_some());
            exercise_reviewed_file_operations(&sftp,&registry,id,directory.path()).await;
        }
        registry.get(id).unwrap().close().await.unwrap();
        server.abort();
    }

    async fn exercise_reviewed_file_operations(sftp:&crate::sftp_ipc::SftpIpcService,registry:&super::SshSessionRegistry,session:SessionId,directory:&std::path::Path) {
        use cshell_application::{SftpFileDraft,SftpFileAction,SftpFileJob,SftpFileState,SftpWorkspaceReply,decode_sftp_workspace};
        use cshell_ipc::{SftpOperation,SftpRequest,SftpStatus};
        async fn call(sftp:&crate::sftp_ipc::SftpIpcService,registry:&super::SshSessionRegistry,session:SessionId,operation:SftpOperation,draft:Option<&SftpFileDraft>,job:Option<&SftpFileJob>)->SftpFileJob {
            let reply=sftp.handle(SftpRequest {operation:operation as i32,session_id:session.as_uuid().as_bytes().to_vec(),
                document_json:draft.map(|d|serde_json::to_vec(d).unwrap()).unwrap_or_default(),
                transfer_id:job.map(|j|j.id.as_uuid().as_bytes().to_vec()).unwrap_or_default(),
                expected_revision:if operation==SftpOperation::FileApply {job.unwrap().revision} else {0},
                confirmed:operation==SftpOperation::FileApply,..Default::default()},registry).await;
            assert_eq!(reply.status,SftpStatus::Ok as i32,"{}",reply.detail);
            match decode_sftp_workspace(&reply.document_json).unwrap() {SftpWorkspaceReply::File(job)=>job,_=>panic!("expected file job")}
        }
        async fn wait(sftp:&crate::sftp_ipc::SftpIpcService,registry:&super::SshSessionRegistry,session:SessionId,mut job:SftpFileJob)->SftpFileJob {
            tokio::time::timeout(std::time::Duration::from_secs(15),async {
                while matches!(job.state,SftpFileState::Inspecting|SftpFileState::Applying) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    job=call(sftp,registry,session,SftpOperation::FileJobGet,None,Some(&job)).await;
                }
                job
            }).await.unwrap()
        }
        for local in [true,false] {
            let first=if local {directory.join("new-directory").to_string_lossy().into_owned()} else {"/new-directory".into()};
            let second=if local {directory.join("renamed-directory").to_string_lossy().into_owned()} else {"/renamed-directory".into()};
            let file=if local {directory.join("source.bin").to_string_lossy().into_owned()} else {"/uploaded.bin".into()};
            for draft in [
                SftpFileDraft {local,action:SftpFileAction::Mkdir,path:first.clone(),target:String::new(),permissions:Some(0o750)},
                SftpFileDraft {local,action:SftpFileAction::Rename,path:first.clone(),target:second.clone(),permissions:None},
                SftpFileDraft {local,action:SftpFileAction::Rename,path:file.clone(),target:format!("{file}.renamed"),permissions:None},
            ] {
                let inspected=call(sftp,registry,session,SftpOperation::FileInspect,Some(&draft),None).await;
                let ready=wait(sftp,registry,session,inspected).await;
                assert_eq!(ready.state,SftpFileState::Ready,"{}",ready.detail);
                let applied=call(sftp,registry,session,SftpOperation::FileApply,None,Some(&ready)).await;
                let finished=wait(sftp,registry,session,applied).await;
                assert_eq!(finished.state,SftpFileState::Succeeded,"{}",finished.detail);
                assert!(finished.acknowledged);
            }
            if local {
                assert!(!std::path::Path::new(&first).exists()); assert!(std::path::Path::new(&second).is_dir());
                assert!(!std::path::Path::new(&file).exists()); assert!(std::path::Path::new(&format!("{file}.renamed")).is_file());
            } else {
                for path in [second,format!("{file}.renamed")] {
                    let draft=SftpFileDraft {local:false,action:SftpFileAction::Chmod,path:path.clone(),target:String::new(),permissions:Some(0o2750)};
                    let inspected=call(sftp,registry,session,SftpOperation::FileInspect,Some(&draft),None).await;
                    let ready=wait(sftp,registry,session,inspected).await;
                    assert_eq!(ready.state,SftpFileState::Ready,"{}",ready.detail);
                    let applied=call(sftp,registry,session,SftpOperation::FileApply,None,Some(&ready)).await;
                    let finished=wait(sftp,registry,session,applied).await;
                    assert_eq!(finished.state,SftpFileState::Succeeded,"{}",finished.detail);
                    let inspect=SftpFileDraft {action:SftpFileAction::Inspect,permissions:None,..draft};
                    let checked=call(sftp,registry,session,SftpOperation::FileInspect,Some(&inspect),None).await;
                    let checked=wait(sftp,registry,session,checked).await;
                    assert_eq!(checked.state,SftpFileState::Ready,"{}",checked.detail);
                    assert_eq!(checked.source.unwrap().permissions,Some(0o2750));
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zmodem_request_is_not_rendered_and_cancel_restores_terminal_input() {
        let key=PrivateKey::random(&mut rng(),Algorithm::Ed25519).unwrap();
        let public=key.public_key().to_openssh().unwrap();
        let mut config=russh::server::Config::default();config.keys.push(key);
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let received=Arc::new(Mutex::new(Vec::<u8>::new()));let capture=received.clone();
        let mut receiver=zmodem2::Receiver::new().unwrap();
        let greeting=match receiver.poll(){zmodem2::Action::WriteWire(bytes)=>bytes.to_vec(),_=>panic!("initial ZRINIT")};
        let server=tokio::spawn(async move {
            let (stream,_)=listener.accept().await.unwrap();
            let running=russh::server::run_stream(Arc::new(config),stream,EchoServer{zmodem_greeting:Some(greeting),received_input:Some(capture),..Default::default()}).await.unwrap();
            let _=running.await;
        });
        let verifier=KnownHostsVerifier::parse(&format!("[127.0.0.1]:{} {public}\n",address.port()),"127.0.0.1",address.port()).unwrap();
        let directory=tempfile::tempdir().unwrap();
        let session=SshSession::connect(SessionId::new(),"zmodem".into(),SshConnect{
            username:"cshell".into(),verifier,authentication:super::SshAuthentication::Password("phase1".into())
        },TerminalSize::cells(24,80),&directory.path().join("zmodem.csjr"),16).await.unwrap();
        let status=tokio::time::timeout(std::time::Duration::from_secs(10),async{
            loop{if let Some(status)=session.zmodem_status(){break status;}
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
        }).await.unwrap();
        assert_eq!(status.direction,1);assert_eq!(status.state,1);
        assert!(session.send_input(&InputAction::Text("should never reach rz".into())).is_err());
        session.zmodem_control(&cshell_ipc::ZmodemRequest{operation:3,transfer_id:status.transfer_id,..Default::default()}).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10),async{
            loop{if let Some(frame)=session.snapshots.latest(){
                let text:String=frame.cells.iter().map(|c|c.character).collect();
                assert!(!text.contains("**"));
                if text.contains("ready$ "){break;}}
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
        }).await.unwrap();
        session.send_input(&InputAction::Text("hello".into())).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10),async{
            loop{if received.lock().unwrap().ends_with(b"hello"){break;}
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
        }).await.unwrap();
        let captured=received.lock().unwrap().clone();
        assert!(captured.starts_with(&[0x18;10]));
        assert!(!captured.windows(6).any(|w|w==b"should"));
        assert_eq!(session.zmodem_status().unwrap().state,4);
        session.close().await.unwrap();server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn strict_host_key_password_input_snapshot_and_close() {
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let mut config = russh::server::Config::default();
        config.keys.push(key);
        config.auth_rejection_time = std::time::Duration::from_millis(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::<u8>::new()));
        let capture = Arc::clone(&received);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let running =
                russh::server::run_stream(Arc::new(config), stream, EchoServer { received_input: Some(capture), ..Default::default() })
                    .await
                    .unwrap();
            let _ = running.await;
        });
        let verifier = KnownHostsVerifier::parse(
            &format!("[127.0.0.1]:{} {public}\n", address.port()),
            "127.0.0.1",
            address.port(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let session = SshSession::connect(
            SessionId::new(),
            "echo".into(),
            SshConnect {
                username: "cshell".into(),
                verifier,
                authentication: super::SshAuthentication::Password("phase1".into()),
            },
            TerminalSize::cells(24, 80),
            &directory.path().join("remote.csjr"),
            16,
        )
        .await
        .unwrap();
        // Encode through IPC, deliver over an authenticated SSH PTY channel,
        // and compare captured server bytes (not a terminal-rendered approximation).
        use cshell_domain::{KeyCode, KeyEvent, Modifiers};
        let mut actions = vec![InputAction::Text((b'!'..=b'~').map(char::from).collect()),
            InputAction::Text(" 中文 e\u{301} € 🙂 ".into())];
        let mut expected = (b'!'..=b'~').collect::<Vec<_>>();
        expected.extend_from_slice(" 中文 e\u{301} € 🙂 ".as_bytes());
        for (code, modifiers, encoded) in [
            (KeyCode::Character(" ".into()), Modifiers::default(), b" ".as_slice()),
            (KeyCode::Character(" ".into()), Modifiers {ctrl:true,..Default::default()}, b"\0"),
            (KeyCode::Character(" ".into()), Modifiers {alt:true,..Default::default()}, b"\x1b "),
            (KeyCode::Home, Modifiers::default(), b"\x1b[H"),
            (KeyCode::End, Modifiers::default(), b"\x1b[F"),
            (KeyCode::Insert, Modifiers::default(), b"\x1b[2~"),
            (KeyCode::Delete, Modifiers::default(), b"\x1b[3~"),
            (KeyCode::PageUp, Modifiers::default(), b"\x1b[5~"),
            (KeyCode::PageDown, Modifiers::default(), b"\x1b[6~"),
            (KeyCode::Tab, Modifiers::default(), b"\t"),
            (KeyCode::Backspace, Modifiers::default(), b"\x7f"),
            (KeyCode::Enter, Modifiers::default(), b"\r"),
        ] {
            actions.push(InputAction::Key(KeyEvent {code,modifiers,pressed:true,repeated:false}));
            expected.extend_from_slice(encoded);
        }
        actions.push(InputAction::Text("\n".into()));
        expected.push(b'\n');
        for action in actions {
            let wire = cshell_ipc::TerminalInputRequest::from_action(session.id(), &action).unwrap();
            session.send_input(&wire.decode_action().unwrap()).unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if received.lock().unwrap().len() >= expected.len() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert_eq!(*received.lock().unwrap(), expected, "SSH must receive every byte in order, including spaces and NUL");
        session
            .send_input(&InputAction::Text("hello\n".into()))
            .unwrap();
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(frame) = session.snapshots.latest() {
                    let text: String = frame.cells.iter().map(|cell| cell.character).collect();
                    if text.contains("ACK:hello") {
                        break frame;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(snapshot.generation > 0);
        tokio::time::timeout(std::time::Duration::from_secs(10), session.close())
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_exit_retains_history_and_explicit_new_shell_has_a_new_identity() {
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let mut config = russh::server::Config::default();
        config.keys.push(key);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = Arc::new(config);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    let running = russh::server::run_stream(config, stream, EchoServer::default())
                        .await
                        .unwrap();
                    let _ = running.await;
                });
            }
        });
        let verifier = KnownHostsVerifier::parse(
            &format!("[127.0.0.1]:{} {public}\n", address.port()),
            "127.0.0.1",
            address.port(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let profile_id = ProfileId::new();
        let first_id = SessionId::new();
        let first = SshSession::connect(
            first_id,
            "saved profile".into(),
            SshConnect {
                username: "cshell".into(),
                verifier: verifier.clone(),
                authentication: super::SshAuthentication::Password("phase1".into()),
            },
            TerminalSize::cells(24, 80),
            &directory.path().join("first.csjr"),
            16,
        )
        .await
        .unwrap()
        .with_profile_id(profile_id);
        first
            .send_input(&InputAction::Text("before exit\n".into()))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !first.snapshots.latest().is_some_and(|frame| {
                frame
                    .cells
                    .iter()
                    .map(|cell| cell.character)
                    .collect::<String>()
                    .contains("ACK:before exit")
            }) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        first
            .send_input(&InputAction::Text("exit\n".into()))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while first.running() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let summary = first.summary();
        assert!(!summary.running);
        assert_eq!(
            summary.profile_id,
            Some(profile_id.as_uuid().as_bytes().to_vec())
        );
        assert_eq!(
            summary.terminal_detail,
            "Remote shell exited with status 42"
        );
        assert!(first.full_frame().is_some());
        assert!(matches!(
            first.send_input(&InputAction::Text("stale input\n".into())),
            Err(super::SshSessionError::Closed)
        ));
        let second = SshSession::connect(
            SessionId::new(),
            "saved profile · New Shell".into(),
            SshConnect {
                username: "cshell".into(),
                verifier,
                authentication: super::SshAuthentication::Password("phase1".into()),
            },
            TerminalSize::cells(24, 80),
            &directory.path().join("second.csjr"),
            16,
        )
        .await
        .unwrap()
        .with_profile_id(profile_id);
        assert_ne!(first.id(), second.id());
        assert!(second.running());
        assert_eq!(second.summary().profile_id, summary.profile_id);
        first.close().await.unwrap();
        second.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn strict_host_key_private_key_session_opens_terminal() {
        let host_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let host_public = host_key.public_key().to_openssh().unwrap();
        let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let private_key = cshell_ssh::SshPrivateKey::decode_openssh(
            &client_key.to_openssh(LineEnding::LF).unwrap(),
            None,
        )
        .unwrap();
        let mut config = russh::server::Config::default();
        config.keys.push(host_key);
        config.auth_rejection_time = std::time::Duration::from_millis(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted_public_key = client_key.public_key().clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let running = russh::server::run_stream(
                Arc::new(config),
                stream,
                EchoServer {
                    zmodem_greeting: None,
                    received_input: None,
                    channels: HashMap::new(),
                    accepted_public_key: Some(accepted_public_key),
                    sftp_state: Arc::default(),
                    deny_terminal:false,terminal_requests:None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let _ = running.await;
        });
        let verifier = KnownHostsVerifier::parse(
            &format!("[127.0.0.1]:{} {host_public}\n", address.port()),
            "127.0.0.1",
            address.port(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let session = SshSession::connect(
            SessionId::new(),
            "key".into(),
            SshConnect {
                username: "cshell".into(),
                verifier,
                authentication: super::SshAuthentication::PrivateKey(private_key),
            },
            TerminalSize::cells(24, 80),
            &directory.path().join("key.csjr"),
            16,
        )
        .await
        .unwrap();
        session.close().await.unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_profile_private_key_reference_opens_verified_session() {
        use crate::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
        use cshell_domain::{
            ProfileId, ProfileKind, ProfileRecord, SshAuthMethod, SshConnectionRecord,
            TerminalOverrides,
        };
        use cshell_ipc::{
            Envelope, ProfileChange, ProfileOperation, ProfileRecordData, ProfileRequest,
            SessionCreateRequest, SshConnectionData, envelope, profile_change, read_envelope,
            write_envelope,
        };
        use cshell_storage::SqliteProfileRepository;
        use std::collections::BTreeSet;

        let host_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let host_public = host_key.public_key().to_openssh().unwrap();
        let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let accepted_public_key = client_key.public_key().clone();
        let mut config = russh::server::Config::default();
        config.keys.push(host_key);
        config.auth_rejection_time = std::time::Duration::from_millis(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let running = russh::server::run_stream(
                Arc::new(config),
                stream,
                EchoServer {
                    zmodem_greeting: None,
                    received_input: None,
                    channels: HashMap::new(),
                    accepted_public_key: Some(accepted_public_key),
                    sftp_state: Arc::default(),
                    deny_terminal:false,terminal_requests:None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let _ = running.await;
        });
        let temp = tempfile::tempdir().unwrap();
        let private_key_path = temp.path().join("id_ed25519");
        std::fs::write(
            &private_key_path,
            client_key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        let known_hosts = temp.path().join("known_hosts");
        std::fs::write(
            &known_hosts,
            format!("[127.0.0.1]:{} {host_public}\n", address.port()),
        )
        .unwrap();
        let repository = SqliteProfileRepository::open(temp.path().join("profiles.db"))
            .await
            .unwrap();
        let profiles = Arc::new(ProfileIpcService::new(repository));
        let profile_id = ProfileId::new();
        let created = profiles
            .handle(ProfileRequest {
                operation: ProfileOperation::ApplyChanges as i32,
                expected_revision: 0,
                changes: vec![
                    ProfileChange {
                        change: Some(profile_change::Change::UpsertProfile(
                            ProfileRecordData::from(&ProfileRecord {
                                id: profile_id,
                                name: "key profile".into(),
                                kind: ProfileKind::Ssh,
                                folder_id: None,
                                tags: BTreeSet::new(),
                                favorite: false,
                                terminal: TerminalOverrides::default(),
                            }),
                        )),
                    },
                    ProfileChange {
                        change: Some(profile_change::Change::UpsertSshConnection(
                            SshConnectionData::from(&SshConnectionRecord {
                                options: Default::default(),
                                profile_id,
                                host: "127.0.0.1".into(),
                                port: address.port(),
                                username: "cshell".into(),
                                auth_method: SshAuthMethod::PrivateKey,
                                private_key_path: Some(private_key_path.to_str().unwrap().into()),
                                certificate_path: None,
                                agent_backend: Default::default(),
                                agent_identity: None,
                                route: Default::default(),
                            }),
                        )),
                    },
                ],
                import_json: Vec::new(),
                import_policy: 0,
                credential_profile_id: Vec::new(),
                credential_secret: Vec::new(),
                host_key_token: Vec::new(),
                host_key_fingerprint: String::new(),
                include_local_context: false,
                export_profile_ids: Vec::new(),
            })
            .await;
        assert_eq!(created.status, 0, "{}", created.detail);
        let local = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 16).unwrap());
        let service = SessionIpcService::new(local)
            .with_known_hosts_path(known_hosts)
            .with_profiles(profiles);
        let (mut client, mut daemon) = tokio::io::duplex(64 * 1024);
        let client_work = async {
            write_envelope(
                &mut client,
                &Envelope {
                    request_id: 3,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::SessionCreateRequest(
                        SessionCreateRequest {connection_diagnostics:false,
                            source_ssh_session_id: None,
                            local_launch: None,
                            rows: 24,
                            cols: 80,
                            profile_id: Some(profile_id.as_uuid().as_bytes().to_vec()),
                        },
                    )),
                },
            )
            .await
            .unwrap();
            read_envelope(&mut client).await.unwrap()
        };
        let (served, response) = tokio::join!(service.serve_one(&mut daemon), client_work);
        served.unwrap();
        let Some(envelope::Payload::SessionCreateResponse(created)) = response.payload else {
            panic!("missing session response")
        };
        assert!(created.session.is_some(), "{}", created.detail);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_profile_ipc_uses_keychain_and_verified_host() {
        if std::env::var_os("CSHELL_KEYCHAIN_NATIVE_TEST").is_none() {
            return;
        }
        use crate::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
        use cshell_domain::{
            ProfileId, ProfileKind, ProfileRecord, SshConnectionRecord, TerminalOverrides,
        };
        use cshell_ipc::{
            Envelope, ProfileChange, ProfileOperation, ProfileRecordData, ProfileRequest,
            SessionCreateRequest, SnapshotRequest, SshConnectionData, TerminalInputRequest,
            envelope, profile_change, read_envelope, write_envelope,
        };
        use cshell_storage::SqliteProfileRepository;
        use cshell_vault::{ProfilePasswordRef, SystemProfilePasswordVault};
        use std::collections::BTreeSet;

        async fn exchange(service: &SessionIpcService, payload: envelope::Payload) -> Envelope {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let client_work = async {
                write_envelope(
                    &mut client,
                    &Envelope {
                        request_id: 42,
                        deadline_unix_ms: 0,
                        payload: Some(payload),
                    },
                )
                .await
                .unwrap();
                read_envelope(&mut client).await.unwrap()
            };
            let (served, response) = tokio::join!(service.serve_one(&mut server), client_work);
            served.unwrap();
            response
        }
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let mut config = russh::server::Config::default();
        config.keys.push(key);
        config.auth_rejection_time = std::time::Duration::from_millis(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let running =
                russh::server::run_stream(Arc::new(config), stream, EchoServer::default())
                    .await
                    .unwrap();
            let _ = running.await;
        });
        let temp = tempfile::tempdir().unwrap();
        let known_hosts = temp.path().join("known_hosts");
        std::fs::write(
            &known_hosts,
            format!("[127.0.0.1]:{} {public}\n", address.port()),
        )
        .unwrap();
        let repository = SqliteProfileRepository::open(temp.path().join("profiles.db"))
            .await
            .unwrap();
        let local = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 16).unwrap());
        let service = SessionIpcService::new(local)
            .with_known_hosts_path(known_hosts)
            .with_profiles(Arc::new(ProfileIpcService::new(repository)));
        let profile_id = ProfileId::new();
        let reference = ProfilePasswordRef::from_profile_bytes(*profile_id.as_uuid().as_bytes());

        let mut create = ProfileRequest {
            operation: ProfileOperation::ApplyChanges as i32,
            expected_revision: 0,
            changes: vec![
                ProfileChange {
                    change: Some(profile_change::Change::UpsertProfile(
                        ProfileRecordData::from(&ProfileRecord {
                            id: profile_id,
                            name: "loopback".into(),
                            kind: ProfileKind::Ssh,
                            folder_id: None,
                            tags: BTreeSet::new(),
                            favorite: false,
                            terminal: TerminalOverrides::default(),
                        }),
                    )),
                },
                ProfileChange {
                    change: Some(profile_change::Change::UpsertSshConnection(
                        SshConnectionData::from(&SshConnectionRecord {
                            options: Default::default(),
                            profile_id,
                            host: "127.0.0.1".into(),
                            port: address.port(),
                            username: "cshell".into(),
                            auth_method: Default::default(),
                            private_key_path: None,
                            certificate_path: None,
                            agent_backend: Default::default(),
                            agent_identity: None,
                            route: Default::default(),
                        }),
                    )),
                },
            ],
            import_json: vec![],
            import_policy: 0,
            credential_profile_id: vec![],
            credential_secret: vec![],
            host_key_token: Vec::new(),
            host_key_fingerprint: String::new(),
            include_local_context: false,
            export_profile_ids: Vec::new(),
        };
        let result = exchange(&service, envelope::Payload::ProfileRequest(create.clone())).await;
        let Some(envelope::Payload::ProfileResponse(created)) = result.payload else {
            panic!("missing Profile response")
        };
        assert_eq!(created.status, 0, "{}", created.detail);
        create.operation = ProfileOperation::SetPassword as i32;
        create.expected_revision = created.revision;
        create.changes.clear();
        create.credential_profile_id = profile_id.as_uuid().as_bytes().to_vec();
        create.credential_secret = b"phase1".to_vec();
        let result = exchange(&service, envelope::Payload::ProfileRequest(create)).await;
        let Some(envelope::Payload::ProfileResponse(saved)) = result.payload else {
            panic!("missing password response")
        };
        assert_eq!(saved.status, 0, "{}", saved.detail);
        let result = exchange(
            &service,
            envelope::Payload::SessionCreateRequest(SessionCreateRequest {connection_diagnostics:false,
                source_ssh_session_id: None,
                local_launch: None,
                rows: 24,
                cols: 80,
                profile_id: Some(profile_id.as_uuid().as_bytes().to_vec()),
            }),
        )
        .await;
        let Some(envelope::Payload::SessionCreateResponse(created)) = result.payload else {
            panic!("missing session response")
        };
        let session = created
            .session
            .unwrap_or_else(|| panic!("{}", created.detail));
        let session_id = SessionId::from_bytes(session.session_id.as_slice().try_into().unwrap());
        let input =
            TerminalInputRequest::from_action(session_id, &InputAction::Text("hello\n".into()))
                .unwrap();
        let result = exchange(&service, envelope::Payload::TerminalInputRequest(input)).await;
        assert!(matches!(
            result.payload,
            Some(envelope::Payload::TerminalControlResponse(_))
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let result = exchange(
                    &service,
                    envelope::Payload::SnapshotRequest(SnapshotRequest {
                        session_id: session_id.as_uuid().as_bytes().to_vec(),
                        current_generation: None,
                    }),
                )
                .await;
                let Some(envelope::Payload::FullFrame(frame)) = result.payload else {
                    panic!("missing frame")
                };
                let snapshot = frame.decode_terminal_snapshot().unwrap();
                let text: String = snapshot.cells.iter().map(|cell| cell.character).collect();
                if text.contains("ACK:hello") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let result = exchange(
            &service,
            envelope::Payload::SessionCloseRequest(cshell_ipc::SessionCloseRequest {
                apply_view_policy: false,
                session_id: session_id.as_uuid().as_bytes().to_vec(),
            }),
        )
        .await;
        assert!(matches!(
            result.payload,
            Some(envelope::Payload::SessionCloseResponse(_))
        ));
        SystemProfilePasswordVault::new()
            .delete(&reference)
            .unwrap();
        server.abort();
    }
}
