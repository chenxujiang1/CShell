use cshell_domain::TerminalSize;
use cshell_output_store::{DEFAULT_SEGMENT_BYTES, Durability, SegmentedJournalWriter};
use cshelld::TerminalPipeline;
use std::env;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const LINE_BYTES: usize = 128;
const LINES_PER_CHUNK: u64 = (CHUNK_BYTES / LINE_BYTES) as u64;
const INGRESS_CAPACITY: usize = 8;
// The telemetry starts tracking before a blocking send so that a producer
// waiting for queue capacity cannot disappear from the pressure metric. The
// maximum is therefore: queued messages + one parser-owned message + one
// producer waiting in submit_blocking.
const MAX_IN_FLIGHT_MESSAGES: usize = INGRESS_CAPACITY + 2;
const DEFAULT_SECONDS: u64 = 2;
const DEFAULT_FEED_MIB_PER_SECOND: u64 = 8;
const REQUIRED_MIB_PER_SECOND: f64 = 50.0;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pipeline throughput probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let seconds = env_u64("CSHELL_PIPELINE_SECONDS", DEFAULT_SECONDS)?;
    let feed_mib_per_second = env_u64(
        "CSHELL_PIPELINE_MIB_PER_SECOND",
        DEFAULT_FEED_MIB_PER_SECOND,
    )?;
    if seconds == 0 || feed_mib_per_second == 0 {
        return Err("pipeline duration and feed rate must be greater than zero".into());
    }
    let chunks_per_second = feed_mib_per_second
        .checked_mul(MIB / CHUNK_BYTES as u64)
        .ok_or("pipeline chunks-per-second overflow")?;
    let total_chunks = chunks_per_second
        .checked_mul(seconds)
        .ok_or("pipeline chunk count overflow")?;
    let total_bytes = total_chunks
        .checked_mul(CHUNK_BYTES as u64)
        .ok_or("pipeline byte count overflow")?;
    let expected_lines = total_chunks
        .checked_mul(LINES_PER_CHUNK)
        .ok_or("pipeline line count overflow")?;

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("pipeline-throughput.csjr");
    let pipeline = TerminalPipeline::spawn(&path, TerminalSize::cells(24, 80), INGRESS_CAPACITY)?;
    let ingress = pipeline
        .ingress()
        .ok_or("pipeline ingress was unavailable after startup")?;
    let snapshots = pipeline.snapshots();
    let responses = pipeline.responses();
    let telemetry = pipeline.telemetry();
    let line_index = pipeline.line_index();
    let base_chunk = corpus_chunk();
    let mut probe_chunk = base_chunk.clone();
    probe_chunk[..4].copy_from_slice(b"\x1b[6n");
    let initial_peak_rss = peak_resident_bytes();
    let started = Instant::now();
    let mut expected_generation = 0_u64;
    let mut response_latencies = Vec::new();
    let mut resize_latencies = Vec::new();
    let mut resize_count = 0_u64;

    for chunk_index in 0..total_chunks {
        let is_probe = chunk_index > 0 && chunk_index % chunks_per_second == 0;
        let response_started = is_probe.then(Instant::now);
        ingress
            .submit_blocking(if is_probe {
                probe_chunk.clone()
            } else {
                base_chunk.clone()
            })
            .map_err(|_| "pipeline closed while accepting sustained output")?;
        expected_generation = expected_generation.saturating_add(1);

        if let Some(response_started) = response_started {
            snapshots
                .wait_for_generation(expected_generation, RESPONSE_TIMEOUT)
                .ok_or("terminal snapshot did not reach the DSR probe generation")?;
            let response = responses.wait_and_drain(RESPONSE_TIMEOUT);
            if !response.iter().any(|bytes| {
                bytes.first() == Some(&0x1b)
                    && bytes.get(1) == Some(&b'[')
                    && bytes.last() == Some(&b'R')
            }) {
                return Err("terminal did not emit a valid DSR cursor response".into());
            }
            response_latencies.push(response_started.elapsed());

            let cols = if resize_count.is_multiple_of(2) {
                81
            } else {
                80
            };
            let size = TerminalSize::cells(24, cols);
            let resize_started = Instant::now();
            ingress.resize_blocking(size)?;
            expected_generation = expected_generation.saturating_add(1);
            let resized = snapshots
                .wait_for_generation(expected_generation, RESPONSE_TIMEOUT)
                .ok_or("terminal snapshot did not reach the resize generation")?;
            if (resized.rows, resized.cols) != (size.rows, size.cols) {
                return Err("terminal published an incorrect size after resize".into());
            }
            resize_latencies.push(resize_started.elapsed());
            resize_count = resize_count.saturating_add(1);
        }

        let scheduled = started
            + Duration::from_secs_f64(
                (chunk_index.saturating_add(1)) as f64 / chunks_per_second as f64,
            );
        let delay = scheduled.saturating_duration_since(Instant::now());
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }

    snapshots
        .wait_for_generation(expected_generation, DRAIN_TIMEOUT)
        .ok_or("terminal parser did not drain sustained output before timeout")?;
    let processed_elapsed = started.elapsed();
    let mib_per_second = total_bytes as f64 / MIB as f64 / processed_elapsed.as_secs_f64();
    let stats = telemetry.snapshot();
    validate_stats(
        stats,
        total_bytes,
        total_chunks,
        resize_count,
        INGRESS_CAPACITY,
    )?;
    if line_index.logical_bytes() != total_bytes
        || line_index.record_count() != total_chunks
        || line_index.total_line_count() != expected_lines
        || line_index.integrity_error().is_some()
    {
        return Err("live journal index did not preserve sustained output exactly".into());
    }

    let finalize_started = Instant::now();
    pipeline.shutdown()?;
    let finalize_elapsed = finalize_started.elapsed();
    let final_peak_rss = peak_resident_bytes();

    let reopened =
        SegmentedJournalWriter::create(&path, Durability::SessionLog, DEFAULT_SEGMENT_BYTES)?;
    let reopened_index = reopened.line_index();
    if reopened_index.logical_bytes() != total_bytes
        || reopened_index.record_count() != total_chunks
        || reopened_index.total_line_count() != expected_lines
        || reopened_index.integrity_error().is_some()
    {
        return Err("reopened journal checkpoint did not preserve sustained output exactly".into());
    }

    let response = latency_report(&mut response_latencies);
    let resize = latency_report(&mut resize_latencies);
    println!(
        "pipeline sustained: {:.1} MiB in {:.3}s = {:.2} MiB/s; {} records, {} lines, peak in-flight {}/{}",
        total_bytes as f64 / MIB as f64,
        processed_elapsed.as_secs_f64(),
        mib_per_second,
        total_chunks,
        expected_lines,
        stats.peak_in_flight_messages,
        MAX_IN_FLIGHT_MESSAGES,
    );
    println!(
        "pipeline control: {} DSR probes p50/p95/p99 {:.3}/{:.3}/{:.3} ms; {} resize probes p50/p95/p99 {:.3}/{:.3}/{:.3} ms",
        response.count,
        response.p50_ms,
        response.p95_ms,
        response.p99_ms,
        resize.count,
        resize.p50_ms,
        resize.p95_ms,
        resize.p99_ms,
    );
    println!(
        "pipeline storage: finalize {:.3}s, {} segments, resident line index {} bytes; peak RSS {}",
        finalize_elapsed.as_secs_f64(),
        reopened.segment_count(),
        reopened_index.resident_line_index_bytes(),
        format_peak_rss(initial_peak_rss, final_peak_rss),
    );

    if env::var_os("CSHELL_ENFORCE_PERF").is_some() && mib_per_second < REQUIRED_MIB_PER_SECOND {
        return Err(format!(
            "{mib_per_second:.2} MiB/s is below {REQUIRED_MIB_PER_SECOND:.2} MiB/s"
        )
        .into());
    }
    Ok(())
}

