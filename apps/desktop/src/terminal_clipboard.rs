//! Local selection and clipboard entry points; never send selected text remotely.
use super::*;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
mod context;
mod history_drag;
mod smart;

pub(super) enum Action {
    Copy,
    Paste,
    Edit,
    Find(String),
    OpenLink(String),
    RemotePath(String, bool),
    CopyLink(String),
}

#[derive(Default)]
pub(super) struct State {
    pub snapshot: Option<Arc<cshell_terminal::FrameSnapshot>>,
    pub history_frame: Option<cshell_render::LogSurfaceFrame>,
    owner: Option<cshell_domain::TabId>,
    dragged: bool,
    clicks: u8,
    word_anchor: Option<TerminalCellPoint>,
    line_anchor: Option<TerminalCellPoint>,
    press_position: Option<winit::dpi::PhysicalPosition<f64>>,
    pub menu: Option<egui::Pos2>,
    menu_session: Option<SessionId>,
    menu_focus_requested: bool,
    toast: Option<(String, std::time::Instant)>,
    pub review: sync_input::compose::PasteReview,
    cross: Option<history_drag::Drag>,
    auto_at: Option<std::time::Instant>,
    auto_key: Option<(u64, u32, i32)>,
    context: context::Context,
    pub pending_link: Option<String>,
}

/// Match the log renderer's grapheme widths and tab stops. Preserve logical
/// continuation metadata so wrapped history copies without added newlines.
pub(super) fn history_snapshot(
    frame: &cshell_render::LogSurfaceFrame,
    rows: u16,
) -> cshell_terminal::FrameSnapshot {
    use cshell_terminal::{Cell, CellWidth, FrameSnapshot};
    let cols = frame.layout_columns.max(1);
    let mut cells = vec![Cell::default(); usize::from(rows) * usize::from(cols)];
    let first = frame.visible_rows.start + frame.first_viewport_row;
    for screen_row in 0..rows {
        let Some(visual) = frame.visual_rows.get(first + usize::from(screen_row)) else {
            break;
        };
        let Some(row) = frame.page.rows.get(visual.logical_row_index as usize) else {
            continue;
        };
        let first_cell = if screen_row == 0 {
            visual.start_cell.max(frame.cell_offset)
        } else {
            visual.start_cell
        };
        let text = row
            .text
            .get(visual.start_byte as usize..visual.end_byte as usize)
            .unwrap_or_default();
        let mut column = visual.start_cell;
        for grapheme in text.graphemes(true) {
            let width = if grapheme == "\t" {
                8 - column % 8
            } else {
                UnicodeWidthStr::width(grapheme).clamp(1, 2) as u32
            };
            let next = column + width;
            if column >= first_cell && column - first_cell < u32::from(cols) {
                let index =
                    usize::from(screen_row) * usize::from(cols) + (column - first_cell) as usize;
                let mut chars = grapheme.chars();
                let c = chars.next().unwrap_or(' ');
                cells[index] = Cell::with_zerowidth(
                    if c == '\t' { ' ' } else { c },
                    chars,
                    if width == 2 && c != '\t' {
                        CellWidth::Wide
                    } else {
                        CellWidth::Single
                    },
                    Default::default(),
                );
                if width == 2 && c != '\t' && column - first_cell + 1 < u32::from(cols) {
                    cells[index + 1] = Cell::new(' ', CellWidth::WideSpacer, Default::default());
                }
            }
            column = next;
        }
        let continues = row.soft_wrap
            || frame
                .visual_rows
                .get(first + usize::from(screen_row) + 1)
                .is_some_and(|next| next.logical_row_index == visual.logical_row_index);
        let index = (usize::from(screen_row) + 1) * usize::from(cols) - 1;
        cells[index] = cells[index].clone().with_soft_wrap(continues);
    }
    FrameSnapshot {
        screen_start_line: 0,
        generation: frame.page.revision,
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        cursor_appearance: Default::default(),
        terminal_modes: Default::default(),
        cells,
    }
}

