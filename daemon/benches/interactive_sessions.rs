//! Native P1-099 functional gate: independent real PTYs over authenticated local IPC.
use cshell_domain::{InputAction, SessionId, TerminalSize};
use cshell_ipc::{
    ClientFrameUpdate, Envelope, Handshake, HandshakePolicy, SessionCloseRequest,
    SessionListRequest, TerminalControlStatus, TerminalInputRequest, TerminalResizeRequest,
    TerminalSubscriptionReplica, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
use cshell_output_store::{DEFAULT_SEGMENT_BYTES, Durability, SegmentedJournalWriter};
use cshelld::{LocalSessionRegistry, SessionIpcServer, SessionIpcService};
use std::{
    collections::BTreeMap,
    env,
    io::{BufRead, Write},
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

const CHILD: &str = "--interactive-session-child";
const FEATURES: u64 =
    features::FULL_FRAME_RECOVERY | features::TERMINAL_CONTROL | features::LOCAL_LAUNCH_OPTIONS;
const TIMEOUT: Duration = Duration::from_secs(5);

type ProbeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> ExitCode {
    if env::args().nth(1).as_deref() == Some(CHILD) {
        return child();
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("cshelld=warn")
        .with_writer(std::io::stderr)
        .try_init();
    let result = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| runtime.block_on(run()).map_err(|error| error.to_string()));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("interactive session gate failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn child() -> ExitCode {
    let Some(index) = env::args()
        .nth(2)
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return ExitCode::FAILURE;
    };
    let result = (|| -> std::io::Result<()> {
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        writeln!(output, "READY:{index:02}")?;
        output.flush()?;
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if !line.trim().starts_with(&format!("s{index:02}:")) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "input reached the wrong session",
                ));
            }
            // An OS terminal echo cannot fake this transformed, session-specific response.
            let reversed: String = line.trim().chars().rev().collect();
            for row in 0..64 {
                writeln!(output, "\x1b[32mBURST:{index:02}:{row:02}\x1b[0m")?;
            }
            writeln!(output, "ACK:{index:02}:{reversed}")?;
            output.flush()?;
        }
    })();
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct Observer {
    index: usize,
    stream: transport::ClientStream,
    replica: TerminalSubscriptionReplica,
}

impl Observer {
    async fn observe(&mut self, marker: &str, size: TerminalSize) -> ProbeResult<()> {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Some(snapshot) = self.replica.snapshot() {
                    let text = snapshot
                        .cells
                        .chunks(usize::from(snapshot.cols))
                        .map(|row| row.iter().map(|cell| cell.character).collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\n");
                    for index in 0..64 {
                        if index != self.index && text.contains(&format!("ACK:{index:02}:")) {
                            return Err("output crossed session identities".into());
                        }
                    }
                    if text.contains(marker)
                        && snapshot.rows == size.rows
                        && snapshot.cols == size.cols
                    {
                        return Ok(());
                    }
                }
                let response = read_envelope(&mut self.stream).await.map_err(|error| {
                    format!("session {} subscriber failed: {error}", self.index)
                })?;
                if let ClientFrameUpdate::RequestFull(request) =
                    self.replica.apply_envelope(response)?
                {
                    write_envelope(
                        &mut self.stream,
                        &Envelope {
                            request_id: 99,
                            payload: Some(envelope::Payload::SnapshotRequest(request)),
                            ..Default::default()
                        },
                    )
                    .await?;
                }
            }
        })
        .await
        .map_err(|_| {
            format!(
                "session {} did not publish the expected output/size within 5 seconds",
                self.index
            )
        })?
    }
}

/// Covers failure paths as well as success: owned children and IPC tasks cannot outlive the probe.
struct Cleanup {
    registry: Arc<LocalSessionRegistry>,
    shutdown: tokio::sync::watch::Sender<bool>,
    server: tokio::task::JoinHandle<ProbeResult<()>>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.server.abort();
        if let Ok(sessions) = self.registry.list() {
            for session in sessions {
                let _ = self.registry.close(session.session_id);
            }
        }
    }
}

