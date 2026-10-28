use super::*;

impl DesktopSyncInput {
    pub(super) fn ensure_command_bar(&mut self, window: WorkspaceWindowId, active: Option<TabId>) {
        if active.is_some()
            && self.compose.owner.is_none()
            && !self.hidden_command_bars.contains(&window)
            && !self.is_realtime()
            && !self.modal_blocked
            && !self.panel_open
            && self.arm_review.is_none()
        {
            self.open_compose(window, active);
            // Being visible must not steal typing from the terminal.
            self.compose.focus_requested = false;
        }
    }
    pub(crate) fn hide_command_bar(&mut self) {
        if let Some(window) = self.compose.owner {
            self.hidden_command_bars.insert(window);
        }
        self.close_compose();
    }
    pub(crate) fn focus_terminal(&mut self) -> bool {
        if self.modal_blocked
            || self.panel_open
            || self.arm_review.is_some()
            || self.pending.is_some()
            || self.pending_current.is_some()
            || !self.ordinary_command_draft()
        {
            return false;
        }
        self.compose.focus_requested = false;
        self.compose.focus_active = false;
        true
    }
    pub(crate) fn set_modal_blocked(&mut self, blocked: bool) {
        if blocked && !self.modal_blocked {
            if self.is_realtime() {
                self.disarm(SyncInputDisarmReason::SecureInput);
            } else {
                self.controller.port().permit.revoke();
                self.controller.port_mut().permit = DesktopInputPermit::with_audit_health(
                    Arc::clone(&self.controller.audit().healthy),
                );
                self.pending = None;
                self.pending_current = None;
                self.arm_review = None;
                self.panel_open = false;
                self.compose.risk_ack = false;
                self.compose.focus_active = false;
                self.compose.focus_requested = false;
            }
        }
        self.modal_blocked = blocked;
    }
}