/// Copy original history graphemes, including emoji sequences with multiple base
/// scalars. The cell snapshot is only a hit-test/highlight map, not the text source.
pub(super) fn history_text(
    frame: &cshell_render::LogSurfaceFrame,
    snapshot: &cshell_terminal::FrameSnapshot,
    selection: TerminalSelection,
) -> Result<String, cshell_render::TerminalInteractionError> {
    let mut output = String::new();
    let mut previous = None;
    let mut previous_soft_wrap = false;
    let first = frame.visible_rows.start + frame.first_viewport_row;
    for screen_row in 0..snapshot.rows {
        let mut selected =
            (0..snapshot.cols).filter(|col| selection.contains(snapshot, screen_row, *col));
        let Some(left) = selected.next() else {
            continue;
        };
        let right = selected.last().unwrap_or(left);
        let Some(visual) = frame.visual_rows.get(first + usize::from(screen_row)) else {
            break;
        };
        let Some(row) = frame.page.rows.get(visual.logical_row_index as usize) else {
            continue;
        };
        if previous.is_some()
            && ((!previous_soft_wrap && previous != Some(visual.logical_row_index))
                || selection.mode == TerminalSelectionMode::Block)
        {
            output.push('\n');
        }
        previous = Some(visual.logical_row_index);
        previous_soft_wrap = row.soft_wrap;
        let offset = if screen_row == 0 {
            visual.start_cell.max(frame.cell_offset)
        } else {
            visual.start_cell
        };
        let mut column = visual.start_cell;
        let mut copied_until = u32::from(left);
        for grapheme in row
            .text
            .get(visual.start_byte as usize..visual.end_byte as usize)
            .unwrap_or_default()
            .graphemes(true)
        {
            let width = if grapheme == "\t" {
                8 - column % 8
            } else {
                UnicodeWidthStr::width(grapheme).clamp(1, 2) as u32
            };
            let next = column + width;
            let start = column.saturating_sub(offset);
            let end = next.saturating_sub(offset);
            if next > offset && start <= u32::from(right) && end > u32::from(left) {
                if grapheme == "\t" {
                    output.extend(std::iter::repeat_n(
                        ' ',
                        (end.min(u32::from(right) + 1) - start.max(u32::from(left))) as usize,
                    ));
                } else {
                    output.push_str(grapheme);
                }
                copied_until = end;
            }
            column = next;
        }
        if selection.mode == TerminalSelectionMode::Block {
            output.extend(std::iter::repeat_n(
                ' ',
                (u32::from(right) + 1).saturating_sub(copied_until) as usize,
            ));
        }
        if output.len() > cshell_render::MAX_TERMINAL_SELECTION_BYTES {
            return Err(cshell_render::TerminalInteractionError::SelectionTooLarge(
                cshell_render::MAX_TERMINAL_SELECTION_BYTES,
            ));
        }
    }
    Ok(output)
}

impl DesktopApp {
    pub(super) fn clipboard_pointer_available(&self, scale: f32) -> bool {
        let over_control = self.cursor_position.is_some_and(|pos| {
            let point = egui::pos2(pos.x as f32 / scale, pos.y as f32 / scale);
            self.terminal_ui_regions.iter().any(|rect| rect.contains(point))
        });
        !over_control && self.terminal_wheel_available(scale)
            && (self.log_surface.is_none() || self.terminal_scrollback)
            && !self.task_panel.open
            && !self.host_panel.open
            && !self.tunnel_panel.open
            && !self.host_ca_panel.open
            && !self.ssh_provider_panel.open
            && !self.terminal_search_open
            && !self.sync_blocks_input()
    }
    pub(super) fn clear_clipboard_selection(&mut self) {
        self.cancel_clipboard_gesture();
        self.terminal_clipboard.snapshot = None;
        self.terminal_clipboard.history_frame = None;
        self.terminal_clipboard.cross = None;
        self.terminal_clipboard.auto_key = None;
        self.terminal_decorations.selection = None;
        self.log_decorations.selection = None;
        self.log_decorations.selection_ranges = Arc::from([]);
        self.log_decorations.revision = self.log_decorations.revision.wrapping_add(1);
        self.bump_terminal_decorations();
    }

    // End pointer ownership independently of retaining the selected text.
    pub(super) fn cancel_clipboard_gesture(&mut self) {
        self.terminal_selecting = false;
        self.terminal_clipboard.press_position = None;
        self.terminal_clipboard.word_anchor = None;
        self.terminal_clipboard.line_anchor = None;
        self.terminal_clipboard.auto_at = None;
        self.terminal_clipboard.auto_key = None;
    }

    fn selection_source(&self) -> Option<Arc<cshell_terminal::FrameSnapshot>> {
        if let Some(frame) = self.log_surface.as_ref()?.prepare_frame(self.viewport_rows) {
            return Some(Arc::new(history_snapshot(&frame, self.viewport_rows)));
        }
        None
    }

