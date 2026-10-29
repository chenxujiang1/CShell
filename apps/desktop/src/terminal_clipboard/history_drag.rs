use super::*;
use cshell_render::{LogRow, LogSearchMatch, TerminalInteractionError};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Point {
    line: u64,
    column: u32,
}
pub(super) struct Drag {
    anchor: Point,
    focus: Point,
    block: bool,
    rows: BTreeMap<u64, LogRow>,
    bytes: usize,
}
fn width(text: &str, column: u32) -> u32 {
    if text == "\t" {
        8 - column % 8
    } else {
        UnicodeWidthStr::width(text).clamp(1, 2) as u32
    }
}
impl Drag {
    fn live(
        snapshot: &cshell_terminal::FrameSnapshot,
        selection: TerminalSelection,
    ) -> Option<Self> {
        if snapshot.screen_start_line == 0 {
            return None;
        }
        let mut rows = BTreeMap::new();
        for row in 0..snapshot.rows {
            let cells = snapshot.row(row)?;
            let soft_wrap = cells.iter().any(cshell_terminal::Cell::soft_wrap);
            let mut text = String::new();
            for cell in cells {
                if !matches!(
                    cell.width,
                    cshell_terminal::CellWidth::WideSpacer
                        | cshell_terminal::CellWidth::LeadingWideSpacer
                ) {
                    text.extend(cell.characters());
                }
            }
            if !soft_wrap {
                text.truncate(text.trim_end_matches(' ').len());
            }
            let line_id = snapshot.screen_start_line + u64::from(row);
            rows.insert(
                line_id,
                LogRow {
                    line_id,
                    text: text.into(),
                    soft_wrap,
                    style_spans: Default::default(),
                    truncated: false,
                },
            );
        }
        let bytes = rows.values().map(|r| r.text.len()).sum();
        Some(Self {
            anchor: Point {
                line: snapshot.screen_start_line + u64::from(selection.anchor.row),
                column: u32::from(selection.anchor.column),
            },
            focus: Point {
                line: snapshot.screen_start_line + u64::from(selection.focus.row),
                column: u32::from(selection.focus.column),
            },
            block: selection.mode == TerminalSelectionMode::Block,
            rows,
            bytes,
        })
    }
    fn point(frame: &cshell_render::LogSurfaceFrame, point: TerminalCellPoint) -> Option<Point> {
        let visual = frame
            .visual_rows
            .get(frame.visible_rows.start + frame.first_viewport_row + usize::from(point.row))?;
        let row = frame.page.rows.get(visual.logical_row_index as usize)?;
        let offset = if point.row == 0 {
            visual.start_cell.max(frame.cell_offset)
        } else {
            visual.start_cell
        };
        Some(Point {
            line: row.line_id,
            column: offset + u32::from(point.column),
        })
    }
    fn history(
        frame: &cshell_render::LogSurfaceFrame,
        selection: TerminalSelection,
    ) -> Option<Self> {
        let mut drag = Self {
            anchor: Self::point(frame, selection.anchor)?,
            focus: Self::point(frame, selection.focus)?,
            block: selection.mode == TerminalSelectionMode::Block,
            rows: BTreeMap::new(),
            bytes: 0,
        };
        drag.capture(frame).ok()?;
        Some(drag)
    }
    fn capture(
        &mut self,
        frame: &cshell_render::LogSurfaceFrame,
    ) -> Result<(), TerminalInteractionError> {
        let additional: usize = frame
            .page
            .rows
            .iter()
            .filter(|row| !self.rows.contains_key(&row.line_id))
            .map(|r| r.text.len() + 64)
            .sum();
        if self.bytes.saturating_add(additional) > cshell_render::MAX_TERMINAL_SELECTION_BYTES {
            return Err(TerminalInteractionError::SelectionTooLarge(
                cshell_render::MAX_TERMINAL_SELECTION_BYTES,
            ));
        }
        for row in frame.page.rows.iter() {
            if let std::collections::btree_map::Entry::Vacant(entry) = self.rows.entry(row.line_id)
            {
                self.bytes += row.text.len() + 64;
                entry.insert(row.clone());
            }
        }
        Ok(())
    }
    fn columns(&self, line: u64) -> Option<(u32, u32)> {
        let first = self.anchor.min(self.focus);
        let last = self.anchor.max(self.focus);
        if line < first.line || line > last.line {
            return None;
        }
        if self.block {
            return Some((
                self.anchor.column.min(self.focus.column),
                self.anchor.column.max(self.focus.column),
            ));
        }
        Some((
            if line == first.line { first.column } else { 0 },
            if line == last.line {
                last.column
            } else {
                u32::MAX
            },
        ))
    }
    fn byte_range(&self, row: &LogRow) -> Option<std::ops::Range<u32>> {
        let (left, right) = self.columns(row.line_id)?;
        let mut column = 0;
        let mut first = None;
        let mut end = 0;
        for (byte, g) in row.text.grapheme_indices(true) {
            let next = column + width(g, column);
            if next > left && column <= right {
                first.get_or_insert(byte as u32);
                end = (byte + g.len()) as u32;
            }
            column = next;
        }
        first.map(|start| start..end)
    }
    pub(super) fn text(&self) -> Result<String, TerminalInteractionError> {
        let first = self.anchor.line.min(self.focus.line);
        let last = self.anchor.line.max(self.focus.line);
        if last.saturating_sub(first) > cshell_render::MAX_TERMINAL_SELECTION_BYTES as u64 {
            return Err(TerminalInteractionError::SelectionTooLarge(
                cshell_render::MAX_TERMINAL_SELECTION_BYTES,
            ));
        }
        let mut output = String::new();
        let mut continued = false;
        for line in first..=last {
            let row = self
                .rows
                .get(&line)
                .ok_or(TerminalInteractionError::SelectionUnavailable)?;
            if row.truncated {
                return Err(TerminalInteractionError::SelectionUnavailable);
            }
            if line > first && (self.block || !continued) {
                output.push('\n');
            }
            let (left, right) = self.columns(line)
                .ok_or(TerminalInteractionError::SelectionUnavailable)?;
            let mut column = 0;
            let mut copied = left;
            for g in row.text.graphemes(true) {
                let next = column + width(g, column);
                if next > left && column <= right {
                    if g == "\t" {
                        output.extend(std::iter::repeat_n(
                            ' ',
                            (next.min(right.saturating_add(1)) - column.max(left)) as usize,
                        ));
                    } else {
                        output.push_str(g);
                    }
                    copied = next;
                }
                column = next;
            }
            if self.block {
                output.extend(std::iter::repeat_n(
                    ' ',
                    right.saturating_add(1).saturating_sub(copied) as usize,
                ));
            }
            if output.len() > cshell_render::MAX_TERMINAL_SELECTION_BYTES {
                return Err(TerminalInteractionError::SelectionTooLarge(
                    cshell_render::MAX_TERMINAL_SELECTION_BYTES,
                ));
            }
            continued = row.soft_wrap;
        }
        Ok(output)
    }
}

