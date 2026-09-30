use crate::{LatestSnapshot, PipelineIngress, TerminalFrameSubscription, TerminalPipeline};
use cshell_domain::{InputAction, ProfileId, SessionId, TerminalSize};
use cshell_ipc::{FullFrame, SessionSummary};
use cshell_output_store::JournalLineIndex;
use cshell_ssh::{KnownHostsVerifier, RusshClient, SshError, SshTerminalWriter, TerminalEvent};
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
    Input(Vec<u8>),
    Resize(TerminalSize),
}

#[derive(Debug)]
pub struct SshSession {
    id: SessionId,
    title: String,
    profile_id: Option<ProfileId>,
    terminal_detail: Arc<Mutex<String>>,
    client: Arc<RusshClient>,
    jump: Option<crate::ssh_route::JumpConnection>,
    writer: Arc<SshTerminalWriter>,
    pipeline: Mutex<Option<TerminalPipeline>>,
    ingress: Mutex<Option<PipelineIngress>>,
    snapshots: LatestSnapshot,
    clipboard: crate::clipboard::ClipboardInbox,
    line_index: JournalLineIndex,
    commands: tokio::sync::mpsc::Sender<Command>,
    closed: Arc<AtomicBool>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SshSession {
    pub async fn connect(
        id: SessionId,
        title: String,
        connect: SshConnect,
        size: TerminalSize,
        journal_path: &Path,
        ingress_capacity: usize,
    ) -> Result<Self, SshSessionError> {
        let client = match connect.authentication {
            SshAuthentication::Password(password) => {
                RusshClient::connect_password_known_hosts(
                    connect.username,
                    password,
                    connect.verifier,
                )
                .await?
            }
            SshAuthentication::PrivateKey(private_key) => {
                RusshClient::connect_public_key_known_hosts(
                    connect.username,
                    private_key,
                    connect.verifier,
                )
                .await?
            }
            SshAuthentication::Certificate {
                private_key,
                certificate,
            } => {
                RusshClient::connect_certificate_known_hosts(
                    connect.username,
                    private_key,
                    *certificate,
                    connect.verifier,
                )
                .await?
            }
            SshAuthentication::Agent {
                backend,
                identity_fingerprint,
            } => {
                RusshClient::connect_agent_known_hosts_identity(
                    connect.username,
                    connect.verifier,
                    backend,
                    identity_fingerprint.as_deref(),
                )
                .await?
            }
        };
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
        let crate::ssh_route::RoutedSshConnection { client, jump } = connection;
        let client = Arc::new(client);
        let terminal = client
            .open_terminal(u32::from(size.rows), u32::from(size.cols))
            .await?;
        let (mut reader, writer) = terminal.split();
        let writer = Arc::new(writer);
        let clipboard = crate::clipboard::ClipboardInbox::default();
        let pipeline = TerminalPipeline::spawn_with_clipboard(
            journal_path,
            size,
            ingress_capacity,
            clipboard.clone(),
        )?;
        let ingress = pipeline.ingress().ok_or(SshSessionError::Closed)?;
        let snapshots = pipeline.snapshots();
        let line_index = pipeline.line_index();
        let responses = pipeline.responses();
        let closed = Arc::new(AtomicBool::new(false));
        let terminal_detail = Arc::new(Mutex::new(String::new()));
        let reader_detail = Arc::clone(&terminal_detail);
        let (commands, mut receiver) = tokio::sync::mpsc::channel(256);
        let reader_ingress = ingress.clone();
        let reader_closed = Arc::clone(&closed);
        let reader_client = Arc::clone(&client);
        let reader_jump = jump.clone();
        let reader_clipboard = clipboard.clone();
        let reader_task = tokio::spawn(async move {
            while let Some(event) = reader.next_event().await {
                match event {
                    TerminalEvent::Data { data, .. } => {
                        let ingress = reader_ingress.clone();
                        let result =
                            tokio::task::spawn_blocking(move || ingress.submit_blocking(data))
                                .await;
                        if !matches!(result, Ok(Ok(()))) {
                            set_terminal_detail(
                                &reader_detail,
                                "SSH output storage failed; the terminal is disconnected".into(),
                            );
                            break;
                        }
                    }
                    TerminalEvent::ExitStatus(code) => set_terminal_detail(
                        &reader_detail,
                        format!("Remote shell exited with status {code}"),
                    ),
                    TerminalEvent::ExitSignal { .. } => set_terminal_detail(
                        &reader_detail,
                        "Remote shell exited due to a signal".into(),
                    ),
                    TerminalEvent::Closed => break,
                    TerminalEvent::Eof => {}
                    _ => {}
                }
            }
            set_terminal_detail(
                &reader_detail,
                "SSH disconnected without an exit status; remote process outcome is unknown".into(),
            );
            reader_closed.store(true, Ordering::Release);
            reader_clipboard.handle(cshell_ipc::ClipboardOperation::Poll, &[], false);
            let _ = reader_client.disconnect().await;
            if let Some(jump) = reader_jump {
                jump.disconnect().await;
            }
        });
        let command_writer = Arc::clone(&writer);
        let command_closed = Arc::clone(&closed);
        let command_detail = Arc::clone(&terminal_detail);
        let command_client = Arc::clone(&client);
        let command_clipboard = clipboard.clone();
        let writer_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut clipboard_cleanup = std::time::Instant::now();
            loop {
                tokio::select! {
                    biased;
                    _ = tick.tick() => {
                        if clipboard_cleanup.elapsed() >= std::time::Duration::from_secs(1) {
                            command_clipboard.expire_pending();
                            clipboard_cleanup = std::time::Instant::now();
                        }
                        if command_closed.load(Ordering::Acquire) { return; }
                        for bytes in responses.drain() {
                            if command_writer.send_input(bytes).await.is_err() {
                                set_terminal_detail(&command_detail, "SSH input delivery failed; remote process outcome is unknown".into());
                                command_closed.store(true, Ordering::Release);
                                let _ = command_client.disconnect().await;
                                return;
                            }
                        }
                    }
                    command = receiver.recv() => {
                        let Some(command) = command else { break };
                        for bytes in responses.drain() {
                            if command_writer.send_input(bytes).await.is_err() {
                                set_terminal_detail(&command_detail, "SSH input delivery failed; remote process outcome is unknown".into());
                                command_closed.store(true, Ordering::Release);
                                let _ = command_client.disconnect().await;
                                return;
                            }
                        }
                        let result = match command {
                            Command::Input(bytes) => command_writer.send_input(bytes).await,
                            Command::Resize(size) => command_writer.resize(
                                u32::from(size.rows), u32::from(size.cols),
                                u32::from(size.pixel_width), u32::from(size.pixel_height),
                            ).await,
                        };
                        if result.is_err() {
                            set_terminal_detail(&command_detail, "SSH connection lost while sending input or resizing; remote process outcome is unknown".into());
                            break;
                        }
                    }
                }
            }
            command_closed.store(true, Ordering::Release);
            let _ = command_client.disconnect().await;
        });
        Ok(Self {
            id,
            title,
            profile_id: None,
            terminal_detail,
            client,
            jump,
            writer,
            pipeline: Mutex::new(Some(pipeline)),
            ingress: Mutex::new(Some(ingress)),
            snapshots,
            clipboard,
            line_index,
            commands,
            closed,
            reader_task: Mutex::new(Some(reader_task)),
            writer_task: Mutex::new(Some(writer_task)),
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
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
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.id.as_uuid().as_bytes().to_vec(),
            title: self.title.clone(),
            running: self.running(),
            generation: self.generation(),
            profile_id: self.profile_id.map(|id| id.as_uuid().as_bytes().to_vec()),
            terminal_detail: self
                .terminal_detail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
    pub async fn open_sftp(&self) -> Result<cshell_sftp::SftpClient, SshSessionError> {
        if !self.running() {
            return Err(SshSessionError::Closed);
        }
        Ok(self.client.open_sftp().await?)
    }

    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn running(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
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
        if !self.running() {
            return Err(SshSessionError::Closed);
        }
        let modes = self
            .snapshots
            .latest()
            .map_or_else(TerminalModes::default, |frame| frame.terminal_modes);
        let bytes = InputEncoder::encode(action, modes)?;
        if !bytes.is_empty() {
            self.commands
                .try_send(Command::Input(bytes))
                .map_err(|_| SshSessionError::Backpressure)?;
        }
        Ok(())
    }
    pub fn resize(&self, size: TerminalSize) -> Result<(), SshSessionError> {
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
        set_terminal_detail(&self.terminal_detail, "SSH session closed by user".into());
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
        let _ = self.writer.close().await;
        let _ = self.client.disconnect().await;
        if let Some(jump) = &self.jump {
            jump.disconnect().await;
        }
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
    sessions: RwLock<BTreeMap<SessionId, Arc<SshSession>>>,
}
impl SshSessionRegistry {
    pub fn insert(&self, session: SshSession) -> Arc<SshSession> {
        let session = Arc::new(session);
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
        channels: HashMap<ChannelId, Channel<Msg>>,
        accepted_public_key: Option<PublicKey>,
        sftp_state: Arc<Mutex<SftpFixtureState>>,
    }
    impl russh::server::Handler for EchoServer {
        type Error = russh::Error;
        async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if user == "cshell" && password == "phase1" {
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
            session.channel_success(channel)?;
            Ok(())
        }
        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            let channel = self.channels.remove(&channel).unwrap();
            session.channel_success(channel.id())?;
            let handle = session.handle();
            let channel_id = channel.id();
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
        files: HashMap<String, Vec<u8>>,
        handles: HashMap<String, String>,
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

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
            if path != "/" {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(Handle {
                id,
                handle: "directory".into(),
            })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
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
            let handle = format!("file-{id}");
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
            let state = self.state.lock().unwrap();
            let path = state.handles.get(&handle).ok_or(StatusCode::Failure)?;
            let bytes = state.files.get(path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
            if start >= bytes.len() {
                return Err(StatusCode::Eof);
            }
            let end = start.saturating_add(len as usize).min(bytes.len());
            Ok(Data {
                id,
                data: bytes[start..end].to_vec(),
            })
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<Status, Self::Error> {
            let mut state = self.state.lock().unwrap();
            let path = state
                .handles
                .get(&handle)
                .ok_or(StatusCode::Failure)?
                .clone();
            let bytes = state.files.get_mut(&path).ok_or(StatusCode::NoSuchFile)?;
            let start = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
            let end = start.checked_add(data.len()).ok_or(StatusCode::Failure)?;
            bytes.resize(end, 0);
            bytes[start..end].copy_from_slice(&data);
            Ok(Self::status(id))
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let state = self.state.lock().unwrap();
            let bytes = state.files.get(&path).ok_or(StatusCode::NoSuchFile)?;
            Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(bytes.len() as u64),
                    permissions: Some(0o100644),
                    ..FileAttributes::default()
                },
            })
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.stat(id, path).await
        }

        async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
            if self.state.lock().unwrap().files.remove(&path).is_none() {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(Self::status(id))
        }

        async fn rename(
            &mut self,
            id: u32,
            oldpath: String,
            newpath: String,
        ) -> Result<Status, Self::Error> {
            let mut state = self.state.lock().unwrap();
            if state.files.contains_key(&newpath) {
                return Err(StatusCode::Failure);
            }
            let bytes = state.files.remove(&oldpath).ok_or(StatusCode::NoSuchFile)?;
            state.files.insert(newpath, bytes);
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
        let session = SshSession::connect(
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
        registry.get(id).unwrap().close().await.unwrap();
        server.abort();
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
                    channels: HashMap::new(),
                    accepted_public_key: Some(accepted_public_key),
                    sftp_state: Arc::default(),
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
                    channels: HashMap::new(),
                    accepted_public_key: Some(accepted_public_key),
                    sftp_state: Arc::default(),
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
                        SessionCreateRequest {
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
            envelope::Payload::SessionCreateRequest(SessionCreateRequest {
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
