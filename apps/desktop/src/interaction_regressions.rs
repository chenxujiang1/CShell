//! Window focus ownership survives application switches independently of drafts.
use super::*;

pub(super) fn intercepts_native_press(event: &WindowEvent, available: bool) -> bool {
    // Even if the release lands over the terminal, egui must see it: a UI
    // press can still be queued when a fast pointer crosses into the pane.
    available
        && matches!(
            event,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            }
        )
}

pub(super) fn return_button_rect(pane: egui::Rect) -> egui::Rect {
    let width = (pane.width() - 28.0).clamp(0.0, 290.0);
    egui::Rect::from_min_size(
        pane.right_bottom() - egui::vec2(width + 20.0, 34.0),
        egui::vec2(width, 28.0),
    )
}

impl DesktopApp {
    pub(super) fn draw_terminal_return_button(
        &mut self,
        ui: &mut egui::Ui,
        pane: egui::Rect,
    ) -> bool {
        if self.task_panel.open
            || !(self.terminal_scrollback
                || self.terminal_clipboard.snapshot.is_some() && !self.terminal_selecting)
        {
            return false;
        }
        let rect = return_button_rect(pane);
        self.terminal_ui_regions.push(rect);
        let text = if rect.width() < 250.0 {
            "返回最新 [End]"
        } else {
            "正在回看历史 · 返回最新 [End]"
        };
        ui.put(
            rect,
            egui::Button::new(text)
                .truncate()
                .fill(cshell_ui::theme::PANEL),
        )
        .clicked()
    }

