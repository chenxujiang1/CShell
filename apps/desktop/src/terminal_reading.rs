//! Per-tab local reading state. Pausing the view never pauses the SSH process.
use super::*;

#[derive(Clone, Copy)]
enum Position {
    Live,
    History { line: u64, cell: u32 },
}

#[derive(Clone, Copy)]
struct Origin {
    tab: Option<cshell_domain::TabId>,
    session: Option<SessionId>,
    position: Position,
}

#[derive(Default)]
pub(super) struct State {
    baseline: Option<u64>,
    paused: bool,
    new_output: bool,
    owner: Option<SessionId>,
    origin: Option<Origin>,
    return_requested: bool,
}

impl DesktopApp {
    pub(super) fn open_terminal_search(&mut self) {
        if !self.terminal_search_open {
            self.terminal_reading.origin = None;
            self.remember_search_position();
        }
        self.terminal_search_open = true;
        self.terminal_search_focus_requested = true;
    }
    pub(super) fn pause_terminal_reading(&mut self) {
        if !self.terminal_reading.paused {
            self.terminal_reading.baseline = self
                .terminal_surface
                .latest_snapshot()
                .map(|s| s.generation);
            self.terminal_reading.new_output = false;
            self.terminal_reading.paused = true;
            self.terminal_reading.owner = self.view_model.selected;
        }
    }

    pub(super) fn refresh_terminal_reading(&mut self) {
        if self.terminal_reading.owner != self.view_model.selected {
            self.terminal_reading = State::default();
            self.terminal_reading.owner = self.view_model.selected;
        }
        let paused = self.terminal_scrollback || self.terminal_clipboard.snapshot.is_some();
        if paused {
            self.pause_terminal_reading();
            let latest = self
                .terminal_surface
                .latest_snapshot()
                .map(|s| s.generation);
            if self.terminal_reading.baseline.is_some() && latest != self.terminal_reading.baseline
            {
                self.terminal_reading.new_output = true;
            }
        } else {
            self.terminal_reading.paused = false;
            self.terminal_reading.new_output = false;
            self.terminal_reading.baseline = None;
        }
        self.view_model.terminal_reading_status =
            if self.terminal_surface.latest_snapshot().is_some() {
                Some(
                    if paused {
                        if self.terminal_reading.new_output {
                            "回看中 · 有新输出"
                        } else {
                            "回看中"
                        }
                    } else {
                        "跟随最新"
                    }
                    .into(),
                )
            } else {
                None
            };
    }

