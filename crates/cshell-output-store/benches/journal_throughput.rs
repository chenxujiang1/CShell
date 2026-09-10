use cshell_output_store::{Durability, MAX_LOG_PAGE_BYTES, SegmentedJournalWriter};
use std::env;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const TOTAL_BYTES: usize = 64 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const REQUIRED_MIB_PER_SECOND: f64 = 50.0;
const MAX_SCALE_RANDOM_PAGE_MS: f64 = 10.0;
const MAX_SCALE_REOPEN_MS: f64 = 500.0;
const MAX_SCALE_RESIDENT_INDEX_BYTES: usize = 250 * 1024 * 1024;
const BENCH_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("journal throughput probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("throughput.csjr");
    let mut writer =
        SegmentedJournalWriter::create(&path, Durability::Ephemeral, BENCH_SEGMENT_BYTES)?;
    let line_index = writer.line_index();
    let chunk = vec![b'x'; CHUNK_BYTES];
    let started = Instant::now();
    for _ in 0..(TOTAL_BYTES / CHUNK_BYTES) {
        writer.append(&chunk)?;
    }
    writer.flush()?;
    let elapsed = started.elapsed();
    let mib_per_second = TOTAL_BYTES as f64 / 1_048_576.0 / elapsed.as_secs_f64();
    println!(
        "segmented journal: {} MiB across {} segments in {:.3}s = {:.1} MiB/s",
        TOTAL_BYTES / 1_048_576,
        writer.segment_count(),
        elapsed.as_secs_f64(),
        mib_per_second
    );
    let page_started = Instant::now();
    let page = line_index.read_page(None, 0, 0)?;
    let page_elapsed = page_started.elapsed();
    println!(
        "journal index: {} logical lines, {} tail bytes in {:.3} ms",
        page.total_line_count,
        page.rows.first().map_or(0, |row| row.bytes.len()),
        page_elapsed.as_secs_f64() * 1_000.0,
    );
    let styled_started = Instant::now();
    let styled_page = line_index.read_styled_page(None, 0, 0)?;
    let styled_elapsed = styled_started.elapsed();
    println!(
        "journal ANSI page: {} text bytes, {} spans in {:.3} ms",
        styled_page.rows.first().map_or(0, |row| row.text.len()),
        styled_page
            .rows
            .first()
            .map_or(0, |row| row.style_spans.len()),
        styled_elapsed.as_secs_f64() * 1_000.0,
    );
    if page.rows.first().map_or(0, |row| row.bytes.len()) != MAX_LOG_PAGE_BYTES {
        return Err("bounded journal tail page did not contain the expected byte count".into());
    }
    drop(writer);
    let reopen_started = Instant::now();
    let reopened =
        SegmentedJournalWriter::create(&path, Durability::Ephemeral, BENCH_SEGMENT_BYTES)?;
    let reopen_elapsed = reopen_started.elapsed();
    println!(
        "journal checkpoint reopen: {} segments, {} logical lines in {:.3} ms",
        reopened.segment_count(),
        reopened.line_index().total_line_count(),
        reopen_elapsed.as_secs_f64() * 1_000.0,
    );
    let colored_path = directory.path().join("colored.csjr");
    let mut colored_writer =
        SegmentedJournalWriter::create(&colored_path, Durability::Ephemeral, BENCH_SEGMENT_BYTES)?;
    let mut colored_line = b"\x1b[1;38;2;255;80;80mERROR\x1b[0m ".to_vec();
    colored_line.resize(1023, b'x');
    colored_line.push(b'\n');
    for _ in 0..4096 {
        colored_writer.append(&colored_line)?;
    }
    let colored_index = colored_writer.line_index();
    let colored_started = Instant::now();
    let colored_page = colored_index.read_styled_page(None, 4095, 0)?;
    let colored_elapsed = colored_started.elapsed();
    println!(
        "journal colored ANSI page: {} rows, {} text bytes, {} spans in {:.3} ms; line index {} bytes ({:.2} bytes/line)",
        colored_page.rows.len(),
        colored_page
            .rows
            .iter()
            .map(|row| row.text.len())
            .sum::<usize>(),
        colored_page
            .rows
            .iter()
            .map(|row| row.style_spans.len())
            .sum::<usize>(),
        colored_elapsed.as_secs_f64() * 1_000.0,
        colored_index.resident_line_index_bytes(),
        colored_index.resident_line_index_bytes() as f64 / colored_index.total_line_count() as f64,
    );
    if env::var_os("CSHELL_ENFORCE_PERF").is_some() && mib_per_second < REQUIRED_MIB_PER_SECOND {
        return Err(format!(
            "{mib_per_second:.1} MiB/s is below {REQUIRED_MIB_PER_SECOND:.1} MiB/s"
        )
        .into());
    }
    if let Some(scale_mib) = env::var_os("CSHELL_SCALE_MIB") {
        let scale_mib = scale_mib
            .to_string_lossy()
            .parse::<u64>()
            .map_err(|error| format!("invalid CSHELL_SCALE_MIB: {error}"))?;
        run_scale_probe(directory.path(), scale_mib)?;
    }
    Ok(())
}