    pub(super) fn draw_terminal_history_scrollbar(
        &mut self,
        ui: &mut egui::Ui,
        rect: &mut egui::Rect,
        state: LogScrollbarState,
    ) -> Option<LogScrollbarAction> {
        self.terminal_ui_regions.push(log_scrollbar_rect(*rect));
        draw_log_scrollbar(ui, rect, state)
    }
    pub(super) fn restore_window_focus(&mut self, active: bool) {
        if self.window_active != active {
            if !active {
                self.focus_before_deactivate = Some(self.egui_context.memory(|m| m.focused()));
            } else if let Some(previous) = self.focus_before_deactivate.take() {
                self.egui_context.memory_mut(|memory| {
                    if let Some(previous) = previous {
                        memory.request_focus(previous);
                    } else if let Some(current) = memory.focused() {
                        memory.surrender_focus(current);
                    }
                });
            }
        }
        self.window_active = active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_switch_restores_terminal_search_or_editor_focus() {
        let command = egui::Id::new("command-editor");
        for previous in [None, Some(command), Some(egui::Id::new("terminal-search"))] {
            let mut app = DesktopApp {
                window_active: true,
                ..Default::default()
            };
            if let Some(id) = previous {
                app.egui_context.memory_mut(|m| m.request_focus(id));
            }
            app.restore_window_focus(false);
            app.egui_context.memory_mut(|m| m.request_focus(command));
            // Duplicate blur notifications cannot overwrite the original owner.
            app.restore_window_focus(false);
            app.restore_window_focus(true);
            assert_eq!(app.egui_context.memory(|m| m.focused()), previous);
        }
    }

    fn selected_app(history: bool) -> DesktopApp {
        let mut app = DesktopApp::default();
        let mut terminal = cshell_terminal::AlacrittyTerminalEngine::new(
            cshell_domain::TerminalSize::cells(2, 16),
        );
        use cshell_terminal::TerminalEngine;
        terminal.feed(b"selected text");
        app.terminal_surface
            .submit_snapshot(Arc::new(terminal.snapshot()));
        app.terminal_scrollback = history;
        if history {
            let mut surface = LogSurfaceModel::default();
            surface
                .submit_page(Arc::new(cshell_render::LogPage {
                    source_id: cshell_render::LogSourceId(0),
                    revision: 1,
                    first_line_id: 1,
                    anchor_line_id: 50,
                    rows: (1..=100)
                        .map(|line_id| cshell_render::LogRow {
                            line_id,
                            text: format!("history {line_id}").into(),
                            soft_wrap: false,
                            style_spans: Default::default(),
                            truncated: false,
                        })
                        .collect::<Vec<_>>()
                        .into(),
                    total_line_count: 100,
                    has_before: false,
                    has_after: false,
                }))
                .unwrap();
            let layout = surface.request_reflow(64).unwrap().execute().unwrap();
            surface.submit_reflow(layout);
            surface.anchor(50, 0);
            app.viewport_rows = 10;
            app.log_surface = Some(surface);
        }
        app.begin_clipboard_selection(TerminalCellPoint { row: 0, column: 0 });
        app.update_clipboard_selection(TerminalCellPoint { row: 0, column: 7 });
        app.finish_clipboard_selection();
        assert!(app.terminal_clipboard.snapshot.is_some());
        app
    }

    #[test]
    fn returning_to_latest_releases_a_frozen_live_or_history_selection() {
        for history in [false, true] {
            let mut app = selected_app(history);
            app.apply_terminal_scroll_navigation(true, None);
            assert!(app.terminal_clipboard.snapshot.is_none());
            assert!(app.terminal_decorations.selection.is_none());
            assert!(!app.terminal_selecting && !app.terminal_scrollback);
            assert!(app.log_surface.is_none());
            app.begin_clipboard_selection(TerminalCellPoint { row: 0, column: 0 });
            assert_eq!(
                app.terminal_decorations.selection.unwrap().mode,
                TerminalSelectionMode::Character,
                "a click after using navigation must not become a double-click copy"
            );
        }
    }

    #[test]
    fn scrollbar_navigation_releases_selection_without_leaving_history() {
        let mut app = selected_app(true);
        app.apply_terminal_scroll_navigation(false, Some(LogScrollbarAction::JumpTo(42)));
        assert!(app.terminal_clipboard.snapshot.is_none());
        assert!(app.terminal_decorations.selection.is_none());
        assert!(app.terminal_scrollback);
        assert_eq!(
            app.log_surface.as_ref().unwrap().mode(),
            cshell_render::ScrollMode::Anchored {
                line_id: 42,
                cell_offset: 0
            }
        );
        app.apply_terminal_scroll_navigation(false, Some(LogScrollbarAction::FollowTail));
        assert!(!app.terminal_scrollback && app.log_surface.is_none());
    }

    fn pointer_frame(app: &mut DesktopApp, events: Vec<egui::Event>) {
        let ctx = app.egui_context.clone();
        let mut action = None;
        let mut live = false;
        app.terminal_ui_regions.clear();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 400.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let mut pane =
                    egui::Rect::from_min_max(egui::pos2(10.0, 30.0), egui::pos2(630.0, 380.0));
                ui.allocate_rect(pane, egui::Sense::hover());
                app.view_model.pane_views = vec![cshell_ui::WorkspacePaneView {
                    group_id: cshell_domain::TabGroupId::new(),
                    tab_id: None,
                    rect: pane,
                }];
                live = app.draw_terminal_return_button(ui, pane);
                action = app.draw_terminal_history_scrollbar(
                    ui,
                    &mut pane,
                    LogScrollbarState {
                        first_line_id: 1,
                        top_line_id: 50,
                        total_line_count: 100,
                        viewport_rows: 10,
                        follow_tail: false,
                    },
                );
                app.terminal_viewport = Some(TerminalViewport::from_logical_rect(pane, 1.0));
                app.viewport_rows = 10;
            },
        );
        output.textures_delta.clear();
        app.apply_terminal_scroll_navigation(live, action);
    }

    fn mouse(state: ElementState) -> WindowEvent {
        WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        }
    }

    #[test]
    fn selected_text_does_not_capture_scrollbar_or_return_button_clicks() {
        for control in [0, 1] {
            let mut app = selected_app(true);
            app.egui_context = crate::ui_layout_gate::context();
            for _ in 0..3 {
                pointer_frame(&mut app, vec![]);
            }
            let rect = app.terminal_ui_regions[control];
            let point = if control == 0 {
                rect.center()
            } else {
                egui::pos2(rect.center().x, rect.top() + rect.height() * 0.25)
            };
            for pressed in [true, false] {
                app.cursor_position = Some(winit::dpi::PhysicalPosition::new(
                    f64::from(point.x),
                    f64::from(point.y),
                ));
                assert!(
                    !app.clipboard_pointer_available(1.0),
                    "UI control was captured by selection"
                );
                let state = if pressed {
                    ElementState::Pressed
                } else {
                    ElementState::Released
                };
                assert!(!intercepts_native_press(
                    &mouse(state),
                    app.clipboard_pointer_available(1.0)
                ));
                pointer_frame(
                    &mut app,
                    vec![
                        egui::Event::PointerMoved(point),
                        egui::Event::PointerButton {
                            pos: point,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(app.terminal_clipboard.snapshot.is_none());
            assert!(!app.terminal_selecting);
            assert_eq!(app.terminal_scrollback, control != 0);
        }
    }

    #[test]
    fn fast_ui_press_and_release_over_terminal_never_leaves_ui_pointer_held() {
        let mut app = selected_app(true);
        app.egui_context = crate::ui_layout_gate::context();
        for _ in 0..3 {
            pointer_frame(&mut app, vec![]);
        }
        let control = app.terminal_ui_regions[1].center();
        let terminal = egui::pos2(100.0, 100.0);
        // Both native events arrive before the next UI frame.
        let mut events = Vec::new();
        for (pos, state, native) in [
            (control, ElementState::Pressed, false),
            (terminal, ElementState::Released, true),
        ] {
            if !intercepts_native_press(&mouse(state), native) {
                events.push(egui::Event::PointerMoved(pos));
                events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: state == ElementState::Pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        assert_eq!(
            events.len(),
            4,
            "a release cannot be swallowed by the terminal"
        );
        pointer_frame(&mut app, events);
        assert!(!app.egui_context.input(|i| i.pointer.primary_down()));
        pointer_frame(&mut app, vec![]);
        assert!(!app.egui_context.egui_is_using_pointer());
    }

    #[test]
    fn return_button_stays_inside_narrow_pane_and_clear_of_scrollbar() {
        for width in [80.0, 160.0, 640.0] {
            let pane = egui::Rect::from_min_size(egui::pos2(10.0, 30.0), egui::vec2(width, 300.0));
            let button = return_button_rect(pane);
            assert!(pane.contains_rect(button));
            assert!(!button.intersects(log_scrollbar_rect(pane)));
        }
    }

    #[test]
    fn scaled_scroll_controls_keep_mouse_ownership_without_disabling_the_wheel() {
        let mut app = selected_app(true);
        app.egui_context = crate::ui_layout_gate::context();
        for _ in 0..3 {
            pointer_frame(&mut app, vec![]);
        }
        for scale in [1.0_f32, 1.5, 2.0] {
            for rect in app.terminal_ui_regions.clone() {
                let pos = rect.center();
                app.cursor_position = Some(winit::dpi::PhysicalPosition::new(
                    f64::from(pos.x * scale),
                    f64::from(pos.y * scale),
                ));
                assert!(!app.clipboard_pointer_available(scale));
                assert!(
                    app.terminal_wheel_available(scale),
                    "hovering controls must not disable terminal wheel scrolling"
                );
            }
        }
    }
}