async fn run() -> ProbeResult<()> {
    let full = env::var_os("CSHELL_INTERACTIVE_GATE").is_some();
    let sessions = if full { 20 } else { 2 };
    let rounds = if full { 10 } else { 2 };
    let directory = tempfile::tempdir()?;
    let registry = Arc::new(LocalSessionRegistry::new(
        directory.path().join("journals"),
        32,
    )?);
    let mut ids = Vec::with_capacity(sessions);
    for index in 0..sessions {
        let profile = LocalProfile {
            name: format!("P1 interactive session {index:02}"),
            program: env::current_exe()?,
            args: vec![CHILD.into(), index.to_string()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };
        ids.push(
            registry
                .spawn_local(&profile, TerminalSize::cells(14, 80))?
                .session_id(),
        );
    }
    #[cfg(windows)]
    let endpoint = format!(
        r"\\.\pipe\cshell-interactive-{}-{}",
        std::process::id(),
        ids[0]
    );
    #[cfg(windows)]
    let listener = transport::LocalListener::bind(&endpoint)?;
    #[cfg(unix)]
    let endpoint_path = directory.path().join("interactive.sock");
    #[cfg(unix)]
    let endpoint = endpoint_path.to_string_lossy().into_owned();
    #[cfg(unix)]
    let listener = transport::LocalListener::bind(&endpoint_path)?;
    let token = rand::random::<[u8; 32]>();
    let server = Arc::new(SessionIpcServer::new(
        listener,
        HandshakePolicy::new(token, FEATURES),
        SessionIpcService::new(Arc::clone(&registry)),
    ));
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let server_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            server.run_until(stopped).await?;
            Ok(())
        })
    };
    let mut cleanup = Cleanup {
        registry: Arc::clone(&registry),
        shutdown,
        server: server_task,
    };
    let mut control = Control {
        stream: authenticated(&endpoint, token).await?,
        next_request: 3,
    };
    let mut observers = Vec::with_capacity(sessions);
    for (index, session) in ids.iter().copied().enumerate() {
        let mut observer = subscribe(&endpoint, token, index, session).await?;
        observer
            .observe(&format!("READY:{index:02}"), TerminalSize::cells(14, 80))
            .await?;
        observers.push(observer);
    }
    assert_sessions(&mut control, &ids).await?;
    let mut latencies = Vec::with_capacity(sessions * rounds);
    let mut reconnects = 0;
    let mut last_responses = Vec::new();
    for round in 0..rounds {
        let size = if round % 2 == 0 {
            TerminalSize::cells(18, 96)
        } else {
            TerminalSize::cells(14, 80)
        };
        let salt = rand::random::<u32>();
        let nonces: Vec<_> = (0..sessions)
            .map(|index| format!("s{index:02}:r{round:02}{salt:08x}"))
            .collect();
        let responses: Vec<String> = nonces
            .iter()
            .map(|nonce| nonce.chars().rev().collect())
            .collect();
        last_responses.clone_from(&responses);
        let mut started = Vec::with_capacity(sessions);
        for (index, session) in ids.iter().enumerate() {
            accepted(
                &mut control,
                envelope::Payload::TerminalResizeRequest(TerminalResizeRequest::from_size(
                    *session, size,
                )),
            )
            .await?;
            started.push(Instant::now());
            accepted(
                &mut control,
                envelope::Payload::TerminalInputRequest(TerminalInputRequest::from_action(
                    *session,
                    &InputAction::Text(format!("{}\r", nonces[index])),
                )?),
            )
            .await?;
        }
        // All children receive input before any subscriber drains its output burst.
        let mut readers = tokio::task::JoinSet::new();
        for mut observer in observers.drain(..) {
            let marker = format!("ACK:{:02}:{}", observer.index, responses[observer.index]);
            let start = started[observer.index];
            readers.spawn(async move {
                observer.observe(&marker, size).await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>((observer, start.elapsed()))
            });
        }
        while let Some(result) = readers.join_next().await {
            let (observer, latency) = result??;
            if latency > TIMEOUT {
                return Err(
                    "interactive input-to-frame time exceeded the 5-second functional budget"
                        .into(),
                );
            }
            latencies.push(latency);
            observers.push(observer);
        }
        observers.sort_by_key(|observer| observer.index);
        if round == rounds / 2 {
            // Dropping every client simulates GUI disconnection. Keep all PTYs alive.
            observers.clear();
            assert_sessions(&mut control, &ids).await?;
            for (index, session) in ids.iter().copied().enumerate() {
                let mut observer = subscribe(&endpoint, token, index, session).await?;
                observer
                    .observe(&format!("ACK:{index:02}:{}", responses[index]), size)
                    .await?;
                reconnects += 1;
                observers.push(observer);
            }
        }
        assert_sessions(&mut control, &ids).await?;
    }
    // Explicit view closure respects KeepAlive even under load; then terminate every child.
    let response = exchange(
        &mut control,
        envelope::Payload::SessionCloseRequest(SessionCloseRequest {
            session_id: ids[0].as_uuid().as_bytes().to_vec(),
            apply_view_policy: true,
        }),
    )
    .await?;
    if !matches!(response, envelope::Payload::SessionCloseResponse(response) if response.session_id == ids[0].as_uuid().as_bytes())
    {
        return Err("view close rejected".into());
    }
    assert_sessions(&mut control, &ids).await?;
    observers.clear();
    for session in &ids {
        let response = exchange(
            &mut control,
            envelope::Payload::SessionCloseRequest(SessionCloseRequest {
                session_id: session.as_uuid().as_bytes().to_vec(),
                apply_view_policy: false,
            }),
        )
        .await?;
        if !matches!(response, envelope::Payload::SessionCloseResponse(response) if response.session_id == session.as_uuid().as_bytes())
        {
            return Err("process close rejected".into());
        }
    }
    assert_sessions(&mut control, &[]).await?;
    drop(control);
    cleanup.shutdown.send(true)?;
    tokio::time::timeout(TIMEOUT, &mut cleanup.server).await???;
    let stats = server.stats();
    if stats.rejected_connections != 0 {
        return Err("authenticated IPC connections were rejected".into());
    }
    for (index, session) in ids.iter().enumerate() {
        let journal = SegmentedJournalWriter::create(
            registry.journal_path(*session),
            Durability::SessionLog,
            DEFAULT_SEGMENT_BYTES,
        )?;
        let index_snapshot = journal.line_index();
        if index_snapshot.integrity_error().is_some() {
            return Err("closed journal failed integrity validation".into());
        }
        let page = index_snapshot.read_page(None, 128, 0)?;
        if !page.rows.iter().any(|row| {
            String::from_utf8_lossy(&row.bytes)
                .contains(&format!("ACK:{index:02}:{}", last_responses[index]))
        }) {
            return Err("closed session journal lost its acknowledged output".into());
        }
    }
    latencies.sort_unstable();
    println!(
        "{}",
        serde_json::json!({
        "gate": "p1-interactive-sessions", "mode": if full { "full" } else { "smoke" },
        "version": 1,
            "os": env::consts::OS, "arch": env::consts::ARCH,
            "sessions": sessions, "rounds": rounds, "acknowledged_samples": latencies.len(),
            "resize_checks": sessions * rounds, "reconnected_sessions": reconnects,
            "journal_sessions": ids.len(), "closed_sessions": ids.len(),
            "ipc_rejected": stats.rejected_connections,
            "input_to_ipc_frame_p95_ms": percentile(&latencies, 95),
            "input_to_ipc_frame_p99_ms": percentile(&latencies, 99),
            "functional_timeout_ms": TIMEOUT.as_millis(), "gpu_measured": false,
        })
    );
    Ok(())
}

