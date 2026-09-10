use cshell_domain::{InputAction, SessionId, TerminalSize};
use cshell_ipc::{
    Envelope, Handshake, HandshakePolicy, SnapshotRequest, TerminalControlStatus,
    TerminalInputRequest, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
use cshelld::{LocalSessionRegistry, SessionIpcServer, SessionIpcService};
use std::{
    collections::BTreeMap,
    env,
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

const DEFAULT_CLIENTS: usize = 10;
const DEFAULT_SECONDS: u64 = 2;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RSS_GROWTH: u64 = 512 * 1024 * 1024;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("IPC fanout probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let clients = env_value("CSHELL_IPC_FANOUT_CLIENTS", DEFAULT_CLIENTS)?;
    let seconds = env_value("CSHELL_IPC_FANOUT_SECONDS", DEFAULT_SECONDS)?;
    if clients == 0 || seconds == 0 {
        return Err("client count and duration must be nonzero".into());
    }
    let directory = tempfile::tempdir()?;
    let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 256)?);
    let attachment = registry.spawn_local(&flood_profile(), TerminalSize::cells(24, 80))?;
    let session_id = attachment.session_id();
    #[cfg(windows)]
    let endpoint = format!(
        r"\\.\pipe\cshell-ipc-fanout-{}-{}",
        std::process::id(),
        session_id
    );
    #[cfg(windows)]
    let listener = transport::LocalListener::bind(&endpoint)?;
    #[cfg(unix)]
    let endpoint_path = directory.path().join("ipc-fanout.sock");
    #[cfg(unix)]
    let endpoint = endpoint_path.to_string_lossy().into_owned();
    #[cfg(unix)]
    let listener = transport::LocalListener::bind(&endpoint_path)?;
    let token = [0x46; 32];
    let server = Arc::new(SessionIpcServer::new(
        listener,
        HandshakePolicy::new(
            token,
            features::FULL_FRAME_RECOVERY | features::TERMINAL_CONTROL,
        ),
        SessionIpcService::new(Arc::clone(&registry)),
    ));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let server_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run_until(shutdown_rx).await })
    };

    let baseline = resources();
    let mut held = Vec::with_capacity(clients);
    let mut initial_generation = 0;
    for index in 0..clients {
        let (client, generation) = subscribe(
            &endpoint,
            token,
            session_id,
            10 + u64::try_from(index)?.saturating_mul(2),
        )
        .await?;
        initial_generation = initial_generation.max(generation);
        held.push(client);
    }
    attachment.send_input(&flood_command(seconds))?;
    let started = Instant::now();
    let mut latencies = Vec::with_capacity(usize::try_from(seconds)?);
    let mut peak = baseline;
    println!("second,generation,accepted,rss_mib,handles,control_ms");
    for second in 1..=seconds {
        tokio::time::sleep_until((started + Duration::from_secs(second)).into()).await;
        let control_started = Instant::now();
        tokio::time::timeout(
            CONTROL_TIMEOUT,
            control(
                &endpoint,
                token,
                session_id,
                10_000 + second.saturating_mul(3),
            ),
        )
        .await
        .map_err(|_| "control connection timed out behind stalled subscribers")??;
        let latency = control_started.elapsed();
        latencies.push(latency);
        let sample = resources();
        peak.rss_bytes = max_option(peak.rss_bytes, sample.rss_bytes);
        peak.handles = max_option(peak.handles, sample.handles);
        println!(
            "{second},{},{},{},{},{:.3}",
            attachment.full_frame()?.generation,
            server.stats().accepted_connections,
            mib(sample.rss_bytes),
            optional(sample.handles),
            latency.as_secs_f64() * 1000.0
        );
    }
    let final_generation = attachment.full_frame()?.generation;
    drop(held);
    shutdown_tx.send(true)?;
    let stats = server_task.await??;
    registry.close(session_id)?;
    latencies.sort_unstable();
    let p95 = percentile_ms(&latencies, 95);
    let rss_growth = peak
        .rss_bytes
        .zip(baseline.rss_bytes)
        .map(|(peak, base)| peak.saturating_sub(base));
    let handle_growth = peak
        .handles
        .zip(baseline.handles)
        .map(|(peak, base)| peak.saturating_sub(base));
    println!(
        "IPC fanout: {clients} stalled subscribers for {seconds}s; generations {initial_generation} -> {final_generation}; control p95 {p95:.3} ms; peak RSS {} MiB (growth {} MiB); peak handles {} (growth {})",
        mib(peak.rss_bytes),
        mib(rss_growth),
        optional(peak.handles),
        optional(handle_growth)
    );
    if env::var_os("CSHELL_ENFORCE_PERF").is_some() {
        if final_generation <= initial_generation {
            return Err("PTY flood produced no terminal generations".into());
        }
        if stats.rejected_connections != 0 {
            return Err(format!("connections were rejected: {stats:?}").into());
        }
        if p95 > CONTROL_TIMEOUT.as_secs_f64() * 1000.0 {
            return Err(format!("control p95 {p95:.3} ms exceeded timeout").into());
        }
        if rss_growth.is_some_and(|bytes| bytes > MAX_RSS_GROWTH) {
            return Err(format!("RSS growth {} MiB exceeded 512 MiB", mib(rss_growth)).into());
        }
        let max_handle_growth = u64::try_from(clients)?
            .saturating_mul(4)
            .saturating_add(128);
        if handle_growth.is_some_and(|count| count > max_handle_growth) {
            return Err(format!(
                "handle/file-descriptor growth {} exceeded {max_handle_growth}",
                optional(handle_growth)
            )
            .into());
        }
        let expected = u64::try_from(clients)?.saturating_add(seconds);
        if stats.accepted_connections != expected {
            return Err(format!(
                "accepted {} connections, expected {expected}",
                stats.accepted_connections
            )
            .into());
        }
    }
    Ok(())
}