    pub(super) fn begin_clipboard_selection(&mut self, point: TerminalCellPoint) {
        self.pause_terminal_reading();
        self.cancel_history_search_jump();
        let extending = self.modifiers.shift_key()
            && self.terminal_clipboard.owner == self.current_workspace_tab
            && self.terminal_decorations.selection.is_some();
        if !extending {
            // Wheel/edge scrolling may have promoted the previous selection to
            // stable history IDs. A fresh press must discard that anchor too.
            self.clear_clipboard_selection();
            self.terminal_clipboard.history_frame = self
                .log_surface
                .as_ref()
                .and_then(|s| s.prepare_frame(self.viewport_rows));
            self.terminal_clipboard.snapshot = if self.log_surface.is_some() {
                self.selection_source()
            } else {
                self.terminal_surface.latest_snapshot().cloned()
            };
        }
        self.terminal_clipboard.owner = self.current_workspace_tab;
        self.terminal_clipboard.press_position = self.cursor_position;
        self.terminal_clipboard.menu = None;
        self.terminal_clipboard.clicks =
            register_terminal_click(&mut self.terminal_click, point, std::time::Instant::now());
        self.terminal_clipboard.dragged = extending;
        self.terminal_clipboard.word_anchor =
            (!extending && !self.modifiers.alt_key() && self.terminal_clipboard.clicks == 2)
                .then_some(point);
        self.terminal_clipboard.line_anchor =
            (!extending && !self.modifiers.alt_key() && self.terminal_clipboard.clicks == 3)
                .then_some(point);
        let previous = self.terminal_decorations.selection;
        self.terminal_decorations.selection = Some(if let Some(previous) = previous.filter(|_| extending) {
            TerminalSelection {
                focus: point,
                ..previous
            }
        } else {
            TerminalSelection {
                anchor: point,
                focus: point,
                mode: if self.modifiers.alt_key() {
                    TerminalSelectionMode::Block
                } else if self.terminal_clipboard.clicks == 3 {
                    TerminalSelectionMode::Line
                } else if self.terminal_clipboard.clicks == 2 {
                    TerminalSelectionMode::Word
                } else {
                    TerminalSelectionMode::Character
                },
            }
        });
        self.terminal_selecting = true;
        if extending && self.terminal_clipboard.cross.is_some() {
            self.refresh_history_drag(true);
        } else {
            self.update_clipboard_highlight();
        }
    }

    pub(super) fn update_clipboard_selection(&mut self, point: TerminalCellPoint) {
        if !self.terminal_selecting {
            return;
        }
        if let (Some(start), Some(now)) =
            (self.terminal_clipboard.press_position, self.cursor_position)
        {
            let threshold = 3.0 * f64::from(self.egui_context.pixels_per_point());
            if !self.terminal_clipboard.dragged && (now.x - start.x).hypot(now.y - start.y) <= threshold {
                return;
            }
        }
        if self.terminal_clipboard.cross.is_some() {
            self.refresh_history_drag(true);
            return;
        }
        if let (Some(start), Some(now)) =
            (self.terminal_clipboard.press_position, self.cursor_position)
        {
            let threshold = 3.0 * f64::from(self.egui_context.pixels_per_point());
            if (now.x - start.x).hypot(now.y - start.y) > threshold {
                self.terminal_clipboard.dragged = true;
            }
        }
        if let Some(selection) = &mut self.terminal_decorations.selection {
            if selection.focus != point {
                *selection = if let Some(anchor) = self.terminal_clipboard.line_anchor {
                    TerminalSelection {
                        anchor,
                        focus: point,
                        mode: TerminalSelectionMode::Line,
                    }
                } else if let Some(anchor) = self.terminal_clipboard.word_anchor {
                    TerminalSelection {
                        anchor,
                        focus: point,
                        mode: TerminalSelectionMode::Word,
                    }
                } else {
                    TerminalSelection {
                        focus: point,
                        ..*selection
                    }
                };
                self.terminal_clipboard.dragged = true;
                self.update_clipboard_highlight();
            }
        }
    }