fn run_scale_probe(directory: &Path, scale_mib: u64) -> Result<(), Box<dyn std::error::Error>> {
    if scale_mib == 0 {
        return Err("CSHELL_SCALE_MIB must be greater than zero".into());
    }
    let total_bytes = scale_mib
        .checked_mul(1024 * 1024)
        .ok_or("scale byte count overflow")?;
    let path = directory.join("scale.csjr");
    let mut writer = SegmentedJournalWriter::create(
        &path,
        Durability::Ephemeral,
        cshell_output_store::DEFAULT_SEGMENT_BYTES,
    )?;
    let mut chunk = vec![b'x'; CHUNK_BYTES];
    for line in chunk.chunks_exact_mut(128) {
        line[127] = b'\n';
    }
    let write_started = Instant::now();
    let mut remaining = total_bytes;
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(CHUNK_BYTES as u64))?;
        writer.append(&chunk[..wanted])?;
        remaining -= wanted as u64;
    }
    writer.flush()?;
    let write_elapsed = write_started.elapsed();
    let index = writer.line_index();
    let line_count = index.total_line_count();
    let expected_lines = total_bytes / 128;
    let expected_records = total_bytes.div_ceil(CHUNK_BYTES as u64);
    if index.logical_bytes() != total_bytes
        || index.record_count() != expected_records
        || line_count != expected_lines
        || index.integrity_error().is_some()
    {
        return Err(format!(
            "live scale index mismatch: bytes={}, records={}, lines={}, integrity={:?}",
            index.logical_bytes(),
            index.record_count(),
            line_count,
            index.integrity_error(),
        )
        .into());
    }
    let checkpoint_bytes = std::fs::metadata(index_cache_path(&path))?.len();
    let cold_index_bytes = std::fs::metadata(companion_path(&path, ".csli"))?.len();
    let resident_bytes = index.resident_line_index_bytes();

    let random_started = Instant::now();
    let mut random = 0x9e37_79b9_7f4a_7c15_u64;
    let random_reads = 1_000_u64.min(line_count);
    for _ in 0..random_reads {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let line_id = random % line_count + 1;
        let page = index.read_page(Some(line_id), 0, 0)?;
        if page.rows.len() != 1 || page.rows[0].line_id != line_id {
            return Err(format!("random page did not preserve LineId {line_id}").into());
        }
    }
    let random_elapsed = random_started.elapsed();
    drop(writer);

    let rebuild_cold_index = env::var_os("CSHELL_SCALE_REBUILD").is_some();
    if rebuild_cold_index {
        OpenOptions::new()
            .write(true)
            .open(companion_path(&path, ".csli"))?
            .set_len(cold_index_bytes.saturating_sub(1))?;
    }
    let reopen_started = Instant::now();
    let reopened = SegmentedJournalWriter::create(
        &path,
        Durability::Ephemeral,
        cshell_output_store::DEFAULT_SEGMENT_BYTES,
    )?;
    let reopen_elapsed = reopen_started.elapsed();
    let reopened_index = reopened.line_index();
    if reopened_index.logical_bytes() != total_bytes
        || reopened_index.record_count() != expected_records
        || reopened_index.total_line_count() != line_count
        || reopened_index.integrity_error().is_some()
    {
        return Err("scale checkpoint reopen changed index identity or integrity".into());
    }
    for line_id in [1, line_count / 2, line_count] {
        let page = reopened.line_index().read_page(Some(line_id), 0, 0)?;
        if page.rows.len() != 1
            || page.rows[0].line_id != line_id
            || page.rows[0].bytes.len() != 127
            || page.rows[0].bytes.iter().any(|byte| *byte != b'x')
        {
            return Err(format!("reopened index returned invalid anchor line {line_id}").into());
        }
    }
    if rebuild_cold_index {
        let rebuilt_cold_index_bytes = std::fs::metadata(companion_path(&path, ".csli"))?.len();
        if rebuilt_cold_index_bytes != cold_index_bytes {
            return Err(format!(
                "cold index rebuild changed its size from {cold_index_bytes} to {rebuilt_cold_index_bytes}"
            )
            .into());
        }
    }
    let write_mib_per_second = scale_mib as f64 / write_elapsed.as_secs_f64();
    let random_ms_per_page = random_elapsed.as_secs_f64() * 1_000.0 / random_reads as f64;
    let reopen_ms = reopen_elapsed.as_secs_f64() * 1_000.0;
    if env::var_os("CSHELL_ENFORCE_PERF").is_some() {
        if write_mib_per_second < REQUIRED_MIB_PER_SECOND {
            return Err(format!(
                "scale throughput {write_mib_per_second:.1} MiB/s is below {REQUIRED_MIB_PER_SECOND:.1} MiB/s"
            )
            .into());
        }
        if random_ms_per_page > MAX_SCALE_RANDOM_PAGE_MS {
            return Err(format!(
                "random page latency {random_ms_per_page:.3} ms exceeded {MAX_SCALE_RANDOM_PAGE_MS:.1} ms"
            )
            .into());
        }
        if !rebuild_cold_index && reopen_ms > MAX_SCALE_REOPEN_MS {
            return Err(format!(
                "checkpoint reopen {reopen_ms:.3} ms exceeded {MAX_SCALE_REOPEN_MS:.1} ms"
            )
            .into());
        }
        if resident_bytes > MAX_SCALE_RESIDENT_INDEX_BYTES {
            return Err(format!(
                "resident line index {resident_bytes} bytes exceeded {MAX_SCALE_RESIDENT_INDEX_BYTES} bytes"
            )
            .into());
        }
    }
    println!(
        "journal scale: {scale_mib} MiB, {line_count} lines, {} segments, {:.1} MiB/s; checkpoint {} bytes, cold index {} bytes, resident index {} bytes; {random_reads} random pages in {:.3} ms ({:.3} ms/page); {} {:.3} ms",
        reopened.segment_count(),
        write_mib_per_second,
        checkpoint_bytes,
        cold_index_bytes,
        resident_bytes,
        random_elapsed.as_secs_f64() * 1_000.0,
        random_ms_per_page,
        if rebuild_cold_index {
            "cold-index rebuild"
        } else {
            "reopen"
        },
        reopen_ms,
    );
    Ok(())
}

fn index_cache_path(base_path: &Path) -> PathBuf {
    companion_path(base_path, ".csix")
}

fn companion_path(base_path: &Path, suffix: &str) -> PathBuf {
    let mut name = base_path
        .file_name()
        .map_or_else(|| OsString::from("journal"), OsString::from);
    name.push(suffix);
    base_path.with_file_name(name)
}
