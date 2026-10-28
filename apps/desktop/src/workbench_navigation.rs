//! Workbench shortcuts are distinct from shell control keys.
use super::*;

pub(super) fn shortcut(
    state: ElementState,
    key: &Key,
    modifiers: ModifiersState,
    letter: &str,
) -> bool {
    state == ElementState::Pressed
        && modifiers.shift_key()
        && !modifiers.alt_key()
        && (modifiers.control_key() != modifiers.super_key())
        && matches!(key,Key::Character(text) if text.eq_ignore_ascii_case(letter))
}

impl DesktopApp {
    pub(super) fn tab_navigation_available(&self) -> bool {
        !self.command_center.blocks_input() && !self.workspace.visible_tabs().is_empty()
            && !self.view_model.ssh_control_busy
            && !self.view_model.layout_paused
            && !self.egui_context.egui_wants_keyboard_input()
            && !self.terminal_search_open
            && self.pending_paste.is_none()
            && !self.profile_panel.is_dialog_open()
            && !self.zmodem.blocks_input()
            && !self.keyboard_auth.blocks_input()
            && !self.tunnel_panel.open
            && !self.host_ca_panel.open
            && !self.host_panel.open
            && !self.task_panel.open
            && !self.terminal_tools_panel.open
            && !self.profile_batch_panel.open
            && !self.terminal_policy_panel.open
            && !self.workspace_recovery_panel.open
            && !self.ssh_provider_panel.open
            && !self.view_model.about_open
            && !self.view_model.confirm_terminate
    }
    pub(super) fn return_focus_from_tab_navigation(&mut self) {
        if std::mem::take(&mut self.view_model.tab_navigator.return_focus_requested) {
            self.egui_context.memory_mut(|memory| {
                if let Some(focused) = memory.focused() {
                    memory.surrender_focus(focused);
                }
            });
        }
    }
}