    fn update_clipboard_highlight(&mut self) {
        if let (Some(selection), Some(snapshot)) = (
            &mut self.terminal_decorations.selection,
            &self.terminal_clipboard.snapshot,
        ) {
            *selection = if selection.mode == TerminalSelectionMode::Line {
                selection.expanded_lines(snapshot)
            } else {
                smart::expand(*selection, snapshot, &terminal_clipboard_settings::get())
            };
        }
        self.bump_terminal_decorations();
        self.log_decorations.selection = if self.log_surface.is_some() {
            self.terminal_clipboard
                .snapshot
                .clone()
                .zip(self.terminal_decorations.selection)
        } else {
            None
        };
        self.log_decorations.revision = self.log_decorations.revision.wrapping_add(1);
    }

    pub(super) fn finish_clipboard_selection(&mut self) {
        if !self.terminal_selecting {
            return;
        }
        self.cancel_clipboard_gesture();
        if !self.terminal_clipboard.dragged && self.terminal_clipboard.clicks == 1 {
            self.clear_clipboard_selection();
        } else if terminal_clipboard_settings::get().copy_on_select {
            self.copy_terminal_selection();
        }
    }

    pub(super) fn copy_terminal_selection(&mut self) {
        if let Some(result) = self.terminal_selection_text() {
            match result {
                Ok(text) if !text.is_empty() => {
                    let count = text.chars().count();
                    if let Some(state) = &mut self.egui_state {
                        state.set_clipboard_text(text);
                        self.terminal_clipboard.toast =
                            Some((format!("已复制 {count} 个字符"), std::time::Instant::now()));
                        self.view_model.input_warning = None;
                    }
                }
                Err(error) => self.view_model.input_warning = Some(format!("复制失败：{error}")),
                _ => {}
            }
        }
    }
    pub(crate) fn clipboard_notice(&mut self, message: String) {
        self.terminal_clipboard.toast = Some((message, std::time::Instant::now()));
    }

    pub(super) fn paste_from_clipboard(&mut self, window: &Window, edit: bool) {
        let text = self
            .egui_state
            .as_mut()
            .and_then(|state| state.clipboard_text());
        let decision = match decide_terminal_paste(text) {
            Ok(decision) => decision,
            Err(error) => {
                self.view_model.input_warning = Some(error);
                return;
            }
        };
        if edit {
            let text = match decision {
                PasteDecision::Immediate(text) | PasteDecision::Confirm(text) => text,
            };
            if let (Some(sync), Some(tab)) = (&self.sync_input, self.current_workspace_tab) {
                let accepted = sync
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .edit_clipboard(self.workspace.window_id, tab, text);
                if !accepted {
                    self.view_model.input_warning =
                        Some("命令栏已有草稿，请先发送或清空，再载入剪贴板".into());
                }
            }
            return;
        }
        match decision {
            PasteDecision::Immediate(text) => {
                self.clear_clipboard_selection();
                self.submit_paste(text, window);
            }
            PasteDecision::Confirm(text) => {
                if self.sync_input.as_ref().is_some_and(|sync| {
                    sync.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_realtime()
                }) {
                    self.clear_clipboard_selection();
                    self.submit_terminal_action(InputAction::Paste {
                        text,
                        bracketed: true,
                    });
                } else if let Some(session_id) = self.active_terminal_session_id() {
                    let session_title = self
                        .view_model
                        .sessions
                        .iter()
                        .find(|s| s.id == session_id)
                        .map_or_else(|| session_id.to_string(), |s| s.title.clone());
                    self.terminal_clipboard.review.reset();
                    self.pending_paste = Some(PendingPaste {
                        text,
                        session_id,
                        session_title,
                    });
                    self.view_model.input_warning = None;
                } else {
                    self.view_model.input_warning = Some("当前标签没有可用终端，粘贴未发送".into());
                }
            }
        }
        window.request_redraw();
    }

