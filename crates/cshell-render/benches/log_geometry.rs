use cshell_render::{
    LogPage, LogRow, LogSourceId, LogStyleSpan, LogSurfaceModel, benchmark_log_geometry,
};
use cshell_terminal::{Color, Style};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

const PAGE_ROWS: usize = 4096;
const VIEWPORT_ROWS: u16 = 40;
const VIEWPORT_COLUMNS: u16 = 120;

fn main() -> ExitCode {
    let rows: Arc<[LogRow]> = (0..PAGE_ROWS)
        .map(|index| LogRow {
            line_id: index as u64 + 1,
            text: Arc::from(format!(
                "ERROR line={index:04} host=prod-{index:04} 中文日志 café 👨‍💻 request completed in 12ms details={}",
                "x".repeat(240)
            )),
            style_spans: Arc::from([LogStyleSpan {
                byte_range: 0..5,
                style: Style {
                    foreground: Color::Rgb(255, 80, 80),
                    ..Style::default()
                },
            }]),
            truncated: false,
        })
        .collect::<Vec<_>>()
        .into();
    let page = Arc::new(LogPage {
        source_id: LogSourceId(1),
        revision: 1,
        anchor_line_id: 1,
        total_line_count: rows.len() as u64,
        rows,
        has_before: false,
        has_after: false,
    });
    let mut surface = LogSurfaceModel::new(2);
    if let Err(error) = surface.submit_page(page) {
        eprintln!("log geometry benchmark page rejected: {error}");
        return ExitCode::FAILURE;
    }
    let Some(reflow) = surface.request_reflow(VIEWPORT_COLUMNS) else {
        eprintln!("log geometry benchmark did not request a reflow layout");
        return ExitCode::FAILURE;
    };
    let reflow_started = Instant::now();
    let layout = match reflow.execute() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("log geometry benchmark reflow failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let reflow_elapsed = reflow_started.elapsed();
    if !surface.submit_reflow(layout) {
        eprintln!("log geometry benchmark rejected its current reflow layout");
        return ExitCode::FAILURE;
    }
    let Some(frame) = surface.prepare_frame(VIEWPORT_ROWS) else {
        eprintln!("log geometry benchmark did not produce a frame");
        return ExitCode::FAILURE;
    };

    match benchmark_log_geometry(&frame, VIEWPORT_COLUMNS, VIEWPORT_ROWS, 500) {
        Ok(report) => {
            println!(
                "log geometry: {} logical rows, {} visual rows in {:.3} ms; {} planned rows, {} vertices, {} dynamic glyphs, {} upload bytes, {} evictions",
                report.page_rows,
                frame.visual_rows.len(),
                reflow_elapsed.as_secs_f64() * 1_000.0,
                report.planned_rows,
                report.vertices,
                report.cached_dynamic_glyphs,
                report.atlas_upload_bytes,
                report.atlas_evictions,
            );
            println!(
                "log geometry: cold {:.3} ms, warm {:.3} ms/frame ({} iterations)",
                report.cold.as_secs_f64() * 1_000.0,
                report.warm_per_iteration.as_secs_f64() * 1_000.0,
                report.iterations,
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("log geometry benchmark failed: {error}");
            ExitCode::FAILURE
        }
    }
}
