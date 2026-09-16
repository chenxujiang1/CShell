//! Renderer-side viewport planning. Network, storage and parsing are intentionally absent.

mod log_surface;
mod terminal_interaction;
mod window;

use cshell_terminal::FrameSnapshot;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

pub use log_surface::{
    LogDecorations, LogPage, LogPageRequest, LogReflowLayout, LogReflowRequest, LogRow,
    LogScrollbarState, LogSearchMatch, LogSourceId, LogStyleSpan, LogSurfaceError, LogSurfaceFrame,
    LogSurfaceModel, LogVisualRow, MAX_LOG_PAGE_ROWS, MAX_LOG_PAGE_STYLE_SPANS,
    MAX_LOG_PAGE_TEXT_BYTES, MAX_LOG_REFLOW_ROWS,
};
pub use terminal_interaction::{
    MAX_TERMINAL_SEARCH_MATCHES, MAX_TERMINAL_SEARCH_QUERY_BYTES, MAX_TERMINAL_SELECTION_BYTES,
    TerminalCellPoint, TerminalDecorations, TerminalInteractionError, TerminalSearchMatch,
    TerminalSearchOptions, TerminalSearchResult, TerminalSelection, TerminalSelectionMode,
    search_terminal_snapshot, validate_terminal_search_query,
};
pub use window::{
    AtlasPressureBenchmarkReport, EguiFrame, HeadlessRenderReport, HeadlessTerminalRenderer,
    LogGeometryBenchmarkReport, RenderOutcome, TerminalGeometryBenchmarkReport, TerminalViewport,
    WindowRenderer, WindowRendererError, benchmark_atlas_pressure, benchmark_log_geometry,
    benchmark_terminal_geometry,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollMode {
    FollowTail,
    Anchored { line_id: u64, cell_offset: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderPlan {
    pub generation: u64,
    pub visible_rows: Range<u16>,
    pub glyph_instances_upper_bound: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ViewportPlanner {
    overscan_rows: u16,
}

#[derive(Clone, Debug)]
pub struct TerminalSurfaceFrame {
    pub snapshot: Arc<FrameSnapshot>,
    pub plan: RenderPlan,
    pub generation_changed: bool,
}

/// Renderer-owned latest snapshot input; GPU resources will hang off this boundary.
#[derive(Clone, Debug, Default)]
pub struct TerminalSurfaceModel {
    latest: Option<Arc<FrameSnapshot>>,
    last_prepared_generation: Option<u64>,
    planner: ViewportPlanner,
}

impl TerminalSurfaceModel {
    /// Replaces the renderer input without cloning terminal cells. Stale snapshots
    /// are ignored so a delayed IPC frame cannot move the visible terminal backwards.
    pub fn submit_snapshot(&mut self, snapshot: Arc<FrameSnapshot>) -> bool {
        if self
            .latest
            .as_ref()
            .is_some_and(|current| snapshot.generation <= current.generation)
        {
            return false;
        }
        self.latest = Some(snapshot);
        true
    }

    #[must_use]
    pub fn latest_snapshot(&self) -> Option<&Arc<FrameSnapshot>> {
        self.latest.as_ref()
    }

    #[must_use]
    pub fn prepare_frame(
        &mut self,
        first_visible_row: u16,
        viewport_rows: u16,
    ) -> Option<TerminalSurfaceFrame> {
        let snapshot = self.latest.as_ref()?.clone();
        let generation_changed = self.last_prepared_generation != Some(snapshot.generation);
        self.last_prepared_generation = Some(snapshot.generation);
        let plan = self
            .planner
            .plan(&snapshot, first_visible_row, viewport_rows);
        Some(TerminalSurfaceFrame {
            snapshot,
            plan,
            generation_changed,
        })
    }
}

impl Default for ViewportPlanner {
    fn default() -> Self {
        Self { overscan_rows: 2 }
    }
}

impl ViewportPlanner {
    #[must_use]
    pub const fn new(overscan_rows: u16) -> Self {
        Self { overscan_rows }
    }

    #[must_use]
    pub fn plan(
        self,
        snapshot: &FrameSnapshot,
        first_visible_row: u16,
        viewport_rows: u16,
    ) -> RenderPlan {
        let start = first_visible_row.saturating_sub(self.overscan_rows);
        let end = first_visible_row
            .saturating_add(viewport_rows)
            .saturating_add(self.overscan_rows)
            .min(snapshot.rows);
        let visible_row_count = usize::from(end.saturating_sub(start));
        RenderPlan {
            generation: snapshot.generation,
            visible_rows: start..end,
            glyph_instances_upper_bound: visible_row_count
                .saturating_mul(usize::from(snapshot.cols)),
        }
    }
}

#[must_use]
pub fn create_wgpu_instance() -> wgpu::Instance {
    wgpu::Instance::default()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuProbeReport {
    pub adapter_name: String,
    pub backend: String,
    pub device_type: String,
}

#[derive(Debug, Error)]
pub enum GpuProbeError {
    #[error("no compatible wgpu adapter: {0}")]
    Adapter(#[from] wgpu::RequestAdapterError),
    #[error("cannot create wgpu device: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    #[error("wgpu submission did not complete: {0}")]
    Poll(#[from] wgpu::PollError),
}

/// Requests a headless adapter and submits a real render pass.
///
/// This is the Phase 0 platform probe used before a window surface and glyph
/// atlas are attached. It validates backend/device creation and command
/// submission without coupling the renderer to winit.
pub async fn run_headless_gpu_probe() -> Result<GpuProbeReport, GpuProbeError> {
    let instance = create_wgpu_instance();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await?;
    let info = adapter.get_info();
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("cshell-phase0-probe-device"),
            ..Default::default()
        })
        .await?;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cshell-phase0-probe-target"),
        size: wgpu::Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // Exercise the same compact, non-256-byte-aligned row upload used when a
    // newly rasterized glyph dirties only a small atlas rectangle.
    queue.write_texture(
        texture.as_image_copy(),
        &[u8::MAX; 3 * 2 * 4],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(3 * 4),
            rows_per_image: Some(2),
        },
        wgpu::Extent3d {
            width: 3,
            height: 2,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("cshell-phase0-probe-encoder"),
    });
    {
        let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("cshell-phase0-probe-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.03,
                        g: 0.04,
                        b: 0.06,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    let submission = queue.submit([encoder.finish()]);
    device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: Some(Duration::from_secs(5)),
    })?;

    Ok(GpuProbeReport {
        adapter_name: info.name,
        backend: format!("{:?}", info.backend),
        device_type: format!("{:?}", info.device_type),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        GpuProbeError, HeadlessTerminalRenderer, TerminalSurfaceModel, ViewportPlanner,
        WindowRendererError, run_headless_gpu_probe,
    };
    use cshell_terminal::{Cell, CellWidth, FrameSnapshot, TerminalModes};
    use std::sync::Arc;

    #[test]
    fn only_visible_rows_and_overscan_are_planned() {
        let snapshot = FrameSnapshot {
            generation: 9,
            rows: 100,
            cols: 80,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: Default::default(),
            terminal_modes: TerminalModes::default(),
            cells: vec![Cell::default(); 8_000],
        };
        let plan = ViewportPlanner::new(2).plan(&snapshot, 50, 20);
        assert_eq!(plan.visible_rows, 48..72);
        assert_eq!(plan.glyph_instances_upper_bound, 24 * 80);
    }

    #[test]
    fn terminal_surface_accepts_latest_arc_and_rejects_stale_generations() {
        let snapshot = |generation| {
            Arc::new(FrameSnapshot {
                generation,
                rows: 24,
                cols: 80,
                cursor_row: 0,
                cursor_col: 0,
                cursor_appearance: Default::default(),
                terminal_modes: TerminalModes::default(),
                cells: vec![Cell::default(); 1_920],
            })
        };
        let mut surface = TerminalSurfaceModel::default();
        assert!(surface.submit_snapshot(snapshot(3)));
        assert!(!surface.submit_snapshot(snapshot(2)));
        let first = surface
            .prepare_frame(0, 20)
            .unwrap_or_else(|| panic!("submitted snapshot must prepare a frame"));
        assert_eq!(first.plan.generation, 3);
        assert!(first.generation_changed);
        assert!(
            !surface
                .prepare_frame(0, 20)
                .unwrap_or_else(|| panic!("submitted snapshot must remain available"))
                .generation_changed
        );

        let newest = snapshot(4);
        assert!(surface.submit_snapshot(Arc::clone(&newest)));
        let frame = surface
            .prepare_frame(0, 20)
            .unwrap_or_else(|| panic!("newer snapshot must prepare a frame"));
        assert!(Arc::ptr_eq(&frame.snapshot, &newest));
        assert!(frame.generation_changed);
    }

    #[tokio::test]
    async fn headless_gpu_submits_a_render_pass() {
        let report = match run_headless_gpu_probe().await {
            Ok(report) => report,
            Err(GpuProbeError::Adapter(error))
                if std::env::var_os("CSHELL_REQUIRE_GPU").is_none() =>
            {
                eprintln!("skipping GPU probe because this runner has no adapter: {error}");
                return;
            }
            Err(error) => panic!("{error}"),
        };
        assert!(!report.adapter_name.is_empty());
        eprintln!(
            "wgpu adapter: {} ({}, {})",
            report.adapter_name, report.backend, report.device_type
        );
    }

    #[tokio::test]
    async fn headless_terminal_renderer_draws_real_terminal_geometry() {
        let mut renderer = match HeadlessTerminalRenderer::new(800, 480).await {
            Ok(renderer) => renderer,
            Err(WindowRendererError::Adapter(error))
                if std::env::var_os("CSHELL_REQUIRE_GPU").is_none() =>
            {
                eprintln!(
                    "skipping terminal GPU probe because this runner has no adapter: {error}"
                );
                return;
            }
            Err(error) => panic!("{error}"),
        };
        let mut cells = vec![Cell::default(); 24 * 80];
        for (cell, character) in cells.iter_mut().zip("CShell GPU 回显".chars()) {
            *cell = Cell::new(character, CellWidth::Single, Default::default());
        }
        let snapshot = Arc::new(FrameSnapshot {
            generation: 1,
            rows: 24,
            cols: 80,
            cursor_row: 0,
            cursor_col: 13,
            cursor_appearance: Default::default(),
            terminal_modes: TerminalModes::default(),
            cells,
        });
        let mut surface = TerminalSurfaceModel::default();
        assert!(surface.submit_snapshot(snapshot));
        let frame = surface
            .prepare_frame(0, 24)
            .unwrap_or_else(|| panic!("terminal frame must be prepared"));
        let report = renderer
            .render_frame(&frame)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(report.vertices > 0);
        assert!(!renderer.adapter_info().0.is_empty());
    }
}
