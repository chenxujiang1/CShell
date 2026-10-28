//! Each host owns an IPC worker and remote panel state. Only local file data/UI is shared.
use super::*;
use crate::sftp_connection::{ConnectionPhase, SftpClientCommand, SftpView};
use crate::sftp_panel::{RemoteTab, TabAction};
use cshell_domain::ProfileId;
const MAX_TABS: usize = 32;
#[derive(Default)]
pub(crate) struct SftpTabs {
    closing: bool,
    entries: Vec<HostTab>,
    pub(super) active: Option<u64>,
    next: u64,
    pub(super) local: CommonLocal,
    last_stamp: Vec<(u64, u64)>,
}
struct HostTab {
    id: u64,
    profile: ProfileId,
    title: String,
    host: String,
    parked: Option<Content>,
}
struct Content {
    connection: Option<DesktopSftpConnection>,
    panel: SftpPanel,
    session: Option<SessionId>,
}
#[derive(Default)]
pub(super) struct CommonLocal {
    connection: Option<DesktopSftpConnection>,
    session: Option<SessionId>,
    generation: Option<u64>,
}
impl CommonLocal {
    fn start(
        &mut self,
        config: crate::daemon_connection::DesktopConnectionConfig,
    ) -> Result<(), std::io::Error> {
        if self.connection.is_some() {
            return Ok(());
        }
        let connection = DesktopSftpConnection::start(config)?;
        let session = SessionId::new();
        if !connection.request(SftpClientCommand::LocalList(
            session,
            crate::sftp_settings::local_directory(),
        )) {
            return Err(std::io::Error::other("本地目录请求未提交"));
        }
        self.connection = Some(connection);
        self.session = Some(session);
        Ok(())
    }
    pub(super) fn submit(&self, command: &SftpClientCommand) -> Option<bool> {
        let local = matches!(command,SftpClientCommand::BrowseManaged(_,o) if o.local)
            || matches!(
                command,
                SftpClientCommand::BrowseLocal(..)
                    | SftpClientCommand::LocalList(..)
                    | SftpClientCommand::BrowseNext(_, true, _)
                    | SftpClientCommand::BrowseClose(_, true, _)
            );
        if !local {
            return None;
        }
        let (Some(connection), Some(session)) = (&self.connection, self.session) else {
            return Some(false);
        };
        let query = match command {
            SftpClientCommand::BrowseManaged(_, options)
            | SftpClientCommand::BrowseLocal(_, options) => {
                SftpClientCommand::BrowseLocal(session, options.clone())
            }
            SftpClientCommand::LocalList(_, path) => {
                SftpClientCommand::LocalList(session, path.clone())
            }
            SftpClientCommand::BrowseNext(_, true, cursor) => {
                SftpClientCommand::BrowseNext(session, true, cursor.clone())
            }
            SftpClientCommand::BrowseClose(_, true, cursor) => {
                SftpClientCommand::BrowseClose(session, true, cursor.clone())
            }
            _ => unreachable!(),
        };
        Some(connection.request(query))
    }
    pub(super) fn display(&self, mut view: SftpView) -> SftpView {
        if let Some(connection) = &self.connection {
            let local = connection.view();
            view.local_directory = local.local_directory;
            view.local_entries = local.local_entries;
            view.local_truncated = local.local_truncated;
            view.local_cursor = local.local_cursor;
            if let Some(error) = local.error {
                view.error = Some(match view.error {
                    Some(remote) => format!("{remote}；本地目录：{error}"),
                    None => format!("本地目录：{error}"),
                });
            }
        }
        view
    }
    fn poll(&mut self) -> bool {
        let generation = self
            .connection
            .as_ref()
            .map(DesktopSftpConnection::generation);
        if generation != self.generation {
            self.generation = generation;
            true
        } else {
            false
        }
    }
    fn close(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.retire();
        }
        self.session = None;
        self.generation = None;
    }
}
impl Drop for CommonLocal {
    fn drop(&mut self) {
        self.close();
    }
}
impl Drop for SftpTabs {
    fn drop(&mut self) {
        for tab in &mut self.entries {
            if let Some(content) = &mut tab.parked {
                if let Some(connection) = content.connection.take() {
                    connection.retire();
                }
            }
        }
    }
}
impl DesktopApp {
    pub(crate) fn sftp_ssh_source(&self,profile:ProfileId)->Option<SessionId> {
        let tab=self.current_workspace_tab?;
        if !self.workspace.document.bindings.iter().any(|b|b.tab_id==tab && b.profile_id==Some(profile)) {return None;}
        let session=*self.workspace.sessions.get(&tab)?;
        self.view_model.reusable_ssh_sessions.contains(&session).then_some(session)
    }
    pub(super) fn open_sftp_tab(&mut self, profile: ProfileId) {
        if let Some(id) = self
            .sftp_tabs
            .entries
            .iter()
            .find(|t| t.profile == profile)
            .map(|t| t.id)
        {
            self.select_sftp_tab(id);
            if let Some(view) = self.sftp.as_ref().map(DesktopSftpConnection::view) {
                if !view.connection_phase.busy()
                    && view.connection_phase != ConnectionPhase::Connected
                {
                    self.submit_sftp_command(sftp_connection::lifecycle::connect(
                        profile,
                        view.connection_previous.or(view.session_id),
                    ));
                }
            }
            self.focus_sftp();
            return;
        }
        if self.sftp_tabs.entries.len() >= MAX_TABS {
            self.view_model.input_warning =
                Some("SFTP 标签已达到 32 个，请先关闭不需要的标签。".into());
            return;
        }
        let connection = if self.sftp_tabs.active.is_none() {
            self.sftp.take()
        } else {
            None
        };
        let connection = match connection {
            Some(connection) => connection,
            None => {
                let Some(config) = self
                    .daemon_config
                    .as_ref()
                    .or(self.workspace_config.as_ref())
                    .cloned()
                else {
                    self.view_model.input_warning = Some("SFTP 后台服务尚未连接。".into());
                    return;
                };
                match DesktopSftpConnection::start(config) {
                    Ok(c) => c,
                    Err(e) => {
                        self.view_model.input_warning = Some(format!("无法创建 SFTP 标签：{e}"));
                        return;
                    }
                }
            }
        };
        if self.sftp_tabs.local.connection.is_none() {
            let Some(config) = self
                .daemon_config
                .as_ref()
                .or(self.workspace_config.as_ref())
                .cloned()
            else {
                self.view_model.input_warning = Some("本地文件服务尚未连接。".into());
                return;
            };
            if let Err(error) = self.sftp_tabs.local.start(config) {
                self.view_model.input_warning = Some(format!("无法打开本地目录：{error}"));
                return;
            }
        }
        connection.share_local_browser();
        let command = sftp_connection::lifecycle::connect_shared(profile, None,self.sftp_ssh_source(profile));
        let session = if let SftpClientCommand::ConnectProfile { session, .. } = &command {
            *session
        } else {
            unreachable!()
        };
        if !connection.request(command) {
            connection.retire();
            self.view_model.input_warning = Some("SFTP 连接请求未提交，请稍后重试。".into());
            return;
        }
        let (title, host) = self
            .profiles
            .as_ref()
            .and_then(|p| p.view().catalog)
            .map(|c| {
                let title = c
                    .profiles
                    .iter()
                    .find(|p| p.id == profile)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "服务器".into());
                let host = c
                    .ssh_connections
                    .iter()
                    .find(|t| t.profile_id == profile)
                    .map(|t| format!("{}@{}:{}", t.username, t.host, t.port))
                    .unwrap_or_default();
                (title, host)
            })
            .unwrap_or_else(|| ("服务器".into(), String::new()));
        self.install_sftp_tab(profile, title, host, connection, session);
        self.focus_sftp();
    }
    fn install_sftp_tab(
        &mut self,
        profile: ProfileId,
        title: String,
        host: String,
        connection: DesktopSftpConnection,
        session: SessionId,
    ) {
        let mut panel = SftpPanel::default();
        self.sftp_panel.carry_local_to(&mut panel);
        if let Some(active) = self.sftp_tabs.active {
            if let Some(tab) = self.sftp_tabs.entries.iter_mut().find(|t| t.id == active) {
                tab.parked = Some(Content {
                    connection: self.sftp.take(),
                    panel: std::mem::replace(&mut self.sftp_panel, panel),
                    session: self.sftp_session,
                });
            }
        } else {
            self.sftp_panel = panel;
        }
        self.sftp_tabs.next += 1;
        let id = self.sftp_tabs.next;
        self.sftp_tabs.entries.push(HostTab {
            id,
            profile,
            title,
            host,
            parked: None,
        });
        self.sftp_tabs.active = Some(id);
        self.sftp = Some(connection);
        self.sftp_session = Some(session);
        self.sftp_panel.open = true;
        self.last_sftp_generation = None;
        self.sync_sftp_tabs();
    }
    fn focus_sftp(&mut self) {
        self.sftp_panel.open = true;
        if let Some(window) = &self.sftp_window {
            window.window.focus_window();
            window.window.request_redraw();
        }
    }
    fn select_sftp_tab(&mut self, id: u64) {
        if self.sftp_tabs.active == Some(id) {
            return;
        }
        let Some(index) = self.sftp_tabs.entries.iter().position(|t| t.id == id) else {
            return;
        };
        let Some(mut target) = self.sftp_tabs.entries[index].parked.take() else {
            return;
        };
        self.sftp_panel.carry_local_to(&mut target.panel);
        let previous = Content {
            connection: self.sftp.take(),
            panel: std::mem::replace(&mut self.sftp_panel, target.panel),
            session: self.sftp_session,
        };
        if let Some(active) = self.sftp_tabs.active {
            if let Some(tab) = self.sftp_tabs.entries.iter_mut().find(|t| t.id == active) {
                tab.parked = Some(previous);
            }
        }
        self.sftp = target.connection;
        self.sftp_session = target.session;
        self.sftp_tabs.active = Some(id);
        self.sftp_panel.open = true;
        self.last_sftp_generation = None;
        self.sync_sftp_tabs();
        if self.sftp_tabs.active==Some(id) {self.focus_sftp();}
    }
    fn close_sftp_tab(&mut self, id: u64) {
        let Some(index) = self.sftp_tabs.entries.iter().position(|t| t.id == id) else {
            return;
        };
        let was_closing=std::mem::replace(&mut self.sftp_tabs.closing,true);
        if self.sftp_tabs.active == Some(id) {
            if self.sftp_tabs.entries.len() > 1 {
                let next = if index + 1 < self.sftp_tabs.entries.len() {
                    index + 1
                } else {
                    index - 1
                };
                self.select_sftp_tab(self.sftp_tabs.entries[next].id);
            } else {
                if let Some(connection) = self.sftp.take() {
                    if let Some(session) = self.sftp_session {
                        let _ = connection.request(SftpClientCommand::Disconnect(session));
                    }
                    connection.retire();
                }
                self.sftp_session = None;
                self.sftp_tabs.active = None;
                self.sftp_panel.open = false;
                self.sftp_panel.remote_tabs.clear();
                self.sftp_window = None;
                self.sftp_tabs.local.close();
            }
        }
        let mut removed = self.sftp_tabs.entries.remove(index);
        if let Some(mut content) = removed.parked.take() {
            if let Some(connection) = content.connection.take() {
                if let Some(session) = connection.view().session_id {
                    let _ = connection.request(SftpClientCommand::Disconnect(session));
                }
                connection.retire();
            }
        }
        self.last_sftp_generation = None;
        self.sync_sftp_tabs();
        self.sftp_tabs.closing=was_closing;
    }
    pub(super) fn close_all_sftp_tabs(&mut self) {
        if let Some(connection) = self.sftp.take() {
            if let Some(session) = connection.view().session_id {
                let _ = connection.request(SftpClientCommand::Disconnect(session));
            }
            connection.retire();
        }
        for tab in &mut self.sftp_tabs.entries {
            if let Some(mut content) = tab.parked.take() {
                if let Some(connection) = content.connection.take() {
                    if let Some(session) = connection.view().session_id {
                        let _ = connection.request(SftpClientCommand::Disconnect(session));
                    }
                    connection.retire();
                }
            }
        }
        self.sftp_tabs.entries.clear();
        self.sftp_tabs.active = None;
        self.sftp_session = None;
        self.sftp_panel.remote_tabs.clear();
        self.sftp_tabs.local.close();
    }
    pub(super) fn apply_sftp_tab_action(&mut self, action: TabAction) {
        match action {
            TabAction::Select(id) => self.select_sftp_tab(id),
            TabAction::Close(id) => self.close_sftp_tab(id),
            TabAction::Open(profile) => self.open_sftp_tab(profile),
        }
    }
    pub(super) fn sync_sftp_tabs(&mut self) {
        let cancelled:Vec<_>=if self.sftp_tabs.closing {Vec::new()}else{self.sftp_tabs.entries.iter().filter_map(|tab|{
            let connection=if self.sftp_tabs.active==Some(tab.id){self.sftp.as_ref()}else{tab.parked.as_ref().and_then(|content|content.connection.as_ref())};
            connection.filter(|connection|connection.take_cancelled_connection()).map(|_|tab.id)
        }).collect()};
        if !cancelled.is_empty() {
            for id in cancelled {self.close_sftp_tab(id);}
            return;
        }
        if self.sftp_tabs.entries.is_empty() {self.sftp_panel.remote_tabs.clear();return;}
        let active_view = self.sftp.as_ref().map(DesktopSftpConnection::view);
        if let Some(view) = &active_view {
            if view.connection_profile.is_some() {
                self.sftp_session = view.session_id;
            }
            let profiles = self.profiles.as_ref().map(DesktopProfileConnection::view);
            self.sftp_panel.sync_connection(view, profiles.as_ref());
        }
        if self.sftp_tabs.local.poll() {
            self.last_sftp_generation = None;
        }
        let mut summaries = Vec::new();
        let mut stamp = Vec::new();
        for tab in &self.sftp_tabs.entries {
            let connection = if self.sftp_tabs.active == Some(tab.id) {
                self.sftp.as_ref()
            } else {
                tab.parked.as_ref().and_then(|c| c.connection.as_ref())
            };
            if let Some(connection) = connection {
                let (generation, phase, _, host, running) = connection.tab_status();
                stamp.push((tab.id, generation));
                summaries.push(RemoteTab {
                    id: tab.id,
                    title: tab.title.clone(),
                    host: host.unwrap_or_else(|| tab.host.clone()),
                    phase,
                    active: self.sftp_tabs.active == Some(tab.id),
                    running,
                });
            }
        }
        if stamp != self.sftp_tabs.last_stamp {
            self.sftp_tabs.last_stamp = stamp;
            self.last_sftp_generation = None;
        }
        self.sftp_panel.remote_tabs = summaries;
        if let Some(catalog) = self.profiles.as_ref().and_then(|p| p.view().catalog) {
            self.sftp_panel.verification_overrides=catalog.ssh_connections.iter().filter_map(|target|target.options.sftp_verification.map(|mode|(target.profile_id,mode))).collect();
            self.sftp_panel.available_hosts = catalog
                .ssh_connections
                .iter()
                .map(|target| {
                    (
                        target.profile_id,
                        catalog
                            .profiles
                            .iter()
                            .find(|p| p.id == target.profile_id)
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| target.host.clone()),
                        format!("{}@{}:{}", target.username, target.host, target.port),
                    )
                })
                .collect();
        }
    }
}
#[cfg(test)]
mod tests;
