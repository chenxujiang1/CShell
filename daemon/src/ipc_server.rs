use crate::{SessionIpcError, SessionIpcService};
use cshell_ipc::{
    HandshakePolicy, HandshakeProtocolError, features, server_handshake, transport::LocalListener,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinSet;

const DEFAULT_MAX_CONNECTIONS: usize = 512;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcServerStats {
    pub accepted_connections: u64,
    pub rejected_connections: u64,
    pub completed_connections: u64,
}

#[derive(Debug, Default)]
struct SharedStats {
    accepted: AtomicU64,
    rejected: AtomicU64,
    completed: AtomicU64,
}

impl SharedStats {
    fn snapshot(&self) -> IpcServerStats {
        IpcServerStats {
            accepted_connections: self.accepted.load(Ordering::Relaxed),
            rejected_connections: self.rejected.load(Ordering::Relaxed),
            completed_connections: self.completed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Error)]
pub enum IpcServerError {
    #[error("cannot accept local IPC connection: {0}")]
    Accept(std::io::Error),
    #[error("daemon IPC shutdown channel closed unexpectedly")]
    ShutdownChannelClosed,
}

#[derive(Debug, Error)]
enum ConnectionError {
    #[error(transparent)]
    Handshake(#[from] HandshakeProtocolError),
    #[error("client did not negotiate a supported session service")]
    SessionServiceRequired,
    #[error(transparent)]
    Session(#[from] SessionIpcError),
}

/// Authenticated, concurrent local IPC accept loop.
#[derive(Debug)]
pub struct SessionIpcServer {
    listener: LocalListener,
    handshake_policy: HandshakePolicy,
    service: SessionIpcService,
    stats: Arc<SharedStats>,
    max_connections: usize,
}

impl SessionIpcServer {
    #[must_use]
    pub fn new(
        listener: LocalListener,
        handshake_policy: HandshakePolicy,
        service: SessionIpcService,
    ) -> Self {
        Self {
            listener,
            handshake_policy,
            service,
            stats: Arc::new(SharedStats::default()),
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }

    #[cfg(test)]
    fn with_connection_limit(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections.max(1);
        self
    }

    #[must_use]
    pub fn stats(&self) -> IpcServerStats {
        self.stats.snapshot()
    }

    pub async fn run_until(
        &self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<IpcServerStats, IpcServerError> {
        let mut connections = JoinSet::new();
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                accepted = self.listener.accept() => {
                    let stream = accepted.map_err(IpcServerError::Accept)?;
                    self.stats.accepted.fetch_add(1, Ordering::Relaxed);
                    if connections.len() >= self.max_connections {
                        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            max_connections = self.max_connections,
                            "local IPC connection rejected at concurrency limit"
                        );
                        drop(stream);
                        continue;
                    }
                    let policy = self.handshake_policy.clone();
                    let service = self.service.clone();
                    let stats = Arc::clone(&self.stats);
                    connections.spawn(async move {
                        let result = serve_authenticated_connection(stream, &policy, &service).await;
                        match result {
                            Ok(()) => {
                                stats.completed.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(error) => {
                                stats.rejected.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(%error, "local IPC connection ended with an error");
                            }
                        }
                    });
                }
                changed = shutdown.changed() => {
                    changed.map_err(|_| IpcServerError::ShutdownChannelClosed)?;
                }
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = joined {
                        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%error, "local IPC connection task panicked");
                    }
                }
            }
        }

        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(self.stats())
    }
}

async fn serve_authenticated_connection<S>(
    mut stream: S,
    policy: &HandshakePolicy,
    service: &SessionIpcService,
) -> Result<(), ConnectionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let negotiated = server_handshake(&mut stream, policy).await?;
    if negotiated.feature_bits
        & (features::FULL_FRAME_RECOVERY
            | features::LOG_PAGING
            | features::TERMINAL_CONTROL
            | features::HISTORY_SEARCH
            | features::PROFILE_CONTROL
            | features::SSH_PROFILE_TARGET
            | features::SSH_PROFILE_SESSION
            | features::SSH_PROFILE_AUTH
            | features::SSH_HOST_KEY_IMPORT
            | features::SSH_PROFILE_ROUTE
            | features::LOCAL_PROFILE
            | features::SSH_SESSION_STATUS)
        == 0
    {
        return Err(ConnectionError::SessionServiceRequired);
    }
    service
        .serve_connection_with_features(stream, negotiated.feature_bits)
        .await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::SessionIpcServer;
    use crate::{LocalSessionRegistry, SessionIpcService};
    use cshell_domain::{InputAction, TerminalSize};
    use cshell_ipc::{
        Envelope, Handshake, HandshakePolicy, LogPageRequest, SessionListRequest, SnapshotRequest,
        TerminalControlStatus, TerminalInputRequest, client_handshake, envelope, features,
        read_envelope, transport, write_envelope,
    };
    use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn long_running_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "IPC accept loop probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "echo CSHELL_SERVER_LOG & ping -n 30 127.0.0.1 >nul".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "IPC accept loop probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-lc".to_owned(),
                "printf 'CSHELL_SERVER_LOG\\n'; sleep 30".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    fn slow_flood_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "IPC slow subscriber flood probe".to_owned(),
            program: PathBuf::from("powershell.exe"),
            args: vec!["-NoLogo".to_owned(), "-NoProfile".to_owned()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "IPC slow subscriber flood probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: Vec::new(),
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    fn slow_flood_command() -> InputAction {
        #[cfg(windows)]
        return InputAction::Text(
            "1..400 | ForEach-Object { [Console]::WriteLine(('CSHELL_SLOW_{0:D4}' -f $_)); Start-Sleep -Milliseconds 10 }\r"
                .to_owned(),
        );

        #[cfg(not(windows))]
        InputAction::Text(
            "i=0; while [ $i -lt 400 ]; do printf 'CSHELL_SLOW_%04d\\n' \"$i\"; i=$((i+1)); sleep 0.01; done\r"
                .to_owned(),
        )
    }

    async fn connect_and_hold(
        endpoint: &str,
        token: [u8; 32],
        request_id: u64,
    ) -> cshell_ipc::transport::ClientStream {
        #[cfg(windows)]
        let mut client = transport::connect(endpoint).await.unwrap();
        #[cfg(unix)]
        let mut client = transport::connect(std::path::Path::new(endpoint))
            .await
            .unwrap();
        let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
        handshake.feature_bits = features::TERMINAL_CONTROL;
        client_handshake(&mut client, request_id, handshake)
            .await
            .unwrap();
        write_envelope(
            &mut client,
            &Envelope {
                request_id: request_id.saturating_add(1),
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::SessionListRequest(SessionListRequest {})),
            },
        )
        .await
        .unwrap();
        let response = read_envelope(&mut client).await.unwrap();
        assert!(matches!(
            response.payload,
            Some(envelope::Payload::SessionListResponse(_))
        ));
        client
    }

    async fn subscribe_and_hold(
        endpoint: &str,
        token: [u8; 32],
        session_id: cshell_domain::SessionId,
        request_id: u64,
    ) -> (cshell_ipc::transport::ClientStream, u64) {
        #[cfg(windows)]
        let mut client = transport::connect(endpoint).await.unwrap();
        #[cfg(unix)]
        let mut client = transport::connect(std::path::Path::new(endpoint))
            .await
            .unwrap();
        let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
        handshake.feature_bits = features::FULL_FRAME_RECOVERY;
        client_handshake(&mut client, request_id, handshake)
            .await
            .unwrap();
        write_envelope(
            &mut client,
            &Envelope {
                request_id: request_id.saturating_add(1),
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
        let Some(envelope::Payload::FullFrame(frame)) = response.payload else {
            panic!("snapshot subscription returned an unexpected payload");
        };
        (client, frame.generation)
    }

    #[tokio::test]
    async fn connection_limit_rejects_excess_clients_and_recovers_capacity() {
        const CONNECTION_LIMIT: usize = 4;
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-server-limit-test-{}", std::process::id());
        #[cfg(windows)]
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        #[cfg(unix)]
        let endpoint_path = directory.path().join("server-limit.sock");
        #[cfg(unix)]
        let endpoint = endpoint_path.to_string_lossy().into_owned();
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&endpoint_path).unwrap();

        let token = [0x4c; 32];
        let server = Arc::new(
            SessionIpcServer::new(
                listener,
                HandshakePolicy::new(token, features::TERMINAL_CONTROL),
                SessionIpcService::new(registry),
            )
            .with_connection_limit(CONNECTION_LIMIT),
        );
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run_until(shutdown_receiver).await })
        };

        let mut held = Vec::with_capacity(CONNECTION_LIMIT);
        for index in 0..CONNECTION_LIMIT {
            held.push(connect_and_hold(&endpoint, token, 10 + index as u64 * 2).await);
        }

        #[cfg(windows)]
        let mut excess = transport::connect(&endpoint).await.unwrap();
        #[cfg(unix)]
        let mut excess = transport::connect(&endpoint_path).await.unwrap();
        let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
        handshake.feature_bits = features::TERMINAL_CONTROL;
        let rejected = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_handshake(&mut excess, 90, handshake),
        )
        .await
        .unwrap();
        assert!(rejected.is_err());

        drop(held.pop());
        let capacity_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while server.stats().completed_connections == 0 {
            assert!(tokio::time::Instant::now() < capacity_deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let recovered = connect_and_hold(&endpoint, token, 100).await;
        drop(recovered);
        drop(held);

        shutdown_sender.send(true).unwrap();
        let stats = server_task.await.unwrap().unwrap();
        assert_eq!(stats.accepted_connections, 6);
        assert!(stats.rejected_connections >= 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slow_subscribers_under_pty_flood_do_not_block_a_control_connection() {
        const SLOW_CLIENTS: usize = 100;
        const MIN_FLOOD_GENERATIONS: u64 = 100;
        // Keep the generation threshold deterministic while allowing loaded
        // native runners enough time to start the shell and emit the paced
        // four-second flood. The separate two-second control deadline remains
        // the isolation requirement this test is intended to enforce.
        const FLOOD_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 256).unwrap());
        let attachment = registry
            .spawn_local(&slow_flood_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();

        #[cfg(windows)]
        let endpoint = format!(
            r"\\.\pipe\cshell-server-slow-test-{}-{}",
            std::process::id(),
            session_id
        );
        #[cfg(windows)]
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        #[cfg(unix)]
        let endpoint_path = directory.path().join("server-slow.sock");
        #[cfg(unix)]
        let endpoint = endpoint_path.to_string_lossy().into_owned();
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&endpoint_path).unwrap();

        let token = [0x73; 32];
        let server = Arc::new(SessionIpcServer::new(
            listener,
            HandshakePolicy::new(
                token,
                features::FULL_FRAME_RECOVERY | features::TERMINAL_CONTROL,
            ),
            SessionIpcService::new(Arc::clone(&registry)),
        ));
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run_until(shutdown_receiver).await })
        };

        let mut slow_clients = Vec::with_capacity(SLOW_CLIENTS);
        let mut initial_generation = 0;
        for index in 0..SLOW_CLIENTS {
            let request_id = 10 + u64::try_from(index).unwrap() * 2;
            let (client, generation) =
                subscribe_and_hold(&endpoint, token, session_id, request_id).await;
            initial_generation = initial_generation.max(generation);
            slow_clients.push(client);
        }

        attachment.send_input(&slow_flood_command()).unwrap();
        let flood_deadline = tokio::time::Instant::now() + FLOOD_START_TIMEOUT;
        loop {
            let generation = attachment.full_frame().unwrap().generation;
            if generation >= initial_generation.saturating_add(MIN_FLOOD_GENERATIONS) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < flood_deadline,
                "PTY output did not produce enough terminal generations"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let control_started = tokio::time::Instant::now();
        let control_result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            #[cfg(windows)]
            let mut control = transport::connect(&endpoint).await.unwrap();
            #[cfg(unix)]
            let mut control = transport::connect(&endpoint_path).await.unwrap();
            let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
            handshake.feature_bits = features::TERMINAL_CONTROL;
            client_handshake(&mut control, 100, handshake)
                .await
                .unwrap();
            let input =
                TerminalInputRequest::from_action(session_id, &InputAction::Text(String::new()))
                    .unwrap();
            write_envelope(
                &mut control,
                &Envelope {
                    request_id: 101,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::TerminalInputRequest(input)),
                },
            )
            .await
            .unwrap();
            let response = read_envelope(&mut control).await.unwrap();
            assert_eq!(response.request_id, 101);
            let Some(envelope::Payload::TerminalControlResponse(response)) = response.payload
            else {
                panic!("terminal input returned an unexpected payload");
            };
            assert_eq!(
                response.decoded_status().unwrap(),
                TerminalControlStatus::Accepted
            );
        })
        .await;
        assert!(
            control_result.is_ok(),
            "slow subscribers blocked an independent control connection after {:?}; generation={}, stats={:?}",
            control_started.elapsed(),
            attachment.full_frame().unwrap().generation,
            server.stats(),
        );

        drop(slow_clients);
        shutdown_sender.send(true).unwrap();
        let stats = server_task.await.unwrap().unwrap();
        assert_eq!(
            stats.accepted_connections,
            u64::try_from(SLOW_CLIENTS).unwrap() + 1
        );
        registry.close(session_id).unwrap();
    }

    #[tokio::test]
    async fn accept_loop_authenticates_and_serves_a_snapshot_subscription() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&long_running_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        drop(attachment);

        #[cfg(windows)]
        let endpoint = format!(
            r"\\.\pipe\cshell-server-test-{}-{}",
            std::process::id(),
            session_id
        );
        #[cfg(windows)]
        let listener = transport::LocalListener::bind(&endpoint).unwrap();

        #[cfg(unix)]
        let endpoint = directory.path().join("server.sock");
        #[cfg(unix)]
        let listener = transport::LocalListener::bind(&endpoint).unwrap();

        let token = [0x6d; 32];
        let server = Arc::new(SessionIpcServer::new(
            listener,
            HandshakePolicy::new(token, features::FULL_FRAME_RECOVERY | features::LOG_PAGING),
            SessionIpcService::new(Arc::clone(&registry)),
        ));
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let server_task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move { server.run_until(shutdown_receiver).await })
        };

        #[cfg(windows)]
        let mut client = transport::connect(&endpoint).await.unwrap();
        #[cfg(unix)]
        let mut client = transport::connect(&endpoint).await.unwrap();

        let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
        handshake.feature_bits = features::FULL_FRAME_RECOVERY | features::LOG_PAGING;
        client_handshake(&mut client, 1, handshake).await.unwrap();
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 2,
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
        assert!(matches!(
            response.payload,
            Some(envelope::Payload::FullFrame(_))
        ));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut log_request_id = 3_u64;
        loop {
            write_envelope(
                &mut client,
                &Envelope {
                    request_id: log_request_id,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::LogPageRequest(LogPageRequest {
                        session_id: session_id.as_uuid().as_bytes().to_vec(),
                        anchor_line_id: None,
                        cell_offset: 0,
                        rows_before: 32,
                        rows_after: 0,
                    })),
                },
            )
            .await
            .unwrap();
            let page = loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let response = tokio::time::timeout(remaining, read_envelope(&mut client))
                    .await
                    .unwrap()
                    .unwrap();
                if response.request_id != log_request_id {
                    continue;
                }
                let Some(envelope::Payload::LogPage(page)) = response.payload else {
                    panic!("log page request returned an unexpected payload");
                };
                break page;
            };
            page.validate().unwrap();
            if page
                .rows
                .iter()
                .any(|row| row.text.contains("CSHELL_SERVER_LOG"))
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            log_request_id = log_request_id.saturating_add(1);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(client);

        // A dedicated paging connection intentionally has no terminal frame
        // subscription, so LOG_PAGING alone is a valid negotiated service.
        let mut log_client = transport::connect(&endpoint).await.unwrap();
        let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
        handshake.feature_bits = features::LOG_PAGING;
        client_handshake(&mut log_client, 100, handshake)
            .await
            .unwrap();
        write_envelope(
            &mut log_client,
            &Envelope {
                request_id: 101,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::LogPageRequest(LogPageRequest {
                    session_id: session_id.as_uuid().as_bytes().to_vec(),
                    anchor_line_id: None,
                    cell_offset: 0,
                    rows_before: 32,
                    rows_after: 0,
                })),
            },
        )
        .await
        .unwrap();
        let response = read_envelope(&mut log_client).await.unwrap();
        assert!(matches!(
            response.payload,
            Some(envelope::Payload::LogPage(_))
        ));
        drop(log_client);

        shutdown_sender.send(true).unwrap();
        let stats = server_task.await.unwrap().unwrap();
        assert_eq!(stats.accepted_connections, 2);
        registry.close(session_id).unwrap();
    }
}
