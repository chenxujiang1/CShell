use cshell_render::{LogPage, LogPageRequest, LogRow, LogSourceId, LogSurfaceModel};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use winit::{dpi::PhysicalSize, window::Window};

pub const ARGUMENT: &str = "--log-window-e2e";
const SOURCE: LogSourceId = LogSourceId(0x4353_4845_4c4c);
const TOTAL: u64 = 20_000;
const PAGE_ROWS: u64 = 2_048;

#[derive(Debug)]
pub struct WindowE2e {
    started: Instant,
    revision: u64,
    presents: u64,
    pages: u64,
    reflows: u64,
    requested_resizes: u64,
    observed_resizes: u64,
    device_loss_injected: bool,
    renderer_recoveries: u64,
    software_recoveries: u64,
    failure: Option<String>,
}

impl WindowE2e {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            revision: 1,
            presents: 0,
            pages: 1,
            reflows: 0,
            requested_resizes: 0,
            observed_resizes: 0,
            device_loss_injected: false,
            renderer_recoveries: 0,
            software_recoveries: 0,
            failure: None,
        }
    }

    pub fn initial_page(&self) -> Arc<LogPage> {
        page_around(TOTAL, self.revision)
    }

    pub fn page_for(&mut self, request: LogPageRequest) -> Arc<LogPage> {
        self.revision += 1;
        self.pages += 1;
        page_around(request.anchor_line_id, self.revision)
    }

    pub fn note_reflow(&mut self, promoted: bool) {
        self.reflows += u64::from(promoted);
    }

    pub fn note_resize(&mut self, size: PhysicalSize<u32>) {
        if size.width > 0 && size.height > 0 && self.requested_resizes > 0 {
            self.observed_resizes += 1;
        }
    }

    pub fn should_inject_device_loss(&mut self) -> bool {
        if self.presents >= 24 && !self.device_loss_injected {
            self.device_loss_injected = true;
            true
        } else {
            false
        }
    }

    pub fn note_renderer_recovered(&mut self, software_adapter: bool) {
        self.renderer_recoveries += 1;
        self.software_recoveries += u64::from(software_adapter);
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(message.into());
        }
    }

    pub fn on_present(&mut self, surface: &mut LogSurfaceModel, window: &Window, rows: u16) {
        self.presents += 1;
        let delta = if self.presents < 58 { -73 } else { 89 };
        surface.scroll_visual_rows(delta, rows.max(1));
        match self.presents {
            18 => {
                surface.jump_to_line(127, 0);
            }
            46 => {
                surface.jump_to_line(15_777, 0);
            }
            74 => {
                surface.jump_to_line(4_096, 12);
            }
            102 => surface.follow_tail(),
            12 | 36 | 60 | 84 | 108 => {
                let size = if self.requested_resizes.is_multiple_of(2) {
                    PhysicalSize::new(1_320, 820)
                } else {
                    PhysicalSize::new(940, 610)
                };
                self.requested_resizes += 1;
                let _previous = window.request_inner_size(size);
            }
            _ => {}
        }
    }

    pub fn should_exit(&mut self) -> bool {
        if self.failure.is_some() {
            return true;
        }
        let done = self.presents >= 120
            && self.pages >= 4
            && self.reflows >= 4
            && self.requested_resizes >= 5
            && self.observed_resizes >= 4
            && self.renderer_recoveries >= 1;
        if self.started.elapsed() >= Duration::from_secs(30) && !done {
            self.fail(format!(
                "window E2E timeout: presents={}, pages={}, reflows={}, resize={}/{}, device recoveries={}",
                self.presents,
                self.pages,
                self.reflows,
                self.observed_resizes,
                self.requested_resizes,
                self.renderer_recoveries
            ));
            return true;
        }
        done
    }

    pub fn finish(&self) -> Result<(), String> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        println!(
            "log window E2E passed: {} presents, {} pages, {} reflows, {}/{} resizes, {} device recoveries ({} software)",
            self.presents,
            self.pages,
            self.reflows,
            self.observed_resizes,
            self.requested_resizes,
            self.renderer_recoveries,
            self.software_recoveries
        );
        Ok(())
    }
}

fn page_around(anchor: u64, revision: u64) -> Arc<LogPage> {
    let anchor = anchor.clamp(1, TOTAL);
    let mut first = anchor.saturating_sub(PAGE_ROWS / 2).max(1);
    let last = first.saturating_add(PAGE_ROWS - 1).min(TOTAL);
    first = last.saturating_sub(PAGE_ROWS - 1).max(1);
    let rows = (first..=last)
        .map(|line_id| LogRow {
            line_id,
            text: Arc::from(format!(
                "{line_id:05} │ present/reflow stress │ 中文日志 😀 │ {}",
                "0123456789abcdef".repeat((line_id as usize % 7) + 1)
            )),
            style_spans: Arc::from([]),
            truncated: false,
        })
        .collect::<Vec<_>>()
        .into();
    Arc::new(LogPage {
        source_id: SOURCE,
        revision,
        anchor_line_id: anchor,
        rows,
        total_line_count: TOTAL,
        has_before: first > 1,
        has_after: last < TOTAL,
    })
}

#[cfg(test)]
mod tests {
    use super::{PAGE_ROWS, TOTAL, page_around};

    #[test]
    fn pages_are_bounded_and_contain_edge_anchors() {
        for anchor in [1, 127, TOTAL / 2, TOTAL] {
            let page = page_around(anchor, anchor);
            assert_eq!(page.rows.len(), PAGE_ROWS as usize);
            assert!(
                page.rows
                    .binary_search_by_key(&anchor, |row| row.line_id)
                    .is_ok()
            );
        }
    }
}
