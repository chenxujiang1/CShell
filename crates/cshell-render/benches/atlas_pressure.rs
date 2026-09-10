use cshell_render::benchmark_atlas_pressure;
use std::{env, process::ExitCode};

const DEFAULT_CHURN: u64 = 64;
const EXPECTED_ATLAS_BYTES: usize = 2048 * 2048 * 4;
const MIN_FORMAL_CHURN_PER_SECOND: f64 = 1_000.0;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("atlas pressure probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let requested_churn = match env::var_os("CSHELL_ATLAS_CHURN") {
        Some(value) => value.to_string_lossy().parse::<u64>()?,
        None => DEFAULT_CHURN,
    };
    if requested_churn == 0 {
        return Err("atlas churn must be greater than zero".into());
    }
    let report = benchmark_atlas_pressure(requested_churn)?;
    let churn_per_second = report.atlas_evictions as f64 / report.elapsed.as_secs_f64();
    println!(
        "atlas pressure: {} byte fixed atlas, {} glyphs at first pressure, {} cached after churn, {} evictions; pinned={}, rerasterized={}",
        report.atlas_bytes,
        report.dynamic_capacity_glyphs,
        report.cached_dynamic_glyphs,
        report.atlas_evictions,
        report.pinned_glyph_survived,
        report.rerasterized_after_eviction,
    );
    println!(
        "atlas uploads: {} operations, {:.1} MiB total, {} byte maximum, integrity={}; {:.3}s, {:.0} evictions/s",
        report.upload_operations,
        report.upload_bytes as f64 / (1024.0 * 1024.0),
        report.max_upload_bytes,
        report.upload_integrity,
        report.elapsed.as_secs_f64(),
        churn_per_second,
    );

    if report.atlas_bytes != EXPECTED_ATLAS_BYTES
        || report.dynamic_capacity_glyphs == 0
        || report.cached_dynamic_glyphs == 0
        || report.atlas_evictions < requested_churn
        || !report.pinned_glyph_survived
        || !report.rerasterized_after_eviction
        || !report.upload_integrity
        || report.upload_operations == 0
        || report.max_upload_bytes >= report.atlas_bytes
    {
        return Err(format!("atlas invariants failed: {report:?}").into());
    }
    if env::var_os("CSHELL_ENFORCE_PERF").is_some()
        && churn_per_second < MIN_FORMAL_CHURN_PER_SECOND
    {
        return Err(format!(
            "{churn_per_second:.0} evictions/s is below {MIN_FORMAL_CHURN_PER_SECOND:.0}"
        )
        .into());
    }
    Ok(())
}