async fn connect(endpoint: &str) -> std::io::Result<transport::ClientStream> {
    #[cfg(windows)]
    return transport::connect(endpoint).await;
    #[cfg(unix)]
    transport::connect(std::path::Path::new(endpoint)).await
}
async fn authenticated(endpoint: &str, token: [u8; 32]) -> ProbeResult<transport::ClientStream> {
    tokio::time::timeout(TIMEOUT, async {
        let mut stream = connect(endpoint).await?;
        let mut handshake = Handshake::new(vec![7; 16], token.to_vec());
        handshake.feature_bits = FEATURES;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|error| format!("interactive handshake failed: {error}"))?;
        if negotiated.feature_bits & FEATURES != FEATURES {
            return Err("interactive features not negotiated".into());
        }
        Ok(stream)
    })
    .await
    .map_err(|_| "interactive IPC handshake timed out")?
}
async fn subscribe(
    endpoint: &str,
    token: [u8; 32],
    index: usize,
    session: SessionId,
) -> ProbeResult<Observer> {
    let mut stream = authenticated(endpoint, token).await?;
    let replica = TerminalSubscriptionReplica::new(session);
    write_envelope(
        &mut stream,
        &Envelope {
            request_id: 2,
            payload: Some(envelope::Payload::SnapshotRequest(
                replica.snapshot_request(),
            )),
            ..Default::default()
        },
    )
    .await?;
    Ok(Observer {
        index,
        stream,
        replica,
    })
}
struct Control {
    stream: transport::ClientStream,
    next_request: u64,
}