fn corpus_chunk() -> Vec<u8> {
    let mut chunk = vec![b'x'; CHUNK_BYTES];
    let prefix = b"LOAD\x1b[1;38;5;196mERROR\x1b[0m ";
    for line in chunk.chunks_exact_mut(LINE_BYTES) {
        line[..prefix.len()].copy_from_slice(prefix);
        line[LINE_BYTES - 1] = b'\n';
    }
    chunk
}

fn env_u64(name: &str, default: u64) -> Result<u64, Box<dyn std::error::Error>> {
    match env::var_os(name) {
        Some(value) => value
            .to_string_lossy()
            .parse::<u64>()
            .map_err(|error| format!("invalid {name}: {error}").into()),
        None => Ok(default),
    }
}

fn validate_stats(
    stats: cshelld::PipelineStats,
    total_bytes: u64,
    total_chunks: u64,
    resize_count: u64,
    ingress_capacity: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if stats.accepted_output_bytes != total_bytes
        || stats.processed_output_bytes != total_bytes
        || stats.accepted_output_messages != total_chunks
        || stats.processed_output_messages != total_chunks
        || stats.accepted_resizes != resize_count
        || stats.processed_resizes != resize_count
        || stats.in_flight_messages != 0
    {
        return Err(format!("pipeline telemetry did not drain exactly: {stats:?}").into());
    }
    let max_in_flight = ingress_capacity.saturating_add(2);
    if stats.peak_in_flight_messages > max_in_flight {
        return Err(format!(
            "pipeline exceeded its bounded in-flight limit: {} > {}",
            stats.peak_in_flight_messages, max_in_flight
        )
        .into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct LatencyReport {
    count: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

fn latency_report(values: &mut [Duration]) -> LatencyReport {
    values.sort_unstable();
    LatencyReport {
        count: values.len(),
        p50_ms: percentile_ms(values, 50),
        p95_ms: percentile_ms(values, 95),
        p99_ms: percentile_ms(values, 99),
    }
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
    values[rank].as_secs_f64() * 1_000.0
}

fn format_peak_rss(initial: Option<u64>, final_value: Option<u64>) -> String {
    match (initial, final_value) {
        (Some(initial), Some(final_value)) => format!(
            "{:.1} MiB (+{:.1} MiB after startup)",
            final_value as f64 / MIB as f64,
            final_value.saturating_sub(initial) as f64 / MIB as f64,
        ),
        _ => "unavailable".to_owned(),
    }
}

#[cfg(windows)]
fn peak_resident_bytes() -> Option<u64> {
    windows_memory::peak_resident_bytes()
}

#[cfg(windows)]
mod windows_memory {
    #![allow(unsafe_code)]

    use std::mem::{MaybeUninit, size_of};
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn peak_resident_bytes() -> Option<u64> {
        let mut counters = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
        unsafe {
            (*counters.as_mut_ptr()).cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                counters.as_mut_ptr(),
                size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ) == 0
            {
                return None;
            }
            Some(counters.assume_init().PeakWorkingSetSize as u64)
        }
    }
}

#[cfg(unix)]
fn peak_resident_bytes() -> Option<u64> {
    unix_memory::peak_resident_bytes()
}

#[cfg(unix)]
mod unix_memory {
    #![allow(unsafe_code)]

    use std::mem::MaybeUninit;

    pub fn peak_resident_bytes() -> Option<u64> {
        let mut usage = MaybeUninit::<libc::rusage>::zeroed();
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let value = u64::try_from(unsafe { usage.assume_init() }.ru_maxrss).ok()?;
        #[cfg(target_os = "macos")]
        return Some(value);
        #[cfg(not(target_os = "macos"))]
        value.checked_mul(1024)
    }
}
