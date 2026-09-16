use cshell_domain::{InputAction, TerminalSize};
use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
use cshell_render::{HeadlessTerminalRenderer, TerminalSurfaceModel};
use cshell_ssh::{PinnedHostKey, RusshClient, TerminalDataStream, TerminalEvent};
use cshell_terminal::FrameSnapshot;
use cshelld::{LocalSessionRegistry, TerminalPipeline};
use rand::rng;
use russh::keys::ssh_key::{Algorithm, HashAlg, PrivateKey};
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const CHILD_MODE: &str = "--pty-echo-child";
const DEFAULT_SAMPLES: usize = 80;
const DEFAULT_WARMUP: usize = 10;
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);
const P95_BUDGET_MS: f64 = 16.7;
const P99_BUDGET_MS: f64 = 33.0;

fn main() -> ExitCode {
    if env::args().nth(1).as_deref() == Some(CHILD_MODE) {
        return echo_child();
    }
    if env::var_os("CSHELL_REQUIRE_GPU").is_none() {
        println!(
            "echo latency gate skipped; run `cargo xtask echo-latency-bench` to require a real GPU adapter"
        );
        return ExitCode::SUCCESS;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("echo latency probe failed to create runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("echo latency probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn echo_child() -> ExitCode {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if writeln!(output, "CSHELL_ECHO_READY").is_err() || output.flush().is_err() {
        return ExitCode::FAILURE;
    }
    let mut line = String::new();
    loop {
        line.clear();
        let read = match input.read_line(&mut line) {
            Ok(read) => read,
            Err(_) => return ExitCode::FAILURE,
        };
        if read == 0 {
            return ExitCode::SUCCESS;
        }
        if write!(output, "ACK:{line}").is_err() || output.flush().is_err() {
            return ExitCode::FAILURE;
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let samples = env_usize("CSHELL_ECHO_SAMPLES", DEFAULT_SAMPLES)?;
    let warmup = env_usize("CSHELL_ECHO_WARMUP", DEFAULT_WARMUP)?;
    if samples < 20 {
        return Err("CSHELL_ECHO_SAMPLES must be at least 20".into());
    }
    let mut renderer = HeadlessTerminalRenderer::new(800, 480).await?;
    let adapter = renderer.adapter_info();
    println!(
        "terminal GPU adapter: {} ({}, {})",
        adapter.0, adapter.1, adapter.2
    );
    let local = benchmark_local_pty(&mut renderer, samples, warmup)?;
    let ssh = benchmark_ssh(&mut renderer, samples, warmup).await?;
    print_report(
        "local PTY input -> journal/snapshot -> GPU completion",
        &local,
    );
    print_report(
        "loopback SSH input -> journal/snapshot -> GPU completion",
        &ssh,
    );
    if env::var_os("CSHELL_ENFORCE_PERF").is_some() {
        enforce("local PTY", &local)?;
        enforce("loopback SSH", &ssh)?;
    }
    Ok(())
}

fn benchmark_local_pty(
    renderer: &mut HeadlessTerminalRenderer,
    samples: usize,
    warmup: usize,
) -> Result<LatencyReport, Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let registry = LocalSessionRegistry::new(directory.path(), 8)?;
    let profile = LocalProfile {
        name: "CShell PTY echo latency child".to_owned(),
        program: env::current_exe()?,
        args: vec![CHILD_MODE.to_owned()],
        cwd_policy: WorkingDirectoryPolicy::Inherit,
        env_overrides: BTreeMap::new(),
    };
    let attachment = registry.spawn_local(&profile, TerminalSize::cells(24, 80))?;
    let session_id = attachment.session_id();
    let snapshots = attachment.snapshots();
    let mut latest = snapshots
        .wait_for_generation(1, SAMPLE_TIMEOUT)
        .ok_or("local PTY child did not publish its ready frame")?;
    let mut surface = TerminalSurfaceModel::default();
    let mut values = Vec::with_capacity(samples);
    let mut gpu_values = Vec::with_capacity(samples);
    for index in 0..samples.saturating_add(warmup) {
        let token = format!("LOCAL-{index:05}");
        let marker = format!("ACK:{token}");
        let started = Instant::now();
        attachment.send_input(&InputAction::Text(format!("{token}\r")))?;
        latest = wait_for_marker(&snapshots, latest.generation, &marker)?;
        let gpu = render_snapshot(renderer, &mut surface, latest.clone())?;
        let latency = started.elapsed();
        if index >= warmup {
            values.push(latency);
            gpu_values.push(gpu);
        }
    }
    registry.close(session_id)?;
    Ok(LatencyReport::new(values, gpu_values))
}

async fn benchmark_ssh(
    renderer: &mut HeadlessTerminalRenderer,
    samples: usize,
    warmup: usize,
) -> Result<LatencyReport, Box<dyn std::error::Error>> {
    let (address, fingerprint, server) = spawn_ssh_echo_server().await?;
    let client = RusshClient::connect_password(
        address,
        "cshell",
        "phase0",
        PinnedHostKey::sha256(fingerprint),
    )
    .await?;
    let terminal = client.open_terminal(24, 80).await?;
    let (mut reader, writer) = terminal.split();
    let directory = tempfile::tempdir()?;
    let journal = directory.path().join("ssh-echo.csjr");
    let pipeline = TerminalPipeline::spawn(&journal, TerminalSize::cells(24, 80), 8)?;
    let ingress = pipeline
        .ingress()
        .ok_or("SSH terminal pipeline ingress was unavailable")?;
    let snapshots = pipeline.snapshots();
    ingress
        .submit_blocking(b"CSHELL_SSH_READY\r\n".to_vec())
        .map_err(|_| "SSH terminal pipeline rejected its ready frame")?;
    let mut latest = snapshots
        .wait_for_generation(1, SAMPLE_TIMEOUT)
        .ok_or("SSH terminal pipeline did not publish its ready frame")?;
    let mut surface = TerminalSurfaceModel::default();
    let mut values = Vec::with_capacity(samples);
    let mut gpu_values = Vec::with_capacity(samples);
    for index in 0..samples.saturating_add(warmup) {
        let token = format!("SSH-{index:05}");
        let marker = format!("ACK:{token}");
        let started = Instant::now();
        writer
            .send_input(format!("{token}\r\n").into_bytes())
            .await?;
        loop {
            let event = tokio::time::timeout(SAMPLE_TIMEOUT, reader.next_event())
                .await
                .map_err(|_| {
                    format!(
                        "timed out waiting for SSH terminal output at sample {index}; server task finished={}",
                        server.is_finished()
                    )
                })?
                .ok_or_else(|| {
                    format!("SSH terminal channel closed before echo at sample {index}")
                })?;
            match event {
                TerminalEvent::Data {
                    stream: TerminalDataStream::Stdout,
                    data,
                }
                | TerminalEvent::Data {
                    stream: TerminalDataStream::Extended(_),
                    data,
                } => {
                    let minimum_generation = latest.generation.saturating_add(1);
                    ingress
                        .submit_blocking(data)
                        .map_err(|_| "SSH output pipeline closed during echo")?;
                    latest = snapshots
                        .wait_for_generation(minimum_generation, SAMPLE_TIMEOUT)
                        .ok_or("SSH output did not produce a terminal frame")?;
                    if snapshot_contains(&latest, &marker) {
                        break;
                    }
                }
                TerminalEvent::Eof | TerminalEvent::Closed => {
                    return Err("SSH terminal closed before all latency samples".into());
                }
                _ => {}
            }
        }
        let gpu = render_snapshot(renderer, &mut surface, latest.clone())?;
        let latency = started.elapsed();
        if index >= warmup {
            values.push(latency);
            gpu_values.push(gpu);
        }
    }
    writer.close().await?;
    client.disconnect().await?;
    pipeline.shutdown()?;
    tokio::time::timeout(SAMPLE_TIMEOUT, server)
        .await
        .map_err(|_| "SSH echo server did not stop")?
        .map_err(|error| format!("SSH echo server task failed: {error}"))?
        .map_err(|error| format!("SSH echo server failed: {error}"))?;
    Ok(LatencyReport::new(values, gpu_values))
}

fn wait_for_marker(
    snapshots: &cshelld::LatestSnapshot,
    mut generation: u64,
    marker: &str,
) -> Result<FrameSnapshot, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + SAMPLE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for terminal marker {marker}").into());
        }
        let snapshot = snapshots
            .wait_for_generation(generation.saturating_add(1), remaining)
            .ok_or_else(|| format!("timed out waiting for terminal marker {marker}"))?;
        generation = snapshot.generation;
        if snapshot_contains(&snapshot, marker) {
            return Ok(snapshot);
        }
    }
}

fn snapshot_contains(snapshot: &FrameSnapshot, marker: &str) -> bool {
    let marker: Vec<_> = marker.chars().collect();
    snapshot
        .cells
        .chunks(usize::from(snapshot.cols))
        .any(|row| {
            row.windows(marker.len()).any(|window| {
                window
                    .iter()
                    .zip(&marker)
                    .all(|(cell, expected)| cell.character == *expected)
            })
        })
}

fn render_snapshot(
    renderer: &mut HeadlessTerminalRenderer,
    surface: &mut TerminalSurfaceModel,
    snapshot: FrameSnapshot,
) -> Result<Duration, Box<dyn std::error::Error>> {
    if !surface.submit_snapshot(Arc::new(snapshot)) {
        return Err("renderer rejected a fresh terminal snapshot".into());
    }
    let frame = surface
        .prepare_frame(0, 24)
        .ok_or("renderer did not prepare the submitted terminal frame")?;
    let report = renderer.render_frame(&frame)?;
    if report.vertices == 0 {
        return Err("terminal GPU submission contained no geometry".into());
    }
    Ok(report.gpu_completion)
}

#[derive(Clone, Copy, Debug)]
struct LatencyReport {
    samples: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    maximum_ms: f64,
    gpu_p95_ms: f64,
    gpu_p99_ms: f64,
}

impl LatencyReport {
    fn new(mut values: Vec<Duration>, mut gpu_values: Vec<Duration>) -> Self {
        values.sort_unstable();
        gpu_values.sort_unstable();
        Self {
            samples: values.len(),
            p50_ms: percentile_ms(&values, 50),
            p95_ms: percentile_ms(&values, 95),
            p99_ms: percentile_ms(&values, 99),
            maximum_ms: values
                .last()
                .map_or(0.0, |value| value.as_secs_f64() * 1_000.0),
            gpu_p95_ms: percentile_ms(&gpu_values, 95),
            gpu_p99_ms: percentile_ms(&gpu_values, 99),
        }
    }
}

fn percentile_ms(values: &[Duration], percentile: usize) -> f64 {
    let rank = values
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(values.len().saturating_sub(1));
    values
        .get(rank)
        .map_or(0.0, |value| value.as_secs_f64() * 1_000.0)
}

fn print_report(label: &str, report: &LatencyReport) {
    println!(
        "{label}: {} samples, p50/p95/p99/max {:.3}/{:.3}/{:.3}/{:.3} ms; GPU completion p95/p99 {:.3}/{:.3} ms",
        report.samples,
        report.p50_ms,
        report.p95_ms,
        report.p99_ms,
        report.maximum_ms,
        report.gpu_p95_ms,
        report.gpu_p99_ms
    );
}

fn enforce(label: &str, report: &LatencyReport) -> Result<(), Box<dyn std::error::Error>> {
    if report.p95_ms > P95_BUDGET_MS || report.p99_ms > P99_BUDGET_MS {
        return Err(format!(
            "{label} latency p95/p99 {:.3}/{:.3} ms exceeded {:.1}/{:.1} ms",
            report.p95_ms, report.p99_ms, P95_BUDGET_MS, P99_BUDGET_MS
        )
        .into());
    }
    Ok(())
}

fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    match env::var_os(name) {
        Some(value) => Ok(value.to_string_lossy().parse()?),
        None => Ok(default),
    }
}

