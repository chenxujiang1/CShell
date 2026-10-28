//! An acknowledged cancelled creation owns no runtime; remove only its matching tab.
use super::*;
impl DesktopApp {
    pub(super) fn finish_cancelled_launch(
        &mut self,
        view: &daemon_connection::DesktopDaemonView,
    ) -> bool {
        if !view.launch_cancelled || view.session_opening || view.session_id.is_some() {
            return false;
        }
        let Some(tab) = self.workspace.pending_launch else {
            return false;
        };
        if self.workspace.sessions.contains_key(&tab) {
            return false;
        }
        if self.current_workspace_tab != Some(tab)
            || !self.workspace.document.bindings.iter().any(|binding| {
                binding.tab_id == tab && binding.profile_id == view.session_profile_id
            })
        {
            return false;
        }
        self.remove_cancelled_connection_tab(tab);
        true
    }
    pub(super) fn dismiss_failed_connection(&mut self, profile: cshell_domain::ProfileId) {
        let Some(tab) = self.current_workspace_tab else {
            return;
        };
        let matches = self
            .workspace
            .document
            .bindings
            .iter()
            .any(|binding| binding.tab_id == tab && binding.profile_id == Some(profile))
            && self.daemon.as_ref().is_some_and(|daemon| {
                let view = daemon.view();
                view.session_profile_id == Some(profile)
                    && !view.session_opening
                    && view.session_id.is_none()
                    && view
                        .launch_error
                        .as_deref()
                        .is_some_and(|error| error.starts_with("HostKeyUnknown:"))
            });
        if matches {
            self.remove_cancelled_connection_tab(tab);
        }
    }
    fn remove_cancelled_connection_tab(&mut self, tab: cshell_domain::TabId) {
        // Do not remove/terminate an existing session in response to a stale prompt.
        if self.workspace.sessions.contains_key(&tab) {
            return;
        }
        self.workspace.remove(tab);
        self.pending_tab = None;
        self.pending_terminal_context = None;
        self.profile_panel.set_launch_error(None, None);
        self.view_model.input_warning = None;
        self.discard_closed_tab_view(tab);
        if let Some(next) = self.workspace.active() {
            self.resume_workspace_tab_runtime(next);
        }
        self.refresh_workspace_tabs();
        self.ensure_metadata_connection();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use daemon_connection::DesktopDaemonView;
    #[test]
    fn acknowledged_cancel_removes_only_its_pending_tab_and_repeated_polls_are_harmless() {
        let mut app = DesktopApp::default();
        let profile = cshell_domain::ProfileId::new();
        let previous = app.workspace.add(None, "keep".into()).unwrap();
        let pending = app.workspace.add(Some(profile), "cancel".into()).unwrap();
        app.current_workspace_tab = Some(pending);
        app.workspace.pending_launch = Some(pending);
        let view = DesktopDaemonView {
            launch_cancelled: true,
            session_profile_id: Some(profile),
            ..Default::default()
        };
        assert!(app.finish_cancelled_launch(&view));
        assert_eq!(app.workspace.active(), Some(previous));
        assert!(app.workspace.pending_launch.is_none());
        assert!(
            !app.workspace
                .document
                .bindings
                .iter()
                .any(|b| b.tab_id == pending)
        );
        assert!(!app.profile_panel.is_dialog_open());
        assert!(!app.finish_cancelled_launch(&view));
    }
    #[test]
    fn stale_or_uncertain_cancel_cannot_close_another_profile_or_live_session() {
        let mut app = DesktopApp::default();
        let profile = cshell_domain::ProfileId::new();
        let tab = app.workspace.add(Some(profile), "keep".into()).unwrap();
        app.current_workspace_tab = Some(tab);
        app.workspace.pending_launch = Some(tab);
        let mut view = DesktopDaemonView {
            launch_cancelled: true,
            session_profile_id: Some(cshell_domain::ProfileId::new()),
            ..Default::default()
        };
        assert!(!app.finish_cancelled_launch(&view));
        view.session_profile_id = Some(profile);
        view.session_opening = true;
        assert!(!app.finish_cancelled_launch(&view));
        view.session_opening = false;
        view.session_id = Some(SessionId::new());
        assert!(!app.finish_cancelled_launch(&view));
        view.session_id = None;
        app.workspace.sessions.insert(tab, SessionId::new());
        assert!(!app.finish_cancelled_launch(&view));
        assert_eq!(app.workspace.active(), Some(tab));
    }
    #[test]
    fn polling_cancelled_connection_closes_last_tab_without_showing_editor() {
        let mut app = DesktopApp::default();
        let profile = cshell_domain::ProfileId::new();
        let tab = app.workspace.add(Some(profile), "OTP".into()).unwrap();
        app.current_workspace_tab = Some(tab);
        app.workspace.pending_launch = Some(tab);
        app.daemon = Some(DesktopDaemonConnection::for_view_test(DesktopDaemonView {
            launch_cancelled: true,
            session_profile_id: Some(profile),
            ..Default::default()
        }));
        assert!(app.poll_primary_terminal(std::time::Instant::now()));
        assert!(app.workspace.document.bindings.is_empty());
        assert!(app.current_workspace_tab.is_none());
        assert!(app.daemon.is_none());
        assert!(!app.profile_panel.is_dialog_open());
    }
    #[test]
    fn dismissing_first_host_key_closes_failed_tab_but_never_a_settings_preview_or_live_tab() {
        let mut app = DesktopApp::default();
        let profile = cshell_domain::ProfileId::new();
        let tab = app
            .workspace
            .add(Some(profile), "unknown host".into())
            .unwrap();
        app.current_workspace_tab = Some(tab);
        app.daemon = Some(DesktopDaemonConnection::for_view_test(DesktopDaemonView {
            session_profile_id: Some(profile),
            launch_error: Some("HostKeyUnknown: fingerprint confirmation".into()),
            ..Default::default()
        }));
        app.dismiss_failed_connection(cshell_domain::ProfileId::new());
        assert_eq!(app.workspace.active(), Some(tab));
        let session = SessionId::new();
        app.workspace.sessions.insert(tab, session);
        app.dismiss_failed_connection(profile);
        assert_eq!(app.workspace.active(), Some(tab));
        app.workspace.sessions.remove(&tab);
        app.dismiss_failed_connection(profile);
        assert!(app.workspace.document.bindings.is_empty());
    }
}