impl DesktopApp {
    pub(crate) fn cross_selection_text(&self) -> Option<Result<String, TerminalInteractionError>> {
        self.terminal_clipboard.cross.as_ref().map(Drag::text)
    }
    pub(crate) fn start_history_drag(&mut self) -> bool {
        if self.terminal_clipboard.cross.is_some() {
            return true;
        }
        let Some(selection) = self.terminal_decorations.selection else {
            return false;
        };
        let drag = if let Some(frame) = &self.terminal_clipboard.history_frame {
            Drag::history(frame, selection)
        } else {
            self.terminal_clipboard
                .snapshot
                .as_ref()
                .and_then(|s| Drag::live(s, selection))
        };
        let Some(drag) = drag else {
            return false;
        };
        if self.log_surface.is_none() {
            let Some(snapshot) = self.terminal_clipboard.snapshot.as_ref() else {
                return false;
            };
            let start = snapshot.screen_start_line;
            let page = Arc::new(cshell_render::LogPage {
                source_id: LogSourceId(0),
                revision: snapshot.generation,
                first_line_id: 1,
                anchor_line_id: start,
                rows: drag.rows.values().cloned().collect::<Vec<_>>().into(),
                total_line_count: start + u64::from(snapshot.rows) - 1,
                has_before: start > 1,
                has_after: false,
            });
            let mut surface = LogSurfaceModel::default();
            if surface.submit_page(page).is_err() {
                return false;
            }
            if let Some(request) = surface.request_reflow(snapshot.cols) {
                if let Ok(layout) = request.execute() {
                    surface.submit_reflow(layout);
                }
            }
            surface.anchor(start, 0);
            self.log_surface = Some(surface);
            self.terminal_scrollback = true;
            self.submitted_log_page = None;
            if self.log_reflow.is_none() {
                self.log_reflow = LogReflowWorker::start().ok();
            }
            if let Some(daemon) = &self.daemon {
                daemon.cancel_log_page();
            }
        }
        self.terminal_clipboard.cross = Some(drag);
        self.terminal_clipboard.snapshot = None;
        self.terminal_clipboard.history_frame = None;
        self.log_decorations.selection = None;
        self.refresh_history_drag(false);
        true
    }
    pub(crate) fn refresh_history_drag(&mut self, extend: bool) {
        let Some(frame) = self
            .log_surface
            .as_ref()
            .and_then(|s| s.prepare_frame(self.viewport_rows))
        else {
            return;
        };
        let point = self.cursor_position.and_then(|position| {
            self.terminal_point_for_grid(position, self.viewport_rows, frame.layout_columns, true)
        });
        let Some(drag) = &mut self.terminal_clipboard.cross else {
            return;
        };
        if extend {
            if let Err(error) = drag.capture(&frame) {
                self.terminal_selecting = false;
                self.view_model.input_warning = Some(error.to_string());
                return;
            }
        }
        if extend && let Some(point) = point.and_then(|p| Drag::point(&frame, p)) {
            drag.focus = point;
            self.terminal_clipboard.dragged = true;
        }
        let ranges: Arc<[_]> = frame
            .page
            .rows
            .iter()
            .filter_map(|row| {
                drag.byte_range(row).map(|byte_range| LogSearchMatch {
                    line_id: row.line_id,
                    byte_range,
                })
            })
            .collect::<Vec<_>>()
            .into();
        if self.log_decorations.selection_ranges != ranges {
            self.log_decorations.selection_ranges = ranges;
            self.log_decorations.revision = self.log_decorations.revision.wrapping_add(1);
        }
    }
    pub(crate) fn tick_selection_autoscroll(&mut self) -> bool {
        if !self.terminal_selecting
            || !self.terminal_clipboard.dragged
            || self.command_center.blocks_input()
            || !self.window_active
            || self.pending_paste.is_some()
            || self.profile_panel.is_dialog_open()
            || self.zmodem.blocks_input()
            || self.keyboard_auth.blocks_input()
            || self.sync_blocks_input()
            || self.terminal_search_open || self.task_panel.open || self.host_panel.open
        {
            return false;
        }
        let (Some(pos), Some(v)) = (self.cursor_position, self.terminal_viewport) else {
            return false;
        };
        let edge = 24.0 * f64::from(self.egui_context.pixels_per_point());
        let top = f64::from(v.y);
        let bottom = f64::from(v.y + v.height);
        let direction = if pos.y < top + edge {
            -1
        } else if pos.y > bottom - edge {
            1
        } else {
            self.terminal_clipboard.auto_key = None;
            return false;
        };
        if !self.start_history_drag() {
            return false;
        }
        self.refresh_history_drag(true);
        let now = std::time::Instant::now();
        if self
            .terminal_clipboard
            .auto_at
            .is_some_and(|at| now.duration_since(at) < std::time::Duration::from_millis(50))
        {
            return false;
        }
        let key = self
            .log_surface
            .as_ref()
            .and_then(|s| s.prepare_frame(self.viewport_rows))
            .and_then(|f| {
                let visual = f
                    .visual_rows
                    .get(f.visible_rows.start + f.first_viewport_row)?;
                Some((
                    f.page.rows.get(visual.logical_row_index as usize)?.line_id,
                    visual.start_cell,
                    direction,
                ))
            });
        let Some(key) = key else {
            return false;
        };
        if Some(key) == self.terminal_clipboard.auto_key {
            return false;
        }
        self.terminal_clipboard.auto_key = Some(key);
        self.terminal_clipboard.auto_at = Some(now);
        self.scroll_terminal_history_kept(direction * 2, true);
        self.refresh_history_drag(true);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cross_page_selection_uses_stable_ids_and_preserves_wraps_and_unicode() {
        let mut drag = Drag {
            anchor: Point {
                line: 10,
                column: 1,
            },
            focus: Point {
                line: 12,
                column: 2,
            },
            block: false,
            rows: BTreeMap::new(),
            bytes: 0,
        };
        for (id, text, soft_wrap) in [
            (10, "hello", true),
            (11, "中文🙂", false),
            (12, "end", false),
        ] {
            drag.rows.insert(
                id,
                LogRow {
                    line_id: id,
                    text: text.into(),
                    soft_wrap,
                    style_spans: Default::default(),
                    truncated: false,
                },
            );
        }
        assert_eq!(drag.text().unwrap(), "ello中文🙂\nend");
        drag.rows.remove(&11);
        assert_eq!(
            drag.text().unwrap_err(),
            TerminalInteractionError::SelectionUnavailable
        );
    }
    #[test]
    fn live_selection_extends_across_history_pages_without_losing_initial_rows() {
        use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};
        let mut engine = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(2, 8));
        engine.feed(b"one\r\ntwo");
        let mut app = DesktopApp::default();
        app.viewport_rows = 2;
        app.terminal_viewport = Some(TerminalViewport {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        });
        app.cursor_position = Some(winit::dpi::PhysicalPosition::new(25.0, 19.0));
        app.terminal_surface
            .submit_snapshot(Arc::new(engine.snapshot()));
        app.begin_clipboard_selection(TerminalCellPoint { row: 0, column: 0 });
        app.update_clipboard_selection(TerminalCellPoint { row: 1, column: 2 });
        assert!(app.start_history_drag());
        let surface = app.log_surface.as_mut().unwrap();
        surface.anchor(3, 0);
        surface
            .submit_page(Arc::new(cshell_render::LogPage {
                source_id: LogSourceId(0),
                revision: 100,
                first_line_id: 1,
                anchor_line_id: 3,
                rows: [(3, "three"), (4, "four"), (5, "five")]
                    .into_iter()
                    .map(|(line_id, text)| LogRow {
                        line_id,
                        text: text.into(),
                        soft_wrap: false,
                        style_spans: Default::default(),
                        truncated: false,
                    })
                    .collect::<Vec<_>>()
                    .into(),
                total_line_count: 5,
                has_before: true,
                has_after: false,
            }))
            .unwrap();
        let layout = surface.request_reflow(8).unwrap().execute().unwrap();
        surface.submit_reflow(layout);
        app.refresh_history_drag(true);
        assert_eq!(
            app.terminal_selection_text().unwrap().unwrap(),
            "one\ntwo\nthree\nfou"
        );
        assert!(app.log_decorations.selection.is_none());
        assert!(!app.log_decorations.selection_ranges.is_empty());
    }
    #[test]
    fn cache_limit_rejects_new_page_without_advancing_selection() {
        let mut drag = Drag {
            anchor: Point { line: 1, column: 0 },
            focus: Point { line: 1, column: 0 },
            block: false,
            rows: BTreeMap::new(),
            bytes: cshell_render::MAX_TERMINAL_SELECTION_BYTES,
        };
        let mut surface = LogSurfaceModel::default();
        surface
            .submit_page(Arc::new(cshell_render::LogPage {
                source_id: LogSourceId(0),
                revision: 1,
                first_line_id: 1,
                anchor_line_id: 1,
                rows: vec![LogRow {
                    line_id: 1,
                    text: "a".into(),
                    soft_wrap: false,
                    style_spans: Default::default(),
                    truncated: false,
                }]
                .into(),
                total_line_count: 1,
                has_before: false,
                has_after: false,
            }))
            .unwrap();
        let layout = surface.request_reflow(8).unwrap().execute().unwrap();
        surface.submit_reflow(layout);
        assert!(matches!(
            drag.capture(&surface.prepare_frame(2).unwrap()),
            Err(TerminalInteractionError::SelectionTooLarge(_))
        ));
        assert!(drag.rows.is_empty());
        assert_eq!(drag.focus.line, 1);
    }
    #[test]
    fn stalled_page_loading_does_not_accumulate_autoscroll_requests() {
        use cshell_terminal::{AlacrittyTerminalEngine,TerminalEngine};
        let mut engine=AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(2,8));engine.feed(b"one\r\ntwo");
        let mut snapshot=engine.snapshot();snapshot.screen_start_line=100;
        let mut app=DesktopApp::default();app.window_active=true;app.viewport_rows=2;
        app.terminal_viewport=Some(TerminalViewport{x:0,y:0,width:80,height:20});
        app.cursor_position=Some(winit::dpi::PhysicalPosition::new(20.0,-10.0));
        app.terminal_surface.submit_snapshot(Arc::new(snapshot));app.begin_clipboard_selection(TerminalCellPoint{row:0,column:1});
        app.cursor_position=Some(winit::dpi::PhysicalPosition::new(35.0,-10.0));
        app.update_clipboard_selection(TerminalCellPoint{row:1,column:2});
        let mut requests=0;
        for _ in 0..32 {app.terminal_clipboard.auto_at=Some(std::time::Instant::now()-std::time::Duration::from_secs(1));requests+=usize::from(app.tick_selection_autoscroll());}
        assert_eq!(requests,1);
        assert!(app.terminal_clipboard.cross.is_some());
    }
}
