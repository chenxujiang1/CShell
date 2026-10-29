use super::*;

impl DesktopApp {
    pub(super) fn jump_to_command(&mut self,line:u64) {
        self.scroll_terminal_history(-1);
        if let Some(surface)=&mut self.log_surface {surface.anchor(line,0);}
        if let Some(daemon)=&self.daemon {daemon.request_terminal_history_page(cshell_render::LogPageRequest{source_id:cshell_render::LogSourceId(0),follow_tail:false,anchor_line_id:line,cell_offset:0,rows_before:0,rows_after:self.viewport_rows.saturating_add(64)});}
        self.egui_context.memory_mut(|m|{if let Some(id)=m.focused(){m.surrender_focus(id);}});
    }

    pub(super) fn terminal_wheel_available(&self, scale: f32) -> bool {
        if self.viewport_rows == 0
            || self.command_center.blocks_input()
            || self.profile_panel.is_dialog_open()
            || self.zmodem.blocks_input()
            || self.keyboard_auth.blocks_input()
            || self.view_model.about_open
            || self.view_model.confirm_terminate
            || self.view_model.tab_navigator.open
            || self.pending_paste.is_some()
            || self.terminal_tools_panel.open
            || self.profile_batch_panel.open
            || self.terminal_policy_panel.open
            || self.workspace_recovery_panel.open
        {
            return false;
        }
        let Some(cursor) = self.cursor_position else {
            return false;
        };
        let logical = egui::pos2(cursor.x as f32 / scale, cursor.y as f32 / scale);
        let over_terminal = if self.view_model.pane_views.is_empty() {
            self.terminal_viewport.is_some_and(|viewport| {
                cursor.x >= f64::from(viewport.x)
                    && cursor.x < f64::from(viewport.x.saturating_add(viewport.width))
                    && cursor.y >= f64::from(viewport.y)
                    && cursor.y < f64::from(viewport.y.saturating_add(viewport.height))
            })
        } else {
            self.view_model
                .pane_views
                .iter()
                .any(|pane| pane.rect.contains(logical))
        };
        over_terminal
            && !self.egui_context.egui_is_using_pointer()
            && self
                .egui_context
                .layer_id_at(logical)
                .is_some_and(|layer| layer.order == egui::Order::Background)
    }

    pub(super) fn handle_terminal_wheel(&mut self, delta: MouseScrollDelta) {
        let rows = match delta {
            MouseScrollDelta::LineDelta(_, vertical) => f64::from(vertical) * 3.0,
            MouseScrollDelta::PixelDelta(position) => {
                let cell_height = self
                    .terminal_viewport
                    .map_or(1.0, |viewport| {
                        f64::from(viewport.height) / f64::from(self.viewport_rows.max(1))
                    })
                    .max(1.0);
                position.y / cell_height
            }
        };
        self.wheel_row_accumulator -= rows;
        let whole_rows = self.wheel_row_accumulator.trunc() as i32;
        if whole_rows != 0 {
            self.wheel_row_accumulator -= f64::from(whole_rows);
            if self.task_panel.open {
                self.task_panel.scroll(whole_rows, self.viewport_rows);
            } else {
                self.scroll_terminal_history(whole_rows);
            }
        }
    }

    pub(super) fn return_to_live_terminal(&mut self) {
        self.cancel_history_search_jump();
        self.terminal_click = TerminalClickState::default();
        self.clear_clipboard_selection();
        if !self.terminal_scrollback {
            return;
        }
        self.terminal_scrollback = false;
        self.log_surface = None;
        self.log_decorations = LogDecorations::default();
        self.submitted_log_page = None;
        self.wheel_row_accumulator = 0.0;
        if let Some(daemon) = &self.daemon {
            daemon.cancel_log_page();
        }
    }
    pub(super) fn apply_terminal_scroll_navigation(&mut self, live: bool, action: Option<LogScrollbarAction>) {
        if live || action.is_some() {
            self.cancel_history_search_jump();
            // A UI navigation gesture breaks the terminal's multi-click chain.
            self.terminal_click = TerminalClickState::default();
        }
        if live || (self.terminal_scrollback && matches!(action, Some(LogScrollbarAction::FollowTail))) {
            self.return_to_live_terminal();
        } else if let Some(action) = action {
            // Explicit navigation releases the frozen selection image too.
            self.clear_clipboard_selection();
            if let Some(surface) = &mut self.log_surface {
                match action {
                    LogScrollbarAction::FollowTail => surface.follow_tail(),
                    LogScrollbarAction::JumpTo(line) => { surface.jump_to_line(line, 0); }
                }
            }
        }
    }
    pub(super) fn scroll_terminal_history(&mut self, rows: i32) {
        if rows != 0 { self.cancel_history_search_jump(); self.pause_terminal_reading(); }
        if rows != 0 { self.terminal_click = TerminalClickState::default(); }
        let keep = self.terminal_decorations.selection.is_some() && self.start_history_drag();
        self.scroll_terminal_history_kept(rows,keep);
    }
    pub(super) fn scroll_terminal_history_kept(&mut self, rows: i32, keep:bool) {
        if rows == 0 {
            return;
        }
        if !keep { self.clear_clipboard_selection(); }
        if self.log_surface.is_none() {
            let bound = self
                .current_workspace_tab
                .is_some_and(|tab| self.workspace.sessions.contains_key(&tab));
            if rows > 0 || (self.active_terminal_session_id().is_none() && !bound) {
                return;
            }
            if self.log_reflow.is_none() {
                match LogReflowWorker::start() {
                    Ok(worker) => self.log_reflow = Some(worker),
                    Err(error) => {
                        self.view_model.input_warning = Some(format!("历史查看无法启动：{error}"));
                        return;
                    }
                }
            }
            self.terminal_scrollback = true;
            self.log_surface = Some(LogSurfaceModel::default());
            self.submitted_log_page = None;
            self.log_decorations = LogDecorations::default();
            if let Some(daemon) = &self.daemon {
                // Re-enter at the current tail, never the previous view's page.
                daemon.cancel_log_page();
                daemon.request_terminal_history_page(cshell_render::LogPageRequest {
                    source_id: cshell_render::LogSourceId(0),
                    follow_tail: true,
                    anchor_line_id: 0,
                    cell_offset: 0,
                    rows_before: self.viewport_rows.saturating_add(128).min(4095),
                    rows_after: 0,
                });
            }
        }
        if let Some(surface) = &mut self.log_surface {
            surface.scroll_visual_rows(rows, self.viewport_rows);
            if rows > 0
                && self.terminal_scrollback
                && matches!(surface.mode(), cshell_render::ScrollMode::FollowTail)
            {
                if !keep { self.return_to_live_terminal(); }
            }
        }
    }
}