#[derive(Debug, Default)]
struct EchoSshServer {
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Handler for EchoSshServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "cshell" && password == "phase0" {
            Auth::Accept
        } else {
            Auth::reject()
        })
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

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if term == "xterm-256color" && col_width == 80 && row_height == 24 {
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(channel_stream) = self.channels.remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(channel_stream.into_stream());
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            loop {
                line.clear();
                let Ok(read) = reader.read_until(b'\n', &mut line).await else {
                    break;
                };
                if read == 0 {
                    break;
                }
                if writer.write_all(b"ACK:").await.is_err()
                    || writer.write_all(&line).await.is_err()
                    || writer.flush().await.is_err()
                {
                    break;
                }
            }
        });
        Ok(())
    }
}

async fn spawn_ssh_echo_server() -> Result<
    (
        std::net::SocketAddr,
        String,
        tokio::task::JoinHandle<Result<(), String>>,
    ),
    Box<dyn std::error::Error>,
> {
    let host_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519)?;
    let fingerprint = host_key.fingerprint(HashAlg::Sha256).to_string();
    let mut config = russh::server::Config::default();
    config.keys.push(host_key);
    config.auth_rejection_time = Duration::from_millis(1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        stream
            .set_nodelay(true)
            .map_err(|error| error.to_string())?;
        let running = russh::server::run_stream(Arc::new(config), stream, EchoSshServer::default())
            .await
            .map_err(|error| error.to_string())?;
        match running.await {
            Ok(()) => Ok(()),
            Err(russh::Error::IO(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        }
    });
    Ok((address, fingerprint, server))
}
