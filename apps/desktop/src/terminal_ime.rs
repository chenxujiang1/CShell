//! A single native IME owner for the GPU terminal and egui editors.
use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Target {
    #[default]
    None,
    Editor(Option<egui::Id>),
    Terminal {
        tab: Option<cshell_domain::TabId>,
        session: SessionId,
        epoch: cshell_domain::ConnectionId,
    },
}

#[derive(Default)]
pub(super) struct State {
    target: Target,
    composing: bool,
    pending: bool,
    fenced: bool,
    interrupt: bool,
    preedit: String,
    suppressed_keys: Vec<winit::keyboard::PhysicalKey>,
}

impl State {
    pub(super) fn set_target(&mut self, target: Target) {
        if self.target != target {
            self.interrupt |= self.pending;
            self.fenced |= self.pending;
            self.pending = false;
            self.composing = false;
            self.preedit.clear();
            self.target = target;
        }
    }

    pub(super) fn cancel(&mut self) {
        self.set_target(Target::None);
    }

    pub(super) fn discard_for_reconnect(&mut self) {
        self.cancel();
        self.fenced = true;
        self.interrupt = true;
        self.pending = false;
        self.composing = false;
        self.preedit.clear();
    }

    /// Returns whether egui must not receive this event, plus committed text.
    pub(super) fn event(&mut self, target: Target, ime: &Ime) -> (bool, Option<InputAction>) {
        self.set_target(target);
        let terminal = matches!(target, Target::Terminal { .. });
        let mut action = None;
        let mut handled = terminal || target == Target::None;
        match ime {
            Ime::Enabled => self.fenced = false,
            Ime::Preedit(text, _) => {
                self.preedit.clear();
                self.composing = !text.is_empty();
                if !text.is_empty() {
                    self.pending = true;
                    self.fenced = false;
                    if terminal {
                        let mut end = text.len().min(4096);
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        self.preedit.push_str(&text[..end]);
                    }
                }
            }
            Ime::Commit(text) => {
                handled |= self.fenced;
                if terminal && !self.fenced && !text.is_empty() {
                    action = Some(InputAction::Text(text.clone()));
                }
                self.preedit.clear();
                self.composing = false;
                self.pending = false;
            }
            Ime::Disabled => {
                self.preedit.clear();
                self.composing = false;
                self.pending = false;
            }
        }
        (handled, action)
    }

    pub(super) fn suppress_key(
        &mut self,
        physical_key: winit::keyboard::PhysicalKey,
        state: ElementState,
        processed: bool,
        force_interrupt: bool,
    ) -> bool {
        if state == ElementState::Released
            && let Some(index) = self
                .suppressed_keys
                .iter()
                .position(|key| *key == physical_key)
        {
            self.suppressed_keys.swap_remove(index);
            return true;
        }
        if (self.composing || processed)
            && matches!(self.target, Target::Terminal { .. })
            && !force_interrupt
        {
            if state == ElementState::Pressed
                && self.suppressed_keys.len() < 64
                && !self.suppressed_keys.contains(&physical_key)
            {
                self.suppressed_keys.push(physical_key);
            }
            return true;
        }
        false
    }

    pub(super) fn output(&mut self, output: &mut egui::PlatformOutput, rect: Option<egui::Rect>) {
        if matches!(self.target, Target::Terminal { .. }) {
            output.ime = rect.map(|rect| egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Terminal,
                rect,
                cursor_rect: rect,
                should_interrupt_composition: false,
            });
        }
        if let Some(ime) = &mut output.ime {
            ime.should_interrupt_composition |= std::mem::take(&mut self.interrupt);
        } else {
            self.interrupt = false;
        }
    }

    pub(super) fn paint(&self, ctx: &egui::Context, rect: Option<egui::Rect>, clip: egui::Rect) {
        if !matches!(self.target, Target::Terminal { .. }) || self.preedit.is_empty() {
            return;
        }
        let Some(rect) = rect else {
            return;
        };
        let painter = ctx
            .layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("terminal-ime-preedit"),
            ))
            .with_clip_rect(clip);
        let font = egui::FontId::monospace(rect.height().clamp(12.0, 40.0));
        let galley = painter.layout_no_wrap(self.preedit.clone(), font, egui::Color32::WHITE);
        let size = galley.size();
        let origin = egui::pos2(
            rect.left().min((clip.right() - size.x).max(clip.left())),
            rect.top(),
        );
        let background = egui::Rect::from_min_size(origin, size);
        painter.rect_filled(background, 0.0, cshell_ui::theme::PANEL);
        painter.galley(origin, galley, egui::Color32::WHITE);
        painter.line_segment(
            [background.left_bottom(), background.right_bottom()],
            egui::Stroke::new(1.0, egui::Color32::WHITE),
        );
    }
}

pub(super) fn disable(window: &Window, state: Option<&mut egui_winit::State>) {
    window.set_ime_allowed(false);
    if let Some(state) = state {
        state.set_allow_ime(false);
    }
}

pub(super) fn cursor_rect(
    viewport: TerminalViewport,
    cell: [f32; 2],
    snapshot: &cshell_terminal::FrameSnapshot,
    scale: f32,
) -> egui::Rect {
    let scale = scale.max(0.01);
    let [width, height] = cell.map(|n| n.max(1.0));
    let x = viewport.x as f32 + f32::from(snapshot.cursor_col) * width;
    let y = viewport.y as f32 + f32::from(snapshot.cursor_row) * height;
    let x = x.clamp(
        viewport.x as f32,
        viewport.x.saturating_add(viewport.width.saturating_sub(1)) as f32,
    );
    let y = y.clamp(
        viewport.y as f32,
        viewport.y.saturating_add(viewport.height.saturating_sub(1)) as f32,
    );
    egui::Rect::from_min_size(
        egui::pos2(x / scale, y / scale),
        egui::vec2(width / scale, height / scale),
    )
}

impl DesktopApp {
    pub(super) fn ime_target(&self) -> Target {
        if !self.window_active {
            return Target::None;
        }
        if self.egui_context.egui_wants_keyboard_input() {
            return Target::Editor(self.egui_context.memory(|m| m.focused()));
        }
        if self.terminal_keyboard_available()
            && self.terminal_clipboard.menu.is_none()
            && let Some(session) = self.active_terminal_session_id()
            && let Some(view) = self.daemon.as_ref().map(|d| d.view())
        {
            Target::Terminal {
                tab: self.current_workspace_tab,
                session,
                epoch: view.input_epoch,
            }
        } else {
            Target::None
        }
    }

    pub(super) fn ime_rect(&self, terminal: egui::Rect, scale: f32) -> Option<egui::Rect> {
        let renderer = self.renderer.as_ref()?;
        let snapshot = self.terminal_surface.latest_snapshot()?;
        let pane = self
            .view_model
            .pane_views
            .iter()
            .find(|pane| pane.tab_id == self.current_workspace_tab)
            .map_or(terminal, |pane| pane.rect);
        if !pane.is_positive() {
            return None;
        }
        Some(cursor_rect(
            TerminalViewport::from_logical_rect(pane, scale),
            renderer.terminal_cell_size(),
            snapshot,
            scale,
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