pub(super) fn history_shortcut(
    event: &winit::event::KeyEvent,
    modifiers: ModifiersState,
    active: bool,
) -> Option<i32> {
    if event.state != ElementState::Pressed {
        return None;
    }
    match event.logical_key {
        ref key => history_key(key, modifiers, active, terminal_clipboard_settings::get().mac_history_shortcuts),
    }
}

pub(super) fn history_key(key: &Key, modifiers: ModifiersState, active: bool, mac: bool) -> Option<i32> {
    if modifiers.alt_key() || modifiers.control_key() { return None; }
    match key {
        Key::Named(NamedKey::PageUp) if modifiers.shift_key() && !modifiers.super_key() => Some(-1),
        Key::Named(NamedKey::PageDown) if modifiers.shift_key() && !modifiers.super_key() => Some(1),
        Key::Named(NamedKey::End) if active && modifiers.is_empty() => Some(0),
        Key::Named(NamedKey::ArrowUp) if mac && modifiers == ModifiersState::SUPER => Some(-1),
        Key::Named(NamedKey::ArrowDown) if mac && modifiers == ModifiersState::SUPER => Some(1),
        Key::Named(NamedKey::ArrowDown) if mac && active && modifiers == (ModifiersState::SUPER | ModifiersState::SHIFT) => Some(0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workbench_terminal_wheel_bypasses_central_panel_but_not_overlays() {
        let context = egui::Context::default();
        let mut model = cshell_ui::WorkbenchViewModel::default();
        let mut terminal = egui::Rect::NOTHING;
        for _ in 0..3 {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 800.0),
                    )),
                    events: if terminal.is_positive() {
                        vec![egui::Event::PointerMoved(terminal.center())]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                },
                |ui| {
                    cshell_ui::draw_workbench_chrome(ui, &mut model);
                    terminal = cshell_ui::draw_workbench_content(ui, &mut model);
                },
            );
            output.textures_delta.clear();
        }
        assert!(
            context.egui_wants_pointer_input(),
            "reproduce egui-winit's consumed wheel flag"
        );
        let mut app = DesktopApp::default();
        app.egui_context = context.clone();
        app.viewport_rows = 24;
        app.terminal_viewport = Some(TerminalViewport::from_logical_rect(terminal, 1.0));
        app.cursor_position = Some(winit::dpi::PhysicalPosition::new(
            terminal.center().x as f64,
            terminal.center().y as f64,
        ));
        assert!(app.terminal_wheel_available(1.0));
        app.cursor_position = Some(winit::dpi::PhysicalPosition::new(10.0, 10.0));
        assert!(
            !app.terminal_wheel_available(1.0),
            "toolbar belongs to egui"
        );
        app.cursor_position = Some(winit::dpi::PhysicalPosition::new(
            terminal.center().x as f64,
            terminal.center().y as f64,
        ));
        for _ in 0..3 {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 800.0),
                    )),
                    events: vec![egui::Event::PointerMoved(terminal.center())],
                    ..Default::default()
                },
                |ui| {
                    cshell_ui::draw_workbench_chrome(ui, &mut model);
                    let _ = cshell_ui::draw_workbench_content(ui, &mut model);
                    egui::Window::new("overlay")
                        .fixed_pos(terminal.center() - egui::vec2(50.0, 50.0))
                        .fixed_size(egui::vec2(300.0, 200.0))
                        .show(ui.ctx(), |ui| {
                            ui.label("transfer details");
                        });
                },
            );
            output.textures_delta.clear();
        }
        assert!(
            !app.terminal_wheel_available(1.0),
            "floating window must retain its own wheel"
        );
    }

    #[test]
    fn a_single_wheel_enters_history_and_return_restores_keyboard() {
        let mut app = DesktopApp::default();
        app.viewport_rows = 24;
        let tab = cshell_domain::TabId::new();
        let session = SessionId::new();
        app.workspace.sessions.insert(tab, session);
        app.current_workspace_tab = Some(tab);
        app.scroll_terminal_history(-3);
        assert!(app.terminal_scrollback && app.log_surface.is_some());
        assert!(app.log_reflow.is_some());
        app.return_to_live_terminal();
        assert!(!app.terminal_scrollback && app.log_surface.is_none());
        assert!(app.log_surface.is_none());
    }
}