    pub(super) fn draw_clipboard_ui(&mut self, ctx: &egui::Context) -> Option<Action> {
        if let Some(link) = self.terminal_clipboard.pending_link.take() {
            ctx.open_url(egui::OpenUrl::new_tab(link));
        }
        let mut command = None;
        if self.terminal_clipboard.menu.is_none() && self.modifiers.control_key() && self.clipboard_pointer_available(ctx.pixels_per_point())
            && let (Some(link),Some(pos))=(self.terminal_link_hint(),self.cursor_position) {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            egui::Area::new(egui::Id::new("terminal-link-hint")).order(egui::Order::Tooltip).interactable(false)
                .fixed_pos(egui::pos2(pos.x as f32/ctx.pixels_per_point()+16.0,pos.y as f32/ctx.pixels_per_point()+20.0)).constrain(true)
                .show(ctx,|ui|{egui::Frame::popup(ui.style()).show(ui,|ui|{ui.set_max_width(420.0);ui.add(egui::Label::new(format!("Ctrl＋单击打开：{link}")).truncate());});});
        }
        if self.command_center.blocks_input() || self.pending_paste.is_some()
            || self.profile_panel.is_dialog_open()
            || self.zmodem.blocks_input()
            || self.keyboard_auth.blocks_input()
            || self.sync_blocks_input()
        {
            self.terminal_clipboard.menu = None;
        }
        if self.terminal_clipboard.snapshot.is_some()
            && self.terminal_clipboard.history_frame.is_none()
            && let Some(viewport) = self.terminal_viewport
        {
            let scale = ctx.pixels_per_point();
            let position = egui::pos2(
                (viewport.x + viewport.width) as f32 / scale - 12.0,
                (viewport.y + viewport.height) as f32 / scale - 12.0,
            );
            egui::Area::new(egui::Id::new("terminal-selection-status"))
                .order(egui::Order::Tooltip)
                .pivot(egui::Align2::RIGHT_BOTTOM)
                .fixed_pos(position)
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.weak("已固定选区 · 单击 / Esc 恢复实时画面");
                    });
                });
        }
        if let Some(position) = self.terminal_clipboard.menu {
            if self.terminal_clipboard.owner != self.current_workspace_tab
                || self.terminal_clipboard.menu_session != self.active_terminal_session_id()
            {
                self.terminal_clipboard.menu = None;
            } else {
                let selection = self.terminal_decorations.selection.is_some();
                let paste = self.active_terminal_session_id().is_some();
                let remote = self.sftp_profile_for_open().is_some();
                let shown = egui::Area::new(egui::Id::new("terminal-clipboard-menu"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(position)
                    .constrain(true)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(250.0);
                            let copy = cshell_ui::form::menu_item(ui,"复制                 Ctrl+Shift+C",selection);
                            if self.terminal_clipboard.menu_focus_requested && selection {
                                copy.request_focus();
                            }
                            if copy.clicked() {
                                command = Some(Action::Copy);
                            }
                            let paste_button = cshell_ui::form::menu_item(ui,"粘贴                 Ctrl+Shift+V",paste);
                            if self.terminal_clipboard.menu_focus_requested && !selection && paste {
                                paste_button.request_focus();
                            }
                            self.terminal_clipboard.menu_focus_requested = false;
                            if paste_button.clicked() {
                                command = Some(Action::Paste);
                            }
                            if ui
                                .add_enabled(paste && self.sync_input.is_some(),egui::Button::new("粘贴到多行编辑器…").min_size(egui::vec2(ui.available_width(),28.0)).right_text(()))
                                .clicked()
                            {
                                command = Some(Action::Edit);
                            }
                            let context = &self.terminal_clipboard.context;
                            if let Some(text) = &context.selected {
                                ui.separator();
                                if cshell_ui::form::menu_item(ui,"查找选中文本",true).clicked() {
                                    command = Some(Action::Find(text.to_string()));
                                }
                            }
                            if let Some(link) = &context.link {
                                ui.separator();
                                if cshell_ui::form::menu_item(ui,"打开链接",true).on_hover_text(link).clicked() {
                                    command = Some(Action::OpenLink(link.clone()));
                                }
                                if cshell_ui::form::menu_item(ui,"复制链接地址",true).clicked() {
                                    command = Some(Action::CopyLink(link.clone()));
                                }
                            }
                            if let Some(path) = &context.path {
                                ui.separator();
                                if cshell_ui::form::menu_item(ui,"在 SFTP 中定位",remote)
                                    .on_hover_text(path)
                                    .clicked()
                                {
                                    command = Some(Action::RemotePath(path.clone(), false));
                                }
                                if cshell_ui::form::menu_item(ui,"通过 SFTP 下载",remote && path != "/")
                                    .on_hover_text("下载到 SFTP 左侧当前本地目录，进度进入传输队列")
                                    .clicked()
                                {
                                    command = Some(Action::RemotePath(path.clone(), true));
                                }
                            }
                        });
                    });
                let outside = ctx.input(|i| {
                    i.pointer.any_pressed()
                        && i.pointer
                            .interact_pos()
                            .is_some_and(|p| !shown.response.rect.contains(p))
                });
                if command.is_some()
                    || outside
                    || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    self.terminal_clipboard.menu = None;
                }
            }
        }
        if let Some((message, at)) = &self.terminal_clipboard.toast {
            if at.elapsed() < std::time::Duration::from_millis(1400) {
                egui::Area::new(egui::Id::new("terminal-copy-toast"))
                    .order(egui::Order::Tooltip)
                    .anchor(egui::Align2::RIGHT_BOTTOM, [-20.0, -50.0])
                    .interactable(false)
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(message);
                        });
                    });
                ctx.request_repaint_after(
                    std::time::Duration::from_millis(1400).saturating_sub(at.elapsed()),
                );
            }
        }
        command
    }

    pub(super) fn open_clipboard_menu(&mut self, position: egui::Pos2) {
        self.cancel_clipboard_gesture();
        self.terminal_click = TerminalClickState::default();
        self.terminal_clipboard.context = self.capture_terminal_context();
        self.terminal_clipboard.menu = Some(position);
        self.terminal_clipboard.menu_focus_requested = true;
        self.terminal_clipboard.owner = self.current_workspace_tab;
        self.terminal_clipboard.menu_session = self.active_terminal_session_id();
    }

    pub(super) fn dismiss_clipboard_menu(&mut self) {
        self.terminal_clipboard.menu = None;
        self.egui_context.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};

    fn app() -> DesktopApp {
        let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(3, 16));
        terminal.feed("hello /var/log\r\n中文日志".as_bytes());
        let mut app = DesktopApp::default();
        app.terminal_surface
            .submit_snapshot(Arc::new(terminal.snapshot()));
        app.current_workspace_tab = Some(cshell_domain::TabId::new());
        app
    }

    #[test]
    fn double_click_jitter_and_movement_after_release_do_not_extend_selection() {
        let mut app = app();
        let point = TerminalCellPoint { row: 0, column: 2 };
        app.cursor_position = Some(winit::dpi::PhysicalPosition::new(20.0, 10.0));
        app.begin_clipboard_selection(point);
        app.finish_clipboard_selection();
        app.begin_clipboard_selection(point);
        app.cursor_position = Some(winit::dpi::PhysicalPosition::new(21.0, 10.0));
        app.update_clipboard_selection(TerminalCellPoint { row: 0, column: 3 });
        assert_eq!(app.terminal_selection_text().unwrap().unwrap(), "hello");
        app.finish_clipboard_selection();
        app.update_clipboard_selection(TerminalCellPoint { row: 1, column: 8 });
        assert_eq!(app.terminal_selection_text().unwrap().unwrap(), "hello");
        app.begin_clipboard_selection(TerminalCellPoint { row: 1, column: 8 });
        app.finish_clipboard_selection();
        assert!(app.terminal_selection_text().is_none());
    }

    #[test]
    fn opening_menu_stops_drag_without_replacing_existing_selection() {
        let mut app = app();
        app.begin_clipboard_selection(TerminalCellPoint { row: 0, column: 0 });
        app.update_clipboard_selection(TerminalCellPoint { row: 0, column: 4 });
        app.open_clipboard_menu(egui::pos2(10.0, 10.0));
        app.update_clipboard_selection(TerminalCellPoint { row: 1, column: 3 });
        assert!(!app.terminal_selecting);
        assert_eq!(app.terminal_selection_text().unwrap().unwrap(), "hello");
    }
    #[test]
    fn single_click_does_not_copy_and_drag_keeps_exact_original_text() {
        let mut app = app();
        let first = TerminalCellPoint { row: 0, column: 0 };
        app.begin_clipboard_selection(first);
        app.finish_clipboard_selection();
        assert!(app.terminal_decorations.selection.is_none());
        // A new click sequence, then drag across hello.
        app.terminal_click = TerminalClickState::default();
        app.begin_clipboard_selection(first);
        app.update_clipboard_selection(TerminalCellPoint { row: 0, column: 4 });
        app.finish_clipboard_selection();
        let mut changed = (*app.terminal_surface.latest_snapshot().unwrap().as_ref()).clone();
        changed.generation += 1;
        changed.cells[0].character = 'X';
        app.terminal_surface.submit_snapshot(Arc::new(changed));
        assert_eq!(app.terminal_selection_text().unwrap().unwrap(), "hello");
        app.clear_clipboard_selection();
        assert!(app.terminal_selection_text().is_none());
    }
    #[test]
    fn double_click_and_shift_click_select_word_then_extend() {
        let mut app = app();
        let point = TerminalCellPoint { row: 0, column: 8 };
        app.begin_clipboard_selection(point);
        app.finish_clipboard_selection();
        app.begin_clipboard_selection(point);
        app.finish_clipboard_selection();
        assert_eq!(app.terminal_selection_text().unwrap().unwrap(), "/var/log");
        app.modifiers = ModifiersState::SHIFT;
        app.begin_clipboard_selection(TerminalCellPoint { row: 1, column: 2 });
        app.finish_clipboard_selection();
        assert!(
            app.terminal_selection_text()
                .unwrap()
                .unwrap()
                .starts_with("/var/log\n中文")
        );
    }
    #[test]
    fn keyboard_copy_chords_keep_ctrl_c_for_remote_and_ignore_repeats() {
        let c = Key::Character("c".into());
        let insert = Key::Named(NamedKey::Insert);
        assert!(matches_terminal_copy_shortcut(
            ElementState::Pressed,
            false,
            &c,
            ModifiersState::CONTROL | ModifiersState::SHIFT
        ));
        assert!(matches_terminal_copy_shortcut(
            ElementState::Pressed,
            false,
            &insert,
            ModifiersState::CONTROL
        ));
        assert!(!matches_terminal_copy_shortcut(
            ElementState::Pressed,
            false,
            &c,
            ModifiersState::CONTROL
        ));
        assert!(!matches_terminal_copy_shortcut(
            ElementState::Pressed,
            true,
            &insert,
            ModifiersState::CONTROL
        ));
        assert!(!matches_terminal_copy_shortcut(
            ElementState::Released,
            false,
            &insert,
            ModifiersState::CONTROL
        ));
    }
    #[test]
    fn history_selection_rejoins_wrapped_chinese_and_preserves_real_line_breaks() {
        use cshell_render::{LogPage, LogRow, LogSourceId, LogSurfaceModel};
        let mut surface = LogSurfaceModel::default();
        surface
            .submit_page(Arc::new(LogPage {
                first_line_id: 1,
                source_id: LogSourceId(0),
                revision: 1,
                anchor_line_id: 1,
                rows: vec![
                    LogRow {
                        soft_wrap: false,
                        line_id: 1,
                        text: Arc::from("abcd中文ef"),
                        style_spans: Default::default(),
                        truncated: false,
                    },
                    LogRow {
                        soft_wrap: false,
                        line_id: 2,
                        text: Arc::from("next👨‍👩‍👧‍👦"),
                        style_spans: Default::default(),
                        truncated: false,
                    },
                ]
                .into(),
                total_line_count: 2,
                has_before: false,
                has_after: false,
            }))
            .unwrap();
        let layout = surface.request_reflow(4).unwrap().execute().unwrap();
        surface.submit_reflow(layout);
        let frame = surface.prepare_frame(8).unwrap();
        let snapshot = history_snapshot(&frame, 8);
        let selection = TerminalSelection {
            anchor: TerminalCellPoint { row: 0, column: 0 },
            focus: TerminalCellPoint { row: 4, column: 3 },
            mode: TerminalSelectionMode::Character,
        };
        assert_eq!(
            history_text(&frame, &snapshot, selection).unwrap(),
            "abcd中文ef\nnext👨‍👩‍👧‍👦"
        );
    }

    #[test]
    fn context_menu_stays_inside_window_and_escape_closes_it() {
        let mut app = app();
        let ctx = crate::ui_layout_gate::context();
        let size = egui::vec2(720.0, 480.0);
        app.open_clipboard_menu(egui::pos2(715.0, 475.0));
        for _ in 0..4 {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ui| {
                    app.draw_clipboard_ui(ui.ctx());
                },
            );
            out.textures_delta.clear();
        }
        let rect = ctx
            .memory(|m| m.area_rect(egui::Id::new("terminal-clipboard-menu")))
            .unwrap();
        assert!(
            egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                .expand(1.0)
                .contains_rect(rect),
            "{rect:?}"
        );
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                app.draw_clipboard_ui(ui.ctx());
            },
        );
        out.textures_delta.clear();
        assert!(app.terminal_clipboard.menu.is_none());
    }
}
