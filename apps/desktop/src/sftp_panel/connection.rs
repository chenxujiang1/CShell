use super::*;
impl SftpPanel {
    pub(crate) fn sync_connection(
        &mut self,
        view: &SftpView,
        profiles: Option<&crate::profile_connection::DesktopProfileView>,
    ) {
        if self.prompt_attempt != view.session_id {
            self.host_prompt = None;
        }
        if let Some(profiles) = profiles {
            if view
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("HostKeyUnknown:"))
                && self.prompt_attempt != view.session_id
            {
                self.prompt_attempt = view.session_id;
                if let Some((target, revision)) = profiles.catalog.as_ref().and_then(|c| {
                    c.ssh_connections
                        .iter()
                        .find(|t| Some(t.profile_id) == view.connection_profile)
                        .map(|t| (t.clone(), c.revision))
                }) {
                    self.host_prompt = Some(crate::host_key_prompt::HostKeyPrompt::new(
                        target, revision, true,
                    ));
                }
            }
            if let Some(prompt) = &mut self.host_prompt {
                prompt.sync(profiles);
            }
        }
    }
    pub(crate) fn take_profile_command(
        &mut self,
    ) -> Option<crate::profile_connection::ProfileClientCommand> {
        self.profile_command.take()
    }
    pub(crate) fn profile_request_failed(&mut self) {
        if let Some(prompt) = &mut self.host_prompt {
            prompt.fail("请求尚未提交，请稍后重试。");
        }
    }
    pub(super) fn draw_host_prompt(
        &mut self,
        context: &egui::Context,
        view: Option<&SftpView>,
        command: &mut Option<SftpClientCommand>,
    ) {
        if let Some(prompt) = &mut self.host_prompt {
            match prompt.draw(context) {
                Some(crate::host_key_prompt::Action::Request(request)) => {
                    self.profile_command = Some(request)
                }
                Some(crate::host_key_prompt::Action::Connect(profile)) => {
                    self.host_prompt = None;
                    *command = Some(crate::sftp_connection::lifecycle::connect(
                        profile,
                        view.and_then(|v| v.connection_previous.or(v.session_id)),
                    ));
                }
                Some(crate::host_key_prompt::Action::Dismiss) => {
                    if prompt.cancelled_connection_profile().is_some_and(|profile|Some(profile)==view.and_then(|view|view.connection_profile))
                        && self.bound_session_id.is_some() && self.bound_session_id==view.and_then(|view|view.session_id) {
                        if let Some(tab)=self.remote_tabs.iter().find(|tab|tab.active) {self.tab_action=Some(super::TabAction::Close(tab.id));}
                    }
                    self.host_prompt = None;
                },
                None => {}
            }
        }
    }
}