fn flood_profile() -> LocalProfile {
    #[cfg(windows)]
    return LocalProfile {
        name: "IPC fanout resource probe".into(),
        program: PathBuf::from("powershell.exe"),
        args: vec!["-NoLogo".into(), "-NoProfile".into()],
        cwd_policy: WorkingDirectoryPolicy::Inherit,
        env_overrides: BTreeMap::new(),
    };
    #[cfg(not(windows))]
    LocalProfile {
        name: "IPC fanout resource probe".into(),
        program: PathBuf::from("/bin/sh"),
        args: Vec::new(),
        cwd_policy: WorkingDirectoryPolicy::Inherit,
        env_overrides: BTreeMap::new(),
    }
}

fn flood_command(seconds: u64) -> InputAction {
    let lines = seconds.saturating_add(1).saturating_mul(100);
    #[cfg(windows)]
    return InputAction::Text(format!(
        "1..{lines} | % {{ [Console]::WriteLine(('CSHELL_FANOUT_{{0:D8}}' -f $_)); Start-Sleep -Milliseconds 10 }}\r"
    ));
    #[cfg(not(windows))]
    InputAction::Text(format!(
        "i=0; while [ $i -lt {lines} ]; do printf 'CSHELL_FANOUT_%08d\\n' $i; i=$((i+1)); sleep 0.01; done\r"
    ))
}

async fn subscribe(
    endpoint: &str,
    token: [u8; 32],
    session_id: SessionId,
    request_id: u64,
) -> Result<(transport::ClientStream, u64), Box<dyn std::error::Error>> {
    let mut client = connect(endpoint).await?;
    let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
    handshake.feature_bits = features::FULL_FRAME_RECOVERY;
    client_handshake(&mut client, request_id, handshake).await?;
    write_envelope(
        &mut client,
        &Envelope {
            request_id: request_id + 1,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::SnapshotRequest(SnapshotRequest {
                session_id: session_id.as_uuid().as_bytes().to_vec(),
                current_generation: None,
            })),
        },
    )
    .await?;
    let response = read_envelope(&mut client).await?;
    let Some(envelope::Payload::FullFrame(frame)) = response.payload else {
        return Err("unexpected snapshot response".into());
    };
    Ok((client, frame.generation))
}

