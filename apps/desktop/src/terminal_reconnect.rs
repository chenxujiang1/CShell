//! Typing in a confirmed disconnected SSH tab is an explicit reconnect gesture.
use super::*;
use cshell_ui::SshConnectionState;
use winit::keyboard::PhysicalKey;

#[derive(Default)]
pub(super) struct State {
    held: Vec<PhysicalKey>,
}

impl State {
    /// Keep activation repeats and key-up out of a newly connected shell, even
    /// if it negotiated Kitty key release reporting before the user lets go.
    pub(super) fn consume_held(
        &mut self,
        key: PhysicalKey,
        state: ElementState,
        repeated: bool,
    ) -> bool {
        let Some(index) = self.held.iter().position(|held| *held == key) else {
            return false;
        };
        if state == ElementState::Released || !repeated {
            self.held.swap_remove(index);
        }
        // A fresh press also recovers from a release lost while unfocused.
        state == ElementState::Released || repeated
    }

    fn hold(&mut self, key: PhysicalKey) {
        if self.held.len() < 64 && !self.held.contains(&key) {
            self.held.push(key);
        }
    }
}

fn activation(key: &Key, modifiers: ModifiersState) -> bool {
    if modifiers.control_key() || modifiers.alt_key() || modifiers.super_key() {
        return false;
    }
    match key {
        Key::Character(text) => !text.is_empty() && !text.chars().any(char::is_control),
        Key::Named(NamedKey::Enter | NamedKey::Space | NamedKey::Process) => true,
        _ => false,
    }
}

impl DesktopApp {
    pub(super) fn reconnect_from_key(
        &mut self,
        key: DesktopKeyInput<'_>,
        physical: PhysicalKey,
    ) -> bool {
        if self.terminal_reconnect.consume_held(
            physical,
            if key.pressed {
                ElementState::Pressed
            } else {
                ElementState::Released
            },
            key.repeated,
        ) {
            return true;
        }
        if !key.pressed || key.repeated || !activation(key.key, self.modifiers) {
            return false;
        }
        if self.reconnect_from_input() {
            self.terminal_reconnect.hold(physical);
            true
        } else {
            false
        }
    }

    pub(super) fn reconnect_from_ime(&mut self, text: &str) -> bool {
        !text.is_empty() && !text.chars().any(char::is_control) && self.reconnect_from_input()
    }

    fn reconnect_from_input(&mut self) -> bool {
        let Some(tab) = self.current_workspace_tab else {
            return false;
        };
        // The common connected path does not refresh every tab on each key.
        if !matches!(
            self.view_model.ssh_tab_states.get(&tab),
            Some(
                SshConnectionState::Disconnected
                    | SshConnectionState::Disconnecting
                    | SshConnectionState::Connecting
                    | SshConnectionState::Reconnecting
            )
        ) || !self.window_active
            || !self.terminal_keyboard_context_available()
            || self.view_model.layout_paused
            || self.egui_context.egui_wants_keyboard_input()
            || self.terminal_clipboard.menu.is_some()
            || self.active_ssh_profile().is_none()
        {
            return false;
        }
        self.refresh_ssh_controls();
        let state = self.view_model.ssh_tab_states.get(&tab).copied();
        if state == Some(SshConnectionState::Disconnected) && self.view_model.can_reconnect_ssh {
            self.terminal_ime.discard_for_reconnect();
            self.request_ssh_control(true);
            true
        } else if matches!(
            state,
            Some(
                SshConnectionState::Connecting
                    | SshConnectionState::Disconnecting
                    | SshConnectionState::Reconnecting
            )
        ) {
            // Do not queue further typing during a connection transition.
            self.terminal_ime.discard_for_reconnect();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]
mod tests;
