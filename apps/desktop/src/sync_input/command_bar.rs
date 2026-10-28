//! Command-bar convenience uses the same frozen targets, audit and risk gates.
use super::*;

impl DesktopSyncInput {
    /// The target list remains visible before the operator clicks Enable.
    /// More than ten targets still use the existing additional arm review.
    pub(super) fn choose_targets(&mut self, window: WorkspaceWindowId, active: Option<TabId>) {
        let draft = std::mem::take(&mut self.compose);
        self.disarm(SyncInputDisarmReason::User);
        self.compose = draft;
        self.command_only = self.picker_command_only;
        let tabs: Vec<_> = self.selected.iter().copied().collect();
        match self
            .controller
            .review_arm(self.selection.unwrap_or(SyncInputSelection::Manual), &tabs)
        {
            Ok(review) if review.requires_confirmation() => {
                self.arm_review = Some(review);
                self.panel_open = false;
            }
            Ok(review) => self.enable_targets(review, window, active),
            Err(error) => self.status = error.to_string(),
        }
    }
    pub(super) fn enable_targets(
        &mut self,
        review: SyncInputArmReview,
        window: WorkspaceWindowId,
        active: Option<TabId>,
    ) {
        let source = if self.command_only {
            review.targets().first().map(|t| t.tab_id)
        } else {
            active.filter(|tab| review.targets().iter().any(|t| t.tab_id == *tab))
        };
        let Some(source) = source else {
            self.status = "实时键盘同步需先激活所选目标中的一个终端".into();
            return;
        };
        // Explicit is sufficient for <=10; the separate large-target review
        // invokes this only after its confirmation button was clicked.
        let approval = if review.requires_confirmation() {
            SyncInputApproval::Confirmed
        } else {
            SyncInputApproval::Explicit
        };
        match self.controller.arm(review, approval) {
            Ok(()) => {
                self.status.clear();
                self.panel_open = false;
                if self.command_only {
                    self.open_command_targets(window, active);
                } else {
                    self.close_compose();
                    self.return_focus_requested = true;
                }
                self.source = Some(source);
            }
            Err(error) => self.status = error.to_string(),
        }
    }
    pub(super) fn request_compose_send(&mut self, active: TabId, force_preview: bool) {
        if self.modal_blocked || self.panel_open || self.arm_review.is_some(){return;}
        if !self.compose_can_send(){self.status="正文为空或超过发送长度限制，请调整后再发送".into();return;}
        let source = if self.compose_uses_targets() && self.command_only {
            let Some(tab) = self
                .controller
                .armed_snapshot()
                .and_then(|s| s.targets().first())
                .map(|t| t.tab_id)
            else {
                self.status = "请先选择发送目标".into();
                return;
            };
            tab
        } else {
            active
        };
        self.prepare_compose_draft(source);
        if !force_preview {
            if self
                .pending_current
                .as_ref()
                .is_some_and(|p| !p.risk().requires_confirmation())
            {
                if let Some(prepared) = self.pending_current.take() {
                    if self.dispatch_current_with_approval(prepared, SyncInputApproval::Explicit) { self.finish_command_send(); }
                    else { self.compose.focus_requested = true; }
                }
            } else if self
                .pending
                .as_ref()
                .is_some_and(|p| !p.risk().requires_confirmation())
            {
                if let Some(prepared) = self.pending.take() {
                    if self.dispatch(prepared, SyncInputApproval::Explicit) { self.finish_command_send(); }
                    else { self.compose.focus_requested = true; }
                }
            }
        }
    }
}