    pub(super) fn reading_return_label(&self, compact: bool) -> &'static str {
        match (self.terminal_reading.new_output, compact) {
            (true, _) => "有新输出 · 返回最新",
            (false, true) => "返回最新 [End]",
            (false, false) => "正在回看历史 · 返回最新 [End]",
        }
    }

    pub(super) fn remember_search_position(&mut self) {
        if self.terminal_reading.origin.is_some() {
            return;
        }
        let position = if self.terminal_scrollback {
            let Some(cshell_render::ScrollMode::Anchored {
                line_id,
                cell_offset,
            }) = self.log_surface.as_ref().map(LogSurfaceModel::mode)
            else {
                return;
            };
            Position::History {
                line: line_id,
                cell: cell_offset,
            }
        } else if self.log_surface.is_none() {
            match self
                .terminal_clipboard
                .snapshot
                .as_ref()
                .filter(|s| s.screen_start_line > 0)
            {
                Some(snapshot) => Position::History {
                    line: snapshot.screen_start_line,
                    cell: 0,
                },
                None => Position::Live,
            }
        } else {
            return;
        };
        self.terminal_reading.origin = Some(Origin {
            tab: self.current_workspace_tab,
            session: self.view_model.selected,
            position,
        });
    }

    pub(super) fn cancel_history_search_jump(&mut self) {
        if self.history_search_direction.take().is_some() {
            self.terminal_search_revision = self.terminal_search_revision.wrapping_add(1);
            if let Some(daemon) = &self.daemon {
                daemon.cancel_history_search();
            }
        }
    }

    pub(super) fn draw_search_origin_return(&mut self, ui: &mut egui::Ui, pane: egui::Rect) {
        if self.terminal_reading.origin.is_none() || self.task_panel.open {
            return;
        }
        let rect =
            interaction_regressions::return_button_rect(pane).translate(egui::vec2(0.0, -32.0));
        self.terminal_ui_regions.push(rect);
        if ui
            .put(
                rect,
                egui::Button::new("返回查找前位置")
                    .truncate()
                    .fill(cshell_ui::theme::PANEL),
            )
            .on_hover_text("恢复查找前的阅读位置；不会向服务器发送按键")
            .clicked()
        {
            self.terminal_reading.return_requested = true;
        }
    }

    pub(super) fn apply_search_return_request(&mut self) {
        if !std::mem::take(&mut self.terminal_reading.return_requested) {
            return;
        }
        self.return_to_search_origin();
    }

    fn return_to_search_origin(&mut self) {
        let Some(origin) = self.terminal_reading.origin.take() else {
            return;
        };
        if origin.tab != self.current_workspace_tab || origin.session != self.view_model.selected {
            return;
        }
        self.close_terminal_search();
        self.clear_clipboard_selection();
        match origin.position {
            Position::Live => self.return_to_live_terminal(),
            Position::History { line, cell } => {
                if self.log_surface.is_none() {
                    self.scroll_terminal_history(-1);
                }
                if let Some(surface) = &mut self.log_surface {
                    if surface.prepare_frame(self.viewport_rows).is_some() {
                        surface.jump_to_line(line, cell);
                    } else {
                        // No page exists after leaving history. Preserve the
                        // destination for the first request instead of losing
                        // the jump while waiting for the tail page.
                        surface.anchor(line, cell);
                    }
                }
            }
        }
    }

    pub(super) fn initial_terminal_history_request(&self) -> cshell_render::LogPageRequest {
        let (follow_tail, anchor_line_id, cell_offset) =
            match self.log_surface.as_ref().map(LogSurfaceModel::mode) {
                Some(cshell_render::ScrollMode::Anchored {
                    line_id,
                    cell_offset,
                }) => (false, line_id, cell_offset),
                _ => (true, 0, 0),
            };
        let rows = self.viewport_rows.saturating_add(128).min(2047);
        cshell_render::LogPageRequest {
            source_id: LogSourceId(0),
            follow_tail,
            anchor_line_id,
            cell_offset,
            rows_before: rows,
            rows_after: if follow_tail { 0 } else { rows },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};

    fn history_app() -> DesktopApp {
        let mut app = DesktopApp::default();
        let mut surface = LogSurfaceModel::default();
        surface
            .submit_page(Arc::new(cshell_render::LogPage {
                source_id: LogSourceId(0),
                revision: 1,
                first_line_id: 1,
                anchor_line_id: 105,
                rows: (100..120)
                    .map(|line_id| cshell_render::LogRow {
                        line_id,
                        text: format!("history {line_id}").into(),
                        soft_wrap: false,
                        style_spans: Default::default(),
                        truncated: false,
                    })
                    .collect::<Vec<_>>()
                    .into(),
                total_line_count: 120,
                has_before: true,
                has_after: false,
            }))
            .unwrap();
        let layout = surface.request_reflow(40).unwrap().execute().unwrap();
        surface.submit_reflow(layout);
        surface.anchor(105, 7);
        app.log_surface = Some(surface);
        app.terminal_scrollback = true;
        app.viewport_rows = 6;
        app
    }

    #[test]
    fn search_return_restores_the_original_anchor_and_fences_other_sessions() {
        let mut app = history_app();
        app.open_terminal_search();
        app.log_surface.as_mut().unwrap().anchor(112, 0);
        app.remember_search_position();
        app.return_to_search_origin();
        assert_eq!(
            app.log_surface.as_ref().unwrap().mode(),
            cshell_render::ScrollMode::Anchored {
                line_id: 105,
                cell_offset: 7
            }
        );
        assert!(!app.terminal_search_open);
        app.open_terminal_search();
        app.log_surface.as_mut().unwrap().anchor(113, 0);
        app.view_model.selected = Some(SessionId::new());
        app.return_to_search_origin();
        assert_eq!(
            app.log_surface.as_ref().unwrap().mode(),
            cshell_render::ScrollMode::Anchored {
                line_id: 113,
                cell_offset: 0
            }
        );
    }

    #[test]
    fn new_search_records_a_new_origin_and_manual_navigation_cancels_pending_jump() {
        let mut app = history_app();
        app.open_terminal_search();
        app.log_surface.as_mut().unwrap().anchor(110, 0);
        app.close_terminal_search();
        app.open_terminal_search();
        app.history_search_direction = Some(HistorySearchDirection::Backward);
        let revision = app.terminal_search_revision;
        app.scroll_terminal_history(-1);
        assert!(app.history_search_direction.is_none());
        assert_ne!(app.terminal_search_revision, revision);
        app.return_to_search_origin();
        assert_eq!(
            app.log_surface.as_ref().unwrap().mode(),
            cshell_render::ScrollMode::Anchored {
                line_id: 110,
                cell_offset: 0
            }
        );
    }

    #[test]
    fn search_return_after_resuming_live_requests_the_original_page() {
        let mut app = history_app();
        let tab = cshell_domain::TabId::new();
        app.current_workspace_tab = Some(tab);
        app.workspace.sessions.insert(tab, SessionId::new());
        app.open_terminal_search();
        app.return_to_live_terminal();
        app.return_to_search_origin();
        let request = app.initial_terminal_history_request();
        assert!(!request.follow_tail);
        assert_eq!((request.anchor_line_id, request.cell_offset), (105, 7));
        assert!(request.rows_after > 0);
    }

    #[test]
    fn search_started_on_a_frozen_live_selection_remembers_its_line() {
        let mut app = DesktopApp::default();
        let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(3, 20));
        terminal.feed(b"selected text\r\n");
        let snapshot = terminal.snapshot();
        let line = snapshot.screen_start_line;
        app.terminal_surface.submit_snapshot(Arc::new(snapshot));
        app.begin_clipboard_selection(TerminalCellPoint { row: 0, column: 0 });
        app.update_clipboard_selection(TerminalCellPoint { row: 0, column: 7 });
        app.finish_clipboard_selection();
        app.open_terminal_search();
        assert!(
            matches!(app.terminal_reading.origin.unwrap().position, Position::History { line: saved, cell: 0 } if saved == line)
        );
    }

    #[test]
    fn returning_from_history_to_a_live_search_origin_resumes_following() {
        let mut app = DesktopApp::default();
        let tab = cshell_domain::TabId::new();
        app.current_workspace_tab = Some(tab);
        app.workspace.sessions.insert(tab, SessionId::new());
        app.open_terminal_search();
        app.scroll_terminal_history(-3);
        app.return_to_search_origin();
        assert!(!app.terminal_scrollback && app.log_surface.is_none());
    }

    #[test]
    fn sustained_tail_output_preserves_copy_and_anchor_with_bounded_history() {
        let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(6, 40));
        terminal.set_history_budget(256 * 1024);
        terminal.feed("uid: 123456\r\n中文日志🙂\r\n".as_bytes());
        let mut app = DesktopApp::default();
        app.viewport_rows = 6;
        app.view_model.selected = Some(SessionId::new());
        app.terminal_surface
            .submit_snapshot(Arc::new(terminal.snapshot()));
        app.begin_clipboard_selection(TerminalCellPoint { row: 0, column: 5 });
        app.update_clipboard_selection(TerminalCellPoint { row: 0, column: 10 });
        app.finish_clipboard_selection();
        assert_eq!(app.terminal_selection_text().unwrap().unwrap(), "123456");
        app.scroll_terminal_history(-1);
        let anchor = app.log_surface.as_ref().unwrap().mode();
        let batch = (0..64)
            .map(|i| format!("{i}: 日志🙂 {{\"uid\":123456}}\r\n"))
            .collect::<String>();
        for _ in 0..128 {
            terminal.feed(batch.as_bytes());
            app.terminal_surface
                .submit_snapshot(Arc::new(terminal.snapshot()));
            app.refresh_terminal_reading();
            app.refresh_history_drag(false);
            assert_eq!(app.terminal_selection_text().unwrap().unwrap(), "123456");
            assert_eq!(app.log_surface.as_ref().unwrap().mode(), anchor);
            assert!(terminal.retained_history_bytes() <= terminal.history_budget_bytes());
        }
        assert_eq!(
            app.view_model.terminal_reading_status.as_deref(),
            Some("回看中 · 有新输出")
        );
        assert!(app.reading_return_label(false).contains("有新输出"));
        app.return_to_live_terminal();
        app.refresh_terminal_reading();
        assert_eq!(
            app.view_model.terminal_reading_status.as_deref(),
            Some("跟随最新")
        );
        assert!(app.terminal_selection_text().is_none());
    }

    #[test]
    fn mac_history_keys_are_optional_and_plain_application_keys_pass_through() {
        use terminal_scrollback::history_key;
        let up = Key::Named(NamedKey::ArrowUp);
        let down = Key::Named(NamedKey::ArrowDown);
        assert_eq!(
            history_key(&up, ModifiersState::SUPER, false, true),
            Some(-1)
        );
        assert_eq!(
            history_key(&down, ModifiersState::SUPER, false, true),
            Some(1)
        );
        assert_eq!(
            history_key(
                &down,
                ModifiersState::SUPER | ModifiersState::SHIFT,
                true,
                true
            ),
            Some(0)
        );
        assert_eq!(history_key(&up, ModifiersState::SUPER, false, false), None);
        for key in [
            up,
            down,
            Key::Named(NamedKey::PageUp),
            Key::Named(NamedKey::PageDown),
        ] {
            assert_eq!(
                history_key(&key, ModifiersState::empty(), false, true),
                None
            );
        }
        assert_eq!(
            history_key(
                &Key::Named(NamedKey::PageDown),
                ModifiersState::SHIFT,
                false,
                false
            ),
            Some(1)
        );
    }
}
