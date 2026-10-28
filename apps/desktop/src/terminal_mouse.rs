//! Terminal application mouse input is always local to the pointed-at session.
use super::*;
use cshell_domain::{MouseButton as B, MouseEvent as E, MouseEventKind as K};

#[derive(Default)]
pub(super) struct State {
    held: Vec<B>,
    owner: Option<(cshell_domain::TabId, SessionId)>,
    last: Option<TerminalCellPoint>,
    motion_at: Option<std::time::Instant>,
    wheel: f64,
    pub zoom: Option<u16>,
    zoom_wheel: f64,
}

impl DesktopApp {
    pub(crate) fn terminal_point_for_grid(
        &self,
        position: winit::dpi::PhysicalPosition<f64>,
        rows: u16,
        cols: u16,
        clamp: bool,
    ) -> Option<TerminalCellPoint> {
        let v = self.terminal_viewport?;
        if rows == 0 || cols == 0 || v.width == 0 || v.height == 0 {
            return None;
        }
        let [width, height] = self
            .renderer
            .as_ref()
            .map(|r| r.terminal_cell_size().map(f64::from))
            .unwrap_or([
                f64::from(v.width) / f64::from(cols),
                f64::from(v.height) / f64::from(rows),
            ]);
        let max_x = (f64::from(v.width)).min(width * f64::from(cols));
        let max_y = (f64::from(v.height)).min(height * f64::from(rows));
        let mut x = position.x - f64::from(v.x);
        let mut y = position.y - f64::from(v.y);
        if clamp {
            x = x.clamp(0.0, (max_x - 0.01).max(0.0));
            y = y.clamp(0.0, (max_y - 0.01).max(0.0));
        } else if x < 0.0 || y < 0.0 || x >= max_x || y >= max_y {
            return None;
        }
        Some(TerminalCellPoint {
            column: ((x / width).floor() as u16).min(cols - 1),
            row: ((y / height).floor() as u16).min(rows - 1),
        })
    }
    pub(super) fn route_terminal_zoom(&mut self, event: &WindowEvent, window: &Window) -> bool {
        if !self.terminal_keyboard_available()
            || self.modifiers.alt_key()
            || self.modifiers.shift_key()
            || !self.modifiers.control_key()
        {
            return false;
        }
        let base = self
            .view_model
            .terminal_font_pixels
            .unwrap_or(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS);
        match event {
            WindowEvent::MouseWheel { delta, .. }
                if self.clipboard_pointer_available(window.scale_factor() as f32) =>
            {
                let step = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(*y),
                    MouseScrollDelta::PixelDelta(p) => p.y / 40.0,
                };
                self.terminal_mouse.zoom_wheel =
                    (self.terminal_mouse.zoom_wheel + step).clamp(-4.0, 4.0);
                let change = self.terminal_mouse.zoom_wheel.trunc() as i32;
                self.terminal_mouse.zoom_wheel -= f64::from(change);
                if change != 0 {
                    let pixels = (i32::from(self.terminal_mouse.zoom.unwrap_or(base)) + change * 2)
                        .clamp(
                            i32::from(cshell_domain::MIN_TERMINAL_FONT_PIXELS),
                            i32::from(cshell_domain::MAX_TERMINAL_FONT_PIXELS),
                        ) as u16;
                    self.terminal_mouse.zoom = (pixels != base).then_some(pixels);
                    self.view_model.terminal_zoom_pixels = self.terminal_mouse.zoom;
                    self.clear_clipboard_selection();
                    self.clipboard_notice(format!("当前窗口 {pixels} px · Ctrl+0 恢复默认"));
                }
                true
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && !self.egui_context.egui_wants_keyboard_input()
                    && matches!(&event.logical_key,Key::Character(s) if s=="0") =>
            {
                self.terminal_mouse.zoom = None;
                self.view_model.terminal_zoom_pixels = None;
                self.clear_clipboard_selection();
                self.clipboard_notice(format!("已恢复全局字号 {base} px"));
                true
            }
            _ => false,
        }
    }
    fn mouse_modes(&self) -> cshell_terminal::TerminalModes {
        self.terminal_surface
            .latest_snapshot()
            .map_or_else(Default::default, |s| s.terminal_modes)
    }
    fn mouse_modifiers(&self) -> cshell_domain::Modifiers {
        cshell_domain::Modifiers {
            ctrl: self.modifiers.control_key(),
            alt: self.modifiers.alt_key(),
            shift: self.modifiers.shift_key(),
            super_key: self.modifiers.super_key(),
        }
    }
    fn mouse_point(&self) -> Option<TerminalCellPoint> {
        let viewport = self.terminal_viewport?;
        if viewport.width == 0 || viewport.height == 0 {
            return None;
        }
        let s = self.terminal_surface.latest_snapshot()?;
        let pos = self.cursor_position?;
        self.terminal_point_for_grid(pos, s.rows, s.cols, true)
    }
    fn send_mouse(&mut self, button: B, kind: K, point: TerminalCellPoint) -> bool {
        let event = E {
            row: point.row,
            column: point.column,
            button,
            kind,
            modifiers: self.mouse_modifiers(),
        };
        // Deliberately bypass the keyboard synchronization router.
        self.daemon
            .as_ref()
            .is_some_and(|d| d.send_input(InputAction::Mouse(event)))
    }
    pub(super) fn release_remote_mouse(&mut self) {
        if self
            .current_workspace_tab
            .zip(self.active_terminal_session_id())
            == self.terminal_mouse.owner
        {
            let held = std::mem::take(&mut self.terminal_mouse.held);
            if let Some(point) = self.terminal_mouse.last {
                for button in held {
                    self.send_mouse(button, K::Release, point);
                }
            }
        }
        self.terminal_mouse.held.clear();
        self.terminal_mouse.owner = None;
        self.terminal_mouse.last = None;
    }
    pub(super) fn route_remote_mouse(&mut self, event: &WindowEvent, window: &Window) -> bool {
        if !self.window_active {
            return false;
        }
        let owner = self
            .current_workspace_tab
            .zip(self.active_terminal_session_id());
        if owner != self.terminal_mouse.owner {
            self.terminal_mouse.held.clear();
            self.terminal_mouse.last = None;
        }
        let modes = self.mouse_modes();
        if modes.mouse_tracking == 0
            || self.log_surface.is_some()
            || !self.terminal_keyboard_available()
        {
            self.release_remote_mouse();
            return false;
        }
        let over_current=self.cursor_position.zip(self.terminal_viewport).is_some_and(|(p,v)|p.x>=f64::from(v.x)&&p.x<f64::from(v.x+v.width)&&p.y>=f64::from(v.y)&&p.y<f64::from(v.y+v.height));
        let available = self.clipboard_pointer_available(window.scale_factor() as f32)&&over_current;
        match event {
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => B::Left,
                    MouseButton::Middle => B::Middle,
                    MouseButton::Right => B::Right,
                    _ => return false,
                };
                let held = self.terminal_mouse.held.contains(&button);
                if *state == ElementState::Released && held {
                    if let Some(point) = self.mouse_point().or(self.terminal_mouse.last) {
                        self.send_mouse(button, K::Release, point);
                    }
                    self.terminal_mouse.held.retain(|b| *b != button);
                    return true;
                }
                if !available || self.modifiers.shift_key() || self.terminal_selecting {
                    return false;
                }
                if *state == ElementState::Pressed
                    && let Some(point) = self.mouse_point()
                {
                    self.clear_clipboard_selection();
                    self.egui_context.memory_mut(|m| {
                        if let Some(id) = m.focused() {
                            m.surrender_focus(id);
                        }
                    });
                    if self.send_mouse(button, K::Press, point) {
                        self.terminal_mouse.held.push(button);
                        self.terminal_mouse.owner = owner;
                        self.terminal_mouse.last = Some(point);
                    } else {
                        self.view_model.input_warning =
                            Some("鼠标操作未入队，请检查当前连接".into());
                    }
                    return true;
                }
                // A local Shift-gesture release must never become a remote release.
                *state == ElementState::Released
            }
            WindowEvent::CursorMoved { .. } => {
                if self.modifiers.shift_key()
                    || self.terminal_selecting
                    || (!available && self.terminal_mouse.held.is_empty())
                {
                    return false;
                }
                if modes.mouse_tracking < 2
                    || (modes.mouse_tracking < 3 && self.terminal_mouse.held.is_empty())
                {
                    return false;
                }
                let Some(point) = self.mouse_point() else {
                    return false;
                };
                let now = std::time::Instant::now();
                if self.terminal_mouse.last == Some(point)
                    || self.terminal_mouse.motion_at.is_some_and(|at| {
                        now.duration_since(at) < std::time::Duration::from_millis(16)
                    })
                {
                    return true;
                }
                let button = self.terminal_mouse.held.last().copied().unwrap_or(B::None);
                if self.send_mouse(button, K::Move, point) {
                    self.terminal_mouse.last = Some(point);
                    self.terminal_mouse.motion_at = Some(now);
                    self.terminal_mouse.owner = owner;
                }
                true
            }
            WindowEvent::MouseWheel { delta, .. } if available && !self.modifiers.shift_key() => {
                let Some(point) = self.mouse_point() else {
                    return false;
                };
                let rows = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(*y),
                    MouseScrollDelta::PixelDelta(p) => {
                        p.y / (f64::from(self.terminal_viewport.map_or(1, |v| v.height))
                            / f64::from(self.viewport_rows.max(1)))
                        .max(1.0)
                    }
                };
                self.terminal_mouse.wheel = (self.terminal_mouse.wheel + rows).clamp(-8.0, 8.0);
                let count = self.terminal_mouse.wheel.trunc() as i32;
                self.terminal_mouse.wheel -= f64::from(count);
                for _ in 0..count.unsigned_abs() {
                    self.send_mouse(
                        if count > 0 { B::WheelUp } else { B::WheelDown },
                        K::Press,
                        point,
                    );
                }
                true
            }
            _ => false,
        }
    }
}