async fn control(
    endpoint: &str,
    token: [u8; 32],
    session_id: SessionId,
    request_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect(endpoint).await?;
    let mut handshake = Handshake::new(vec![4; 16], token.to_vec());
    handshake.feature_bits = features::TERMINAL_CONTROL;
    client_handshake(&mut client, request_id, handshake).await?;
    let input = TerminalInputRequest::from_action(session_id, &InputAction::Text(String::new()))?;
    write_envelope(
        &mut client,
        &Envelope {
            request_id: request_id + 1,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::TerminalInputRequest(input)),
        },
    )
    .await?;
    let response = read_envelope(&mut client).await?;
    let Some(envelope::Payload::TerminalControlResponse(response)) = response.payload else {
        return Err("unexpected control response".into());
    };
    if response.decoded_status()? != TerminalControlStatus::Accepted {
        return Err("control request was rejected".into());
    }
    Ok(())
}

async fn connect(endpoint: &str) -> std::io::Result<transport::ClientStream> {
    #[cfg(windows)]
    return transport::connect(endpoint).await;
    #[cfg(unix)]
    transport::connect(std::path::Path::new(endpoint)).await
}

fn env_value<T>(name: &str, default: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + 'static,
{
    Ok(match env::var_os(name) {
        Some(value) => value.to_string_lossy().parse()?,
        None => default,
    })
}
fn percentile_ms(values: &[Duration], percentile: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let rank = values
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(values.len() - 1);
    values[rank].as_secs_f64() * 1000.0
}
fn max_option(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(v), None) | (None, Some(v)) => Some(v),
        _ => None,
    }
}
fn mib(value: Option<u64>) -> String {
    value
        .map(|v| format!("{:.1}", v as f64 / 1_048_576.0))
        .unwrap_or_else(|| "unavailable".into())
}
fn optional(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unavailable".into())
}

#[derive(Clone, Copy, Default)]
struct Resources {
    rss_bytes: Option<u64>,
    handles: Option<u64>,
}
#[cfg(windows)]
fn resources() -> Resources {
    windows_resources::snapshot()
}
#[cfg(unix)]
fn resources() -> Resources {
    unix_resources::snapshot()
}

#[cfg(windows)]
mod windows_resources {
    #![allow(unsafe_code)]
    use super::Resources;
    use std::mem::{MaybeUninit, size_of};
    use windows_sys::Win32::System::{
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{GetCurrentProcess, GetProcessHandleCount},
    };
    pub fn snapshot() -> Resources {
        unsafe {
            let process = GetCurrentProcess();
            let mut counters = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
            (*counters.as_mut_ptr()).cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let rss_bytes = (K32GetProcessMemoryInfo(
                process,
                counters.as_mut_ptr(),
                size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ) != 0)
                .then(|| counters.assume_init().WorkingSetSize as u64);
            let mut count = 0;
            let handles =
                (GetProcessHandleCount(process, &mut count) != 0).then_some(u64::from(count));
            Resources { rss_bytes, handles }
        }
    }
}

#[cfg(unix)]
mod unix_resources {
    #![allow(unsafe_code)]
    use super::Resources;
    use std::mem::MaybeUninit;
    pub fn snapshot() -> Resources {
        let mut usage = MaybeUninit::<libc::rusage>::zeroed();
        let rss_bytes = if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
            let value = u64::try_from(unsafe { usage.assume_init() }.ru_maxrss).ok();
            #[cfg(target_os = "macos")]
            let value = value;
            #[cfg(not(target_os = "macos"))]
            let value = value.and_then(|v| v.checked_mul(1024));
            value
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let handles = std::fs::read_dir("/proc/self/fd")
            .ok()
            .and_then(|e| u64::try_from(e.count()).ok());
        #[cfg(not(target_os = "linux"))]
        let handles = None;
        Resources { rss_bytes, handles }
    }
}
