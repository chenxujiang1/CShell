use super::*;
impl DesktopApp {
    pub(crate) fn clone_terminal_tab(&mut self, source: TabId, reuse: bool) {
        if !self.workspace_loaded
            || !self.workspace.writable
            || self.workspace.pending_launch.is_some()
        {
            return;
        }
        let source_session = self.workspace.sessions.get(&source).copied();
        if reuse
            && !source_session.is_some_and(|id| self.view_model.reusable_ssh_sessions.contains(&id))
        {
            self.view_model.input_warning = Some("原 SSH 渠道不可用，请先连接 SSH".into());
            return;
        }
        let Some(tab) = self.workspace.clone_session_tab(source) else {
            return;
        };
        let profile = self
            .workspace
            .document
            .bindings
            .iter()
            .find(|b| b.tab_id == tab)
            .and_then(|b| b.profile_id);
        self.switch_tab_view(tab);
        self.view_model.selected = None;
        self.pending_tab = None;
        self.workspace.pending_launch = Some(tab);
        let accepted = self.daemon.as_ref().is_some_and(|daemon| {
            if reuse {
                source_session.is_some_and(|source| daemon.duplicate_ssh_channel(source, profile))
            } else {
                profile.map_or_else(|| daemon.new_default_shell(), |id| daemon.open_profile(id))
            }
        });
        if !accepted {
            self.workspace.pending_launch = None;
            self.view_model.workspace_status = "创建请求未提交，请稍后重试".into();
        }
        self.refresh_workspace_tabs();
    }
}
