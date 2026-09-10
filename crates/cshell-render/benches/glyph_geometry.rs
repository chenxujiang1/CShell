use cshell_domain::TerminalSize;
use cshell_render::benchmark_terminal_geometry;
use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};
use std::process::ExitCode;

const SOURCE: &str = include_str!("../../../tests/visual/terminal-unicode-v1.ansi.txt");

fn main() -> ExitCode {
    let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(30, 100));
    terminal.feed(&decoded_bytes());
    match benchmark_terminal_geometry(&terminal.snapshot(), 200) {
        Ok(report) => {
            println!(
                "glyph geometry: {} cells, {} vertices, {} dynamic glyphs, {} upload bytes, {} evictions",
                report.cells,
                report.vertices,
                report.cached_dynamic_glyphs,
                report.atlas_upload_bytes,
                report.atlas_evictions,
            );
            println!(
                "glyph geometry: cold {:.3} ms, warm {:.3} ms/frame ({} iterations)",
                report.cold.as_secs_f64() * 1_000.0,
                report.warm_per_iteration.as_secs_f64() * 1_000.0,
                report.iterations,
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("glyph geometry benchmark failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn decoded_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SOURCE.len());
    for line in SOURCE.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut remaining = line.as_bytes();
        while let Some(position) = remaining.windows(4).position(|window| window == b"\\x1b") {
            bytes.extend_from_slice(&remaining[..position]);
            bytes.push(0x1b);
            remaining = &remaining[position + 4..];
        }
        bytes.extend_from_slice(remaining);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes
}