async fn exchange(
    control: &mut Control,
    payload: envelope::Payload,
) -> ProbeResult<envelope::Payload> {
    let operation = match &payload {
        envelope::Payload::TerminalResizeRequest(_) => "resize",
        envelope::Payload::TerminalInputRequest(_) => "input",
        envelope::Payload::SessionListRequest(_) => "list",
        envelope::Payload::SessionCloseRequest(_) => "close",
        _ => "unknown",
    };
    let request_id = control.next_request;
    control.next_request = request_id
        .checked_add(1)
        .ok_or("control request ID exhausted")?;
    let stream = &mut control.stream;
    tokio::time::timeout(TIMEOUT, async {
        write_envelope(
            stream,
            &Envelope {
                request_id,
                payload: Some(payload),
                ..Default::default()
            },
        )
        .await?;
        let response = read_envelope(stream)
            .await
            .map_err(|error| format!("{operation} response failed: {error}"))?;
        if response.request_id != request_id {
            return Err("control response correlation failed".into());
        }
        response
            .payload
            .ok_or_else(|| "missing interactive control response".into())
    })
    .await
    .map_err(|_| "interactive control request timed out")?
}
async fn accepted(control: &mut Control, payload: envelope::Payload) -> ProbeResult<()> {
    let expected_session = match &payload {
        envelope::Payload::TerminalResizeRequest(request) => request.session_id.clone(),
        envelope::Payload::TerminalInputRequest(request) => request.session_id.clone(),
        _ => return Err("invalid interactive control payload".into()),
    };
    let envelope::Payload::TerminalControlResponse(response) = exchange(control, payload).await?
    else {
        return Err("unexpected terminal control response".into());
    };
    if response.session_id != expected_session
        || response.decoded_status()? != TerminalControlStatus::Accepted
    {
        return Err("interactive control rejected".into());
    }
    Ok(())
}
async fn assert_sessions(control: &mut Control, ids: &[SessionId]) -> ProbeResult<()> {
    let envelope::Payload::SessionListResponse(response) = exchange(
        control,
        envelope::Payload::SessionListRequest(SessionListRequest {}),
    )
    .await?
    else {
        return Err("missing interactive session list".into());
    };
    if response.sessions.len() != ids.len()
        || response
            .sessions
            .iter()
            .map(|session| &session.session_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != ids.len()
        || response.sessions.iter().any(|session| {
            !session.running
                || !ids
                    .iter()
                    .any(|id| id.as_uuid().as_bytes().as_slice() == session.session_id)
        })
    {
        return Err("session liveness or identity set changed".into());
    }
    Ok(())
}
fn percentile(values: &[Duration], percentile: usize) -> f64 {
    let index = values
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1);
    values[index].as_secs_f64() * 1000.0
}
