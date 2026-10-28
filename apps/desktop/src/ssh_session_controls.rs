//! Explicit SSH controls close the old runtime before reusing its saved tab.
mod channels;
use super::*;
use cshell_domain::{ProfileId, ProfileKind, TabId};
use cshell_ui::SshConnectionState;
use daemon_connection::DesktopDaemonView;

pub(super) fn configured_target(connection: &cshell_domain::SshConnectionRecord) -> String {
    let host = if connection.host.contains(':') && !connection.host.starts_with('[') {
        format!("[{}]", connection.host)
    } else { connection.host.clone() };
    if connection.username.is_empty() {format!("{host}:{}", connection.port)}
    else {format!("{}@{host}:{}", connection.username, connection.port)}
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PendingSshControl {
    tab: TabId,
    profile: ProfileId,
    session: SessionId,
    reconnect: bool,
}

impl DesktopApp {
    pub(super) fn active_ssh_profile(&self) -> Option<(TabId, ProfileId)> {
        let tab = self.current_workspace_tab?;
        if self.workspace.window_of_tab(tab) != Some(self.workspace.window_id) {
            return None;
        }
        let profile = self
            .workspace
            .document
            .bindings
            .iter()
            .find(|b| b.tab_id == tab)?
            .profile_id?;
        self.view_model
            .saved_ssh_profiles
            .iter()
            .any(|p| p.id == profile && p.kind == ProfileKind::Ssh)
            .then_some((tab, profile))
    }

    pub(super) fn refresh_ssh_controls(&mut self) {
        let profile = self.active_ssh_profile();
        let view = self.daemon.as_ref().map(DesktopDaemonConnection::view);
        let busy = self.ssh_control.is_some()
            || self.workspace.pending_launch.is_some()
            || self.closing_workspace_tab.is_some()
            || self.view_model.terminate_pending
            || view
                .as_ref()
                .is_some_and(|v| v.close_pending.is_some() || v.session_opening);
        let live = profile
            .and_then(|(tab, _)| self.workspace.sessions.get(&tab).copied())
            .is_some_and(|id| {
                view.as_ref()
                    .is_some_and(|v| v.session_id == Some(id) && v.session_running)
            });
        let writable = self.workspace.writable
            && !self
                .workspace_connection
                .as_ref()
                .is_some_and(|c| c.view().paused);
        self.view_model.ssh_control_busy = busy;
        self.view_model.can_disconnect_ssh = profile.is_some() && (live || view.as_ref().is_some_and(|v| v.session_reconnecting)) && !busy;
        self.view_model.can_restart_ssh = profile.is_some() && writable && !busy;
        self.view_model.can_reconnect_ssh = self.view_model.can_restart_ssh && !live;
        let mut states = self
            .ssh_tab_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(summaries) = view.as_ref().and_then(|v| v.sessions.as_ref()) {
            for (tab, id) in &self.workspace.sessions {
                if let Some(summary) = summaries.iter().find(|s| s.session_id == id.as_uuid().as_bytes())
                    && self.workspace.document.bindings.iter().any(|b| b.tab_id == *tab && b.profile_id.is_some_and(|p| self.view_model.saved_ssh_profiles.iter().any(|saved| saved.id == p && saved.kind == ProfileKind::Ssh)))
                {
                    states.insert(*tab, if summary.reconnecting { SshConnectionState::Reconnecting } else if summary.running { SshConnectionState::Connected } else { SshConnectionState::Disconnected });
                }
            }
        }
        if let Some((tab, _)) = profile {
            let previous = states.get(&tab).copied();
            let state = if let Some(control) = self.ssh_control.filter(|c| c.tab == tab) {
                Some(if control.reconnect {
                    SshConnectionState::Reconnecting
                } else {
                    SshConnectionState::Disconnecting
                })
            } else if self.workspace.pending_launch == Some(tab) {
                Some(
                    if matches!(
                        previous,
                        Some(SshConnectionState::Disconnected | SshConnectionState::Reconnecting)
                    ) {
                        SshConnectionState::Reconnecting
                    } else {
                        SshConnectionState::Connecting
                    },
                )
            } else if view.as_ref().is_some_and(|v| v.close_error.is_some()) {
                Some(SshConnectionState::Unconfirmed)
            } else if view.as_ref().is_some_and(|v| v.session_reconnecting) {
                Some(SshConnectionState::Reconnecting)
            } else if live && view.as_ref().is_some_and(|v| v.connected) {
                Some(SshConnectionState::Connected)
            } else if previous == Some(SshConnectionState::Disconnected)
                && (!self.workspace.sessions.contains_key(&tab) || view.as_ref().is_some_and(|v| !v.session_running && !v.session_reconnecting && v.sessions.is_some()))
            {
                previous
            } else if previous.is_some() {
                Some(SshConnectionState::Unconfirmed)
            } else {
                None
            };
            if let Some(state) = state {
                states.insert(tab, state);
            }
        }
        self.view_model.ssh_tab_states.clone_from(&states);
    }

    pub(super) fn request_ssh_control(&mut self, reconnect: bool) {
        self.refresh_ssh_controls();
        if if reconnect {
            !self.view_model.can_restart_ssh
        } else {
            !self.view_model.can_disconnect_ssh
        } {
            return;
        }
        let Some((tab, profile)) = self.active_ssh_profile() else {
            return;
        };
        self.pending_paste = None;
        self.pending_terminal_context = None;
        if let Some(sync) = &self.sync_input {
            sync.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .disarm(cshell_application::SyncInputDisarmReason::User);
        }
        self.ensure_metadata_connection();
        if let Some(session) = self.workspace.sessions.get(&tab).copied() {
            if self
                .daemon
                .as_ref()
                .is_some_and(|d| d.terminate_observed_session(session))
            {
                self.ssh_control = Some(PendingSshControl {
                    tab,
                    profile,
                    session,
                    reconnect,
                });
                // Freeze tab moves/switches and all other launches until close is acknowledged.
                self.workspace.pending_launch = Some(tab);
                self.view_model.input_warning = None;
                self.view_model.daemon_status_detail = if reconnect {
                    "正在重新连接：关闭旧 SSH 连接…"
                } else {
                    "正在断开 SSH 连接…"
                }
                .into();
            } else {
                self.view_model.input_warning =
                    Some("无法提交 SSH 断开请求，请等待当前操作完成后重试。".into());
            }
        } else if reconnect {
            self.launch_ssh_in_existing_tab(tab, profile);
        }
        self.refresh_ssh_controls();
    }

    fn launch_ssh_in_existing_tab(&mut self, tab: TabId, profile: ProfileId) {
        if self.current_workspace_tab != Some(tab)
            || !self
                .workspace
                .document
                .bindings
                .iter()
                .any(|b| b.tab_id == tab && b.profile_id == Some(profile))
        {
            self.view_model.input_warning = Some("当前标签已变化，未重新连接 SSH。".into());
            return;
        }
        if self
            .daemon
            .as_ref()
            .is_some_and(|d| d.open_profile(profile))
        {
            self.workspace.pending_launch = Some(tab);
            self.pending_tab = None;
            self.terminal_surface = Default::default();
            self.terminal_decorations = Default::default();
            self.log_surface = None;
            self.submitted_log_page = None;
            self.terminal_search_requested_generation = None;
            self.view_model.input_warning = None;
            self.view_model.daemon_status_detail = "正在连接 SSH…".into();
        } else {
            self.workspace.pending_launch = None;
            self.view_model.input_warning = Some("无法提交 SSH 连接请求，请稍后重试。".into());
        }
    }

    pub(super) fn poll_ssh_control(&mut self, view: &DesktopDaemonView) {
        let Some(control) = self.ssh_control else {
            return;
        };
        if view.closed_session == Some(control.session) && view.close_pending.is_none() {
            self.ssh_control = None;
            self.workspace.pending_launch = None;
            if self.workspace.sessions.get(&control.tab) != Some(&control.session) {
                self.view_model.input_warning = Some("标签绑定已变化，SSH 重连已停止。".into());
                return;
            }
            self.workspace.sessions.remove(&control.tab);
            self.ssh_tab_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(control.tab, SshConnectionState::Disconnected);
            self.pending_tab = None;
            if let Some(connection) = self.pane_daemons.remove(&control.tab) {
                connection.retire();
            }
            if let Some(connection) = self.sync_daemons.remove(&control.tab) {
                connection.retire();
            }
            self.sync_opened_sessions(view);
            self.view_model.selected = None;
            self.view_model.daemon_connected = false;
            self.view_model.can_sftp = false;
            self.cursor_blink
                .synchronize(None, false, std::time::Instant::now());
            self.view_model.daemon_status_detail = "SSH 已断开，可点击重新连接。".into();
            // Preserve the binding, tab ID, terminal snapshot and scrollback.
            self.refresh_workspace_tabs();
            if control.reconnect {
                self.launch_ssh_in_existing_tab(control.tab, control.profile);
            } else if let Some(daemon) = &self.daemon {
                daemon.detach_for_placeholder();
            }
        } else if view.close_pending.is_none()
            && let Some(error) = &view.close_error
        {
            self.ssh_control = None;
            self.workspace.pending_launch = None;
            self.view_model.input_warning =
                Some(format!("SSH 断开未能确认，未创建新连接：{error}"));
            self.ssh_tab_states
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(control.tab, SshConnectionState::Unconfirmed);
            // An uncertain close must never trigger a replacement session.
            if self.current_workspace_tab == Some(control.tab)
                && let Some(daemon) = &self.daemon
            {
                daemon.attach_session(control.session);
            }
        }
        self.refresh_ssh_controls();
    }
}

#[cfg(test)]
mod tests;
