mod command_workspace;
mod app_icon;
mod shell_history_panel;
mod clipboard_connection;
mod clipboard_policy;
mod credential_package;
mod managed_keys_panel;
mod cursor_blink;
mod daemon_connection;
mod daemon_input;
mod diagnostics;
mod interaction_regressions;
mod display_settings;
mod terminal_clipboard_settings;
mod terminal_clipboard;
mod terminal_mouse;
mod command_center;
mod workbench_settings;
mod terminal_appearance;
mod download_connection;
mod download_panel;
mod file_task_transport;
mod host_ca_connection;
mod host_ca_panel;
mod host_group_connection;
mod host_group_panel;
mod host_key_prompt;
mod keyboard_auth;
mod connection_cancel;
#[cfg(test)]
mod interactive_workbench_gate;
mod mixed_connection;
mod mixed_panel;
mod mixed_results_panel;
mod mixed_retry_panel;
mod output_analysis_connection;
mod output_analysis_panel;
mod output_archive_connection;
mod output_archive_panel;
mod output_merge_connection;
mod output_merge_panel;
mod output_search_connection;
mod output_search_panel;
mod output_workspace_connection;
mod output_workspace_panel;
mod profile_batch_connection;
mod profile_batch_panel;
mod profile_connection;
mod profile_export;
mod profile_panel;
mod quick_command_connection;
mod replay_worker;
mod script_authoring_panel;
mod script_live_panel;
mod secret_edit;
mod sftp_connection;
mod sftp_panel;
mod sftp_window;
mod zmodem_connection;
mod zmodem_panel;
mod zmodem_settings;
mod terminal_upload;
mod terminal_scrollback;
mod terminal_reading;
mod ssh_session_controls;
#[cfg(test)]
mod terminal_keyboard_tests;
mod sftp_settings;
mod ssh_provider_panel;
mod sync_input;
mod tab_activity;
mod task_connection;
mod task_definition_editor;
mod task_panel;
mod task_results_panel;
mod terminal_accessibility;
mod terminal_policy_connection;
mod terminal_policy_panel;
mod terminal_search;
mod terminal_tools_connection;
mod terminal_tools_panel;
mod tunnel_connection;
mod tunnel_panel;
mod ui_fonts;
#[cfg(test)]
mod ui_layout_gate;
mod upload_connection;
mod upload_panel;
mod vault_panel;
mod visual_corpus;
mod window_e2e;
mod workspace_connection;
mod workspace_model;
mod workbench_navigation;
mod workspace_recovery_panel;

use cshell_domain::{InputAction, KeyCode, KeyEvent, Modifiers, SessionId};
use cshell_ipc::{HistorySearchDirection, MAX_TERMINAL_INPUT_BYTES};
use cshell_render::{
    EguiFrame, LogDecorations, LogReflowLayout, LogReflowRequest, LogScrollbarState,
    LogSearchMatch, LogSourceId, LogSurfaceError, LogSurfaceModel, MAX_TERMINAL_SEARCH_QUERY_BYTES,
    RenderOutcome, TerminalCellPoint, TerminalDecorations, TerminalSelection,
    TerminalSelectionMode, TerminalSurfaceModel, TerminalTheme, TerminalViewport, WindowRenderer,
    WindowRendererError, validate_terminal_search_query,
};
use cshell_ui::{WorkbenchMenuCommand, WorkbenchViewModel};
use cursor_blink::CursorBlinkState;
use daemon_connection::{
    DesktopConnectionConfig, DesktopDaemonConnection, DesktopHistorySearchRequest,
    DesktopHistorySearchResponse,
};
use profile_connection::DesktopProfileConnection;
use profile_panel::ProfilePanel;
use sftp_connection::DesktopSftpConnection;
use sftp_panel::SftpPanel;
use std::error::Error;
use std::sync::Arc;
use terminal_search::{TerminalSearchRequest, TerminalSearchWorker};
use tracing::info;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

enum DesktopEvent {
    AccessKit(egui_winit::accesskit_winit::Event),
}

impl From<egui_winit::accesskit_winit::Event> for DesktopEvent {
    fn from(event: egui_winit::accesskit_winit::Event) -> Self {
        Self::AccessKit(event)
    }
}

const SESSION_LOG_ARGUMENT: &str = "--session-log";
const TERMINAL_MULTI_CLICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Default)]
struct TerminalClickState {
    last: Option<(std::time::Instant, TerminalCellPoint)>,
    count: u8,
}

#[derive(Default)]
struct DesktopApp {
    // Outer None means no saved transition; inner None means terminal focus.
    focus_before_deactivate: Option<Option<egui::Id>>,
    terminal_ui_regions: Vec<egui::Rect>,
    keyboard_auth: keyboard_auth::Dialog,
    zmodem: zmodem_panel::Panel,
    terminal_upload: terminal_upload::Panel,
    accesskit_proxy: Option<EventLoopProxy<DesktopEvent>>,
    window: Option<Arc<Window>>,
    renderer: Option<WindowRenderer>,
    display_settings_path: Option<std::path::PathBuf>,
    saved_font_pixels: Option<u16>,
    workbench_settings_path: Option<std::path::PathBuf>,
    saved_workbench_layout: Option<cshell_ui::workbench_layout::Layout>,
    egui_context: egui::Context,
    egui_state: Option<egui_winit::State>,
    egui_repaint_deadline: Option<std::time::Instant>,
    last_renderer_attempt: Option<std::time::Instant>,
    renderer_recovering: bool,
    renderer_needs_font_upload: bool,
    view_model: WorkbenchViewModel,
    daemon: Option<DesktopDaemonConnection>,
    profiles: Option<DesktopProfileConnection>,
    profile_panel: ProfilePanel,
    host_groups: Option<Arc<host_group_connection::HostGroupConnection>>,
    host_panel: host_group_panel::HostGroupPanel,
    last_host_stamp: Option<(u64, bool)>,
    tasks: Option<Arc<task_connection::TaskConnection>>,
    task_panel: task_panel::TaskPanel,
    last_task_stamp: Option<(u64, bool)>,
    sftp: Option<DesktopSftpConnection>,
    sftp_tabs:sftp_window::SftpTabs,
    sftp_panel: SftpPanel,
    sftp_window: Option<sftp_window::SftpWindow>,
    sftp_session: Option<SessionId>,
    last_sftp_generation: Option<u64>,
    tunnel_panel: tunnel_panel::TunnelPanel,
    last_tunnel_generation: Option<u64>,
    host_ca_panel: host_ca_panel::HostCaPanel,
    last_host_ca_generation: Option<u64>,
    ssh_provider_panel: ssh_provider_panel::SshProviderPanel,
    last_ssh_provider_generation: Option<u64>,
    profile_batch_panel: profile_batch_panel::ProfileBatchPanel,
    last_profile_batch_generation: Option<u64>,
    terminal_tools_panel: terminal_tools_panel::TerminalToolsPanel,
    highlight_worker: Option<cshell_render::HighlightWorker>,
    highlight_attempted: bool,
    last_highlight_generation: u64,
    clipboard: Option<clipboard_connection::DesktopClipboardConnection>,
    last_clipboard_generation: Option<u64>,
    clipboard_policy: Option<clipboard_policy::DesktopClipboardPolicy>,
    last_clipboard_policy_generation: Option<u64>,
    pending_tab: Option<SessionId>,
    workspace: workspace_model::DesktopWorkspace,
    opened_sessions: Arc<std::sync::Mutex<std::collections::BTreeSet<SessionId>>>,
    workspace_connection: Option<Arc<workspace_connection::WorkspaceConnection>>,
    workspace_recovery_panel: workspace_recovery_panel::WorkspaceRecoveryPanel,
    workspace_runtime_busy: bool,
    transfer_group_to: Option<Vec<cshell_domain::TabId>>,
    retry_workspace_windows: bool,
    workspace_config: Option<DesktopConnectionConfig>,
    workspace_loaded: bool,
    pane_daemons: std::collections::BTreeMap<cshell_domain::TabId, DesktopDaemonConnection>,
    sync_daemons: std::collections::BTreeMap<cshell_domain::TabId, DesktopDaemonConnection>,
    sync_input: Option<Arc<std::sync::Mutex<sync_input::DesktopSyncInput>>>,
    last_sync_view: Option<sync_input::DesktopSyncViewStamp>,
    tab_activity: tab_activity::TabActivity,
    daemon_config: Option<DesktopConnectionConfig>,
    workspace_layout_error: Option<String>,
    closing_workspace_tab: Option<cshell_domain::TabId>,
    ssh_control: Option<ssh_session_controls::PendingSshControl>,
    ssh_tab_states: Arc<std::sync::Mutex<std::collections::BTreeMap<cshell_domain::TabId, cshell_ui::SshConnectionState>>>,
    current_workspace_tab: Option<cshell_domain::TabId>,
    cached_tab_views: std::collections::BTreeMap<cshell_domain::TabId, CachedTabView>,
    terminal_surface: TerminalSurfaceModel,
    terminal_decorations: TerminalDecorations,
    terminal_search: Option<TerminalSearchWorker>,
    terminal_search_open: bool,
    terminal_search_focus_requested: bool,
    terminal_search_query: String,
    terminal_search_options: cshell_render::TerminalSearchOptions,
    terminal_search_error: Option<String>,
    terminal_search_revision: u64,
    terminal_search_requested_generation: Option<u64>,
    log_surface: Option<LogSurfaceModel>,
    terminal_scrollback: bool,
    terminal_reading: terminal_reading::State,
    log_decorations: LogDecorations,
    submitted_history_search: Option<Arc<DesktopHistorySearchResponse>>,
    history_search_direction: Option<HistorySearchDirection>,
    submitted_log_page: Option<(LogSourceId, u64)>,
    log_reflow: Option<LogReflowWorker>,
    terminal_viewport: Option<TerminalViewport>,
    viewport_rows: u16,
    cursor_position: Option<winit::dpi::PhysicalPosition<f64>>,
    terminal_selecting: bool,
    terminal_click: TerminalClickState,
    terminal_clipboard: terminal_clipboard::State,
    command_center: command_center::State,
    terminal_mouse: terminal_mouse::State,
    pending_paste: Option<PendingPaste>,
    wheel_row_accumulator: f64,
    modifiers: ModifiersState,
    cursor_blink: CursorBlinkState,
    window_active: bool,
    key_picker_focus_deferred: bool,
    window_e2e: Option<window_e2e::WindowE2e>,
    transfer_to: Option<(cshell_domain::TabId, cshell_domain::WorkspaceWindowId)>,
    terminal_frame_presented: bool,
    secondary_window: bool,
    profile_theme_generation: Option<u64>,
    profile_policy_stamp: Option<(u64, u64, u64)>,
    terminal_policy_panel: terminal_policy_panel::TerminalPolicyPanel,
    pending_terminal_context: Option<std::sync::Arc<cshell_application::TerminalResolutionReport>>,
    saved_profiles_generation: Option<u64>,
    default_terminal_theme: TerminalTheme,
    profile_themes: std::collections::BTreeMap<cshell_domain::ProfileId, TerminalTheme>,
}

#[derive(Default)]
struct SplitRenderFrame {
    viewport: TerminalViewport,
    theme: TerminalTheme,
    terminal: Option<cshell_render::TerminalSurfaceFrame>,
    log: Option<cshell_render::LogSurfaceFrame>,
    decorations: TerminalDecorations,
    log_decorations: LogDecorations,
    focused: bool,
}

#[derive(Default)]
struct CachedTabView {
    reading: terminal_reading::State,
    clipboard: terminal_clipboard::State,
    surface: TerminalSurfaceModel,
    decorations: TerminalDecorations,
    log_surface: Option<LogSurfaceModel>,
    terminal_scrollback: bool,
    log_decorations: LogDecorations,
    query: String,
    options: cshell_render::TerminalSearchOptions,
    search_open: bool,
    last_log_page: Option<(LogSourceId, u64)>,
}

struct TabReflowRequest {
    tab_id: Option<cshell_domain::TabId>,
    request: LogReflowRequest,
}
struct TabReflowResult {
    tab_id: Option<cshell_domain::TabId>,
    result: Result<LogReflowLayout, LogSurfaceError>,
}

struct LogReflowWorker {
    requests: Option<std::sync::mpsc::Sender<TabReflowRequest>>,
    results: std::sync::mpsc::Receiver<TabReflowResult>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogScrollbarAction {
    FollowTail,
    JumpTo(u64),
}

#[derive(Debug, Eq, PartialEq)]
enum PasteDecision {
    Immediate(String),
    Confirm(String),
}

struct PendingPaste {
    text: String,
    session_id: SessionId,
    session_title: String,
}

impl DesktopApp {
    fn schedule_egui_repaint(&mut self, output: &egui::FullOutput, now: std::time::Instant) {
        self.egui_repaint_deadline = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .filter(|viewport| viewport.repaint_delay != std::time::Duration::MAX)
            .and_then(|viewport| now.checked_add(viewport.repaint_delay));
    }

    fn poll_egui_repaint(&mut self, now: std::time::Instant) -> bool {
        if self
            .egui_repaint_deadline
            .is_some_and(|deadline| deadline <= now)
        {
            self.egui_repaint_deadline = None;
            true
        } else {
            false
        }
    }

    fn next_wake_deadline(&self, poll_deadline: std::time::Instant) -> std::time::Instant {
        [self.cursor_blink.next_toggle(), self.egui_repaint_deadline]
            .into_iter()
            .flatten()
            .fold(poll_deadline, std::cmp::min)
    }

    fn terminal_selection_text(
        &self,
    ) -> Option<Result<String, cshell_render::TerminalInteractionError>> {
        if let Some(text) = self.cross_selection_text() { return Some(text); }
        if let (Some(frame), Some(snapshot), Some(selection)) = (&self.terminal_clipboard.history_frame,
            &self.terminal_clipboard.snapshot, self.terminal_decorations.selection) {
            return Some(terminal_clipboard::history_text(frame, snapshot, selection));
        }
        Some(
            self.terminal_decorations
                .selection?
                .text(self.terminal_clipboard.snapshot.as_ref().or_else(|| self.terminal_surface.latest_snapshot())?),
        )
    }

    fn poll_terminal_search(&mut self) -> bool {
        let mut redraw_needed = false;
        let search_response = self
            .terminal_search
            .as_ref()
            .and_then(TerminalSearchWorker::take_latest);
        if let Some(response) = search_response {
            let current_generation = self.terminal_clipboard.snapshot.as_ref()
                .or_else(|| self.terminal_surface.latest_snapshot())
                .map(|snapshot| snapshot.generation);
            if response.revision == self.terminal_search_revision
                && current_generation == Some(response.snapshot_generation)
            {
                match response.result {
                    Ok(result) => {
                        self.terminal_search_error = None;
                        let previous_active =
                            self.terminal_decorations.active_search_match.unwrap_or(0);
                        self.terminal_decorations.search = result;
                        self.terminal_decorations.active_search_match =
                            (!self.terminal_decorations.search.matches.is_empty()).then_some(
                                previous_active
                                    .min(self.terminal_decorations.search.matches.len() - 1),
                            );
                        self.bump_terminal_decorations();
                        redraw_needed = true;
                    }
                    Err(error) => {
                        self.terminal_search_error = Some(error.to_string());
                        self.terminal_decorations.search = Default::default();
                        self.terminal_decorations.active_search_match = None;
                        self.bump_terminal_decorations();
                        redraw_needed = true;
                        tracing::warn!(%error, "terminal search rejected");
                    }
                }
            }
        }
        redraw_needed
    }

    fn poll_primary_terminal(&mut self, now: std::time::Instant) -> bool {
        let mut redraw_needed = false;
        if let Some(view)=self.daemon.as_ref().map(DesktopDaemonConnection::view) {
            if self.finish_cancelled_launch(&view) {return true;}
        }
        if let Some(daemon) = &self.daemon
            && (self.current_workspace_tab.is_none()
                || self
                    .current_workspace_tab
                    .and_then(|tab| self.workspace.sessions.get(&tab))
                    .is_some_and(|id| daemon.view().session_id == Some(*id)))
        {
            let view = daemon.view();
            if self.log_surface.is_some() && self.submitted_log_page.is_none() && view.session_id.is_some() {
                let request=cshell_render::LogPageRequest{source_id:cshell_render::LogSourceId(0),follow_tail:true,anchor_line_id:0,cell_offset:0,rows_before:self.viewport_rows.saturating_add(128).min(4095),rows_after:0};
                if self.terminal_scrollback{daemon.request_terminal_history_page(request);}else{daemon.request_log_page(request);}
            }
            redraw_needed |= self
                .profile_panel
                .set_launch_error(view.launch_error.clone(), view.session_profile_id);
            if let Some(response) = view.history_search.as_ref()
                && self
                    .submitted_history_search
                    .as_ref()
                    .is_none_or(|submitted| !Arc::ptr_eq(submitted, response))
            {
                self.submitted_history_search = Some(Arc::clone(response));
                if response.revision == self.terminal_search_revision && self.log_surface.is_some()
                    && self.terminal_search_open && self.history_search_direction.is_some()
                {
                    if let Some(matched) = response.result.matches.first() {
                        self.terminal_search_error = response.result.incomplete.then(|| {
                            "history search encountered a truncated oversized line".to_owned()
                        });
                        self.log_decorations.revision =
                            self.log_decorations.revision.wrapping_add(1);
                        self.log_decorations.active_search_match = Some(LogSearchMatch {
                            line_id: matched.line_id,
                            byte_range: matched.byte_start..matched.byte_end,
                        });
                        if let Some(surface) = &mut self.log_surface {
                            surface.jump_to_byte(matched.line_id, matched.byte_start);
                        }
                        self.history_search_direction = None;
                        redraw_needed = true;
                    } else if let (Some(line_id), Some(byte_offset), Some(direction)) = (
                        response.result.next_line_id,
                        response.result.next_byte_offset,
                        self.history_search_direction,
                    ) {
                        daemon.request_history_search(self.history_search_request(
                            response.revision,
                            direction,
                            Some((line_id, byte_offset)),
                        ));
                    } else {
                        self.terminal_search_error = Some(if response.result.incomplete {
                            "history search is incomplete because a logical line exceeded 4 MiB"
                                .to_owned()
                        } else {
                            "no history match found".to_owned()
                        });
                        redraw_needed = true;
                    }
                }
            }
            redraw_needed |= self.view_model.daemon_connected != view.connected;
            redraw_needed |= self.view_model.daemon_status_detail != view.detail;
            redraw_needed |= self.view_model.terminal_generation
                != view.snapshot.as_ref().map(|snapshot| snapshot.generation);
            self.view_model.daemon_connected = view.connected;
            let can_reconnect =
                view.session_profile_id.is_some() && !view.connected && !view.session_opening;
            redraw_needed |= self.view_model.can_reconnect_ssh != can_reconnect;
            self.view_model.can_reconnect_ssh = can_reconnect;
            self.view_model.daemon_status_detail = view.detail;
            self.view_model.terminal_generation =
                view.snapshot.as_ref().map(|snapshot| snapshot.generation);
            if let Some(snapshot) = view.snapshot {
                redraw_needed |= self.cursor_blink.synchronize(
                    Some(snapshot.as_ref()),
                    self.window_active && view.connected,
                    now,
                );
                redraw_needed |= self.terminal_surface.submit_snapshot(snapshot);
            }
            if let Some(page) = view.log_page && view.log_page_terminal_history == self.terminal_scrollback {
                let key = (page.source_id, page.revision);
                if self.submitted_log_page != Some(key)
                    && let Some(log_surface) = &mut self.log_surface
                {
                    redraw_needed = true;
                    if let Err(error) = log_surface.submit_page(page) {
                        tracing::error!(%error, "daemon log page rejected by render model");
                    }
                    // A delivered page is immutable. Even when it is stale for
                    // the current anchor, retrying it every frame cannot make it
                    // acceptable and would only flood the log until the newer
                    // request arrives on the dedicated paging connection.
                    self.submitted_log_page = Some(key);
                }
            }
            if view.session_id.is_none() && !view.connected && !view.session_opening {
                redraw_needed |= self.view_model.selected.is_some();
                if self.view_model.selected.is_some() {
                    self.terminal_surface = TerminalSurfaceModel::default();
                    self.terminal_decorations = TerminalDecorations::default();
                    self.terminal_clipboard = terminal_clipboard::State::default();
                    self.log_surface = None;
                    self.terminal_scrollback = false;
                    self.submitted_log_page = None;
                }
                self.view_model.selected = None;
                for entry in &mut self.view_model.sessions {
                    entry.connected = false;
                }
            }
            if let Some(session_id) = view.session_id {
                if self.pending_tab == Some(session_id) || view.launch_error.is_some() {
                    self.pending_tab = None;
                }
                if self.pending_tab.is_none() && self.view_model.selected != Some(session_id) {
                    self.terminal_surface = TerminalSurfaceModel::default();
                    self.terminal_decorations = TerminalDecorations::default();
                    self.terminal_clipboard = terminal_clipboard::State::default();
                    self.log_surface = None;
                    self.terminal_scrollback = false;
                    self.submitted_log_page = None;
                    self.view_model.selected = Some(session_id);
                    redraw_needed = true;
                }
                for entry in &mut self.view_model.sessions {
                    let connected = entry.id == session_id && view.connected;
                    redraw_needed |= entry.connected != connected;
                    entry.connected = connected;
                }
                if let Some(session) = self
                    .view_model
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    redraw_needed |= session.connected != view.connected;
                    session.connected = view.connected;
                } else {
                    redraw_needed = true;
                    self.view_model
                        .sessions
                        .push(cshell_ui::SessionTabViewModel {
                            id: session_id,
                            title: view
                                .session_title
                                .clone()
                                .unwrap_or_else(|| "Terminal".to_owned()),
                            connected: view.connected,
                        });
                    self.view_model.selected = Some(session_id);
                }
            }
        }
        if let Some(view) = self.daemon.as_ref().map(DesktopDaemonConnection::view) {
            redraw_needed |= self.sync_opened_sessions(&view);
        }
        redraw_needed
    }

    fn sync_opened_sessions(&mut self, view: &daemon_connection::DesktopDaemonView) -> bool {
        // Runtime-only scope shared by this GUI's windows. A daemon listing is
        // metadata, not evidence that this application run opened those sessions.
        let mut opened = self.opened_sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        opened.extend(self.workspace.sessions.values().copied());
        if let Some(id) = view.session_id { opened.insert(id); }
        if let Some(id) = view.closed_session { opened.remove(&id); }
        let mut sessions: Vec<_> = if let Some(summaries) = &view.sessions {
            summaries.iter().filter_map(|summary| {
                let id = SessionId::from_bytes(summary.session_id.as_slice().try_into().ok()?);
                opened.contains(&id).then(|| cshell_ui::SessionTabViewModel {
                    id, title: summary.title.clone(), connected: view.connected && view.session_id == Some(id),
                })
            }).collect()
        } else {
            self.view_model.sessions.iter().filter(|entry| opened.contains(&entry.id)).cloned().collect()
        };
        if let Some(id) = view.session_id
            && opened.contains(&id)
            && !sessions.iter().any(|session| session.id == id)
        {
            sessions.push(cshell_ui::SessionTabViewModel {
                id, title: view.session_title.clone().unwrap_or_else(|| "Terminal".into()), connected: view.connected,
            });
        }
        let reusable = view.sessions.as_ref().map(|all|all.iter().filter(|s|s.can_duplicate_ssh_channel && s.running).filter_map(|s|Some(SessionId::from_bytes(s.session_id.as_slice().try_into().ok()?))).collect::<Vec<_>>()).unwrap_or_default();
        let changed = self.view_model.sessions != sessions || self.view_model.reusable_ssh_sessions != reusable;
        self.view_model.reusable_ssh_sessions = reusable;
        self.view_model.sessions = sessions;
        changed
    }

    fn update_profile_themes(&mut self, view: &profile_connection::DesktopProfileView) -> bool {
        let policies = self.terminal_policy_panel.catalog();
        let stamp = policies
            .as_ref()
            .map(|p| (p.catalog.revision, p.profile_revision, p.host_revision));
        if self.profile_theme_generation == Some(view.generation)
            && self.profile_policy_stamp == stamp
        {
            return false;
        }
        self.profile_theme_generation = Some(view.generation);
        self.profile_policy_stamp = stamp;
        self.profile_themes.clear();
        self.default_terminal_theme = TerminalTheme::Dark;
        if let Some(data) = &view.catalog {
            self.default_terminal_theme =
                TerminalTheme::from_catalog_name(&data.defaults.theme, &data.themes);
            let snapshot = cshell_application::CatalogSnapshot {
                revision: data.revision,
                defaults: data.defaults.clone(),
                folders: data.folders.clone(),
                profiles: data.profiles.clone(),
                ssh_connections: data.ssh_connections.clone(),
                local_connections: data.local_connections.clone(),
                themes: data.themes.clone(),
            };
            if let Ok(catalog) = cshell_application::ProfileCatalog::from_snapshot(snapshot) {
                for profile in &data.profiles {
                    let settings = policies
                        .as_ref()
                        .filter(|p| p.profile_revision == data.revision)
                        .and_then(|p| {
                            cshell_application::resolve_validated_terminal_context(
                                &catalog,
                                &p.catalog,
                                &cshell_application::TerminalResolutionContext {
                                    profile_id: profile.id,
                                    host_id: None,
                                    temporary: Default::default(),
                                },
                                None,
                            )
                            .ok()
                            .map(|(settings, _)| settings)
                        })
                        .or_else(|| catalog.resolve(profile.id).ok());
                    if let Some(settings) = settings {
                        self.profile_themes.insert(
                            profile.id,
                            TerminalTheme::from_catalog_name(&settings.theme.value, &data.themes),
                        );
                    }
                }
            }
        }
        true
    }

    fn theme_for_tab(&self, tab: Option<cshell_domain::TabId>) -> TerminalTheme {
        if let Some(tab) = tab
            && let Some(session) = self.workspace.sessions.get(&tab)
            && let Some(profile) = self
                .workspace
                .document
                .bindings
                .iter()
                .find(|b| b.tab_id == tab)
                .and_then(|b| b.profile_id)
            && let Some(profiles) = self.profiles.as_ref().and_then(|p| p.view().catalog)
            && let Some(theme) =
                self.terminal_policy_panel
                    .theme_for(tab, *session, profile, profiles.revision)
        {
            return TerminalTheme::from_catalog_name(&theme, &profiles.themes);
        }
        tab.and_then(|tab| {
            self.workspace
                .document
                .bindings
                .iter()
                .find(|b| b.tab_id == tab)
        })
        .and_then(|binding| binding.profile_id)
        .and_then(|id| self.profile_themes.get(&id).copied())
        .unwrap_or(self.default_terminal_theme)
    }

    fn prepare_split_frames(&mut self, scale: f32) -> Option<Vec<SplitRenderFrame>> {
        if self.view_model.pane_views.len() < 2 {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        let mut frames = Vec::new();
        for pane in &self.view_model.pane_views {
            let theme = self.theme_for_tab(pane.tab_id);
            let viewport = TerminalViewport::from_logical_rect(pane.rect, scale);
            let rows = renderer.viewport_rows(viewport);
            let columns = renderer.viewport_columns(viewport);
            let focused = pane.tab_id == self.current_workspace_tab && pane.tab_id.is_some();
            let cached = pane
                .tab_id
                .and_then(|tab| self.cached_tab_views.get_mut(&tab));
            let frozen_snapshot = if focused { self.terminal_clipboard.snapshot.clone() }
                else { cached.as_ref().and_then(|c| c.clipboard.snapshot.clone()) };
            let frozen_history = if focused { self.terminal_clipboard.history_frame.clone() }
                else { cached.as_ref().and_then(|c| c.clipboard.history_frame.clone()) };
            let memory_history=if focused{self.terminal_scrollback}else{cached.as_ref().is_some_and(|c|c.terminal_scrollback)};
            let (surface, log_surface, decorations, log_decorations) = if focused {
                (
                    &mut self.terminal_surface,
                    &mut self.log_surface,
                    &self.terminal_decorations,
                    &self.log_decorations,
                )
            } else if let Some(cached) = cached {
                (
                    &mut cached.surface,
                    &mut cached.log_surface,
                    &cached.decorations,
                    &cached.log_decorations,
                )
            } else {
                frames.push(SplitRenderFrame {
                    viewport,
                    theme,
                    ..Default::default()
                });
                continue;
            };
            let connection = if focused {
                self.daemon.as_ref()
            } else {
                pane.tab_id.and_then(|t| self.pane_daemons.get(&t))
            };
            let mut frame = SplitRenderFrame {
                viewport,
                theme,
                decorations: decorations.clone(),
                log_decorations: log_decorations.clone(),
                focused,
                ..Default::default()
            };
            if let Some(log_surface) = log_surface {
                if let Some(request) = log_surface.request_reflow(columns)
                    && self
                        .log_reflow
                        .as_ref()
                        .is_none_or(|w| !w.submit(request, pane.tab_id))
                {
                    log_surface.reflow_failed();
                }
                if let Some(request) = log_surface.take_page_request(rows)
                    && let Some(connection) = connection
                {
                    if memory_history{connection.request_terminal_history_page(request);}else{connection.request_log_page(request);}
                }
                frame.log = frozen_history.or_else(|| log_surface.prepare_frame(rows));
            } else {
                if let Some(connection) = connection {
                    connection.request_resize(
                        rows,
                        columns,
                        viewport.width.min(u32::from(u16::MAX)) as u16,
                        viewport.height.min(u32::from(u16::MAX)) as u16,
                    );
                }
                frame.terminal = surface.prepare_frame(0, rows);
                if let (Some(terminal), Some(snapshot)) = (&mut frame.terminal, &frozen_snapshot) {
                    terminal.snapshot = snapshot.clone();
                    terminal.plan.generation = snapshot.generation;
                }
            }
            if let Some(session) = pane
                .tab_id
                .and_then(|tab| self.workspace.sessions.get(&tab).copied())
                && let Some(worker) = &mut self.highlight_worker
            {
                let profile = connection.and_then(|c| c.session_profile(session));
                let (revision, rules) = self.terminal_tools_panel.highlight_rules(session, profile);
                if let Some(terminal) = &frame.terminal {
                    worker.terminal(
                        session,
                        revision,
                        rules,
                        terminal.snapshot.clone(),
                        &mut frame.decorations,
                    );
                } else if let Some(log) = &frame.log {
                    worker.log(session, revision, rules, log, &mut frame.log_decorations);
                }
            }
            frames.push(frame);
        }
        Some(frames)
    }

    fn refresh_workspace_tabs(&mut self) {
        self.update_workspace_tab_models();
        if self.workspace.writable
            && !self.secondary_window
            && let Some(connection) = &self.workspace_connection
        {
            connection.save(&self.workspace.document);
        }
    }

    fn update_workspace_tab_models(&mut self) {
        self.view_model.workspace_document = Some(self.workspace.document.clone());
        self.view_model.workspace_window_id = Some(self.workspace.window_id);
        let focused = self.workspace.focused_group();
        self.view_model.tabs = self
            .workspace
            .document
            .tab_groups
            .iter()
            .find(|g| g.id == focused)
            .unwrap_or(&self.workspace.document.tab_groups[0])
            .tabs
            .iter()
            .filter_map(|id| {
                let binding = self
                    .workspace
                    .document
                    .bindings
                    .iter()
                    .find(|b| b.tab_id == *id)?;
                Some(cshell_ui::WorkspaceTabViewModel {
                    id: *id,
                    title: binding.title.clone(),
                    session_id: self.workspace.sessions.get(id).copied(),
                })
            })
            .collect();
        self.view_model.selected_tab = self.workspace.active();
    }

    fn initialize_workspace(&mut self, view: &workspace_connection::WorkspaceView) {
        // Reading the saved revision remains the autosave/CAS barrier. Startup
        // opens a fresh window, never stale placeholders or automatic terminals.
        // Preserve the saved snapshot until the user edits the new workspace.
        self.workspace = workspace_model::DesktopWorkspace {
            writable: view.error.is_none() && !view.paused && !view.blocked,
            ..Default::default()
        };
        self.workspace_layout_error = view.error.clone();
        self.workspace_loaded = true;
        self.current_workspace_tab = None;
        self.view_model.selected = None;
        if let Some(config) = self.workspace_config.take() {
            self.daemon_config = Some(config.clone());
            self.daemon = match DesktopDaemonConnection::start_idle(config) {
                Ok(daemon) => Some(daemon),
                Err(error) => {
                    self.view_model.workspace_status = error.to_string();
                    None
                }
            };
        }
        self.update_workspace_tab_models();
    }

    fn switch_tab_view(&mut self, tab: cshell_domain::TabId) {
        if self.current_workspace_tab == Some(tab) {
            return;
        }
        self.release_remote_mouse();
        if self.window_active
            && let Some(sync) = &self.sync_input
        {
            sync.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .source_changed(tab);
        }
        if let Some(window) = &self.window {
            window.set_ime_allowed(false);
        }
        if let Some(previous) = self.current_workspace_tab {
            if let Some(connection) = self.daemon.take() {
                self.pane_daemons.insert(previous, connection);
            }
            self.daemon = self.pane_daemons.remove(&tab).or_else(|| self.sync_daemons.remove(&tab));
        } else if let Some(connection) = self.pane_daemons.remove(&tab) {
            if let Some(idle) = self.daemon.take() {
                idle.retire();
            }
            self.daemon = Some(connection);
        }
        if self.daemon.is_none()
            && let Some(config) = &self.daemon_config
        {
            self.daemon = DesktopDaemonConnection::start_idle(config.clone()).ok();
        }
        self.terminal_clipboard.menu = None;
        let old = CachedTabView {
            reading: std::mem::take(&mut self.terminal_reading),
            clipboard: std::mem::take(&mut self.terminal_clipboard),
            surface: std::mem::take(&mut self.terminal_surface),
            decorations: std::mem::take(&mut self.terminal_decorations),
            log_surface: self.log_surface.take(),
            terminal_scrollback: self.terminal_scrollback,
            log_decorations: std::mem::take(&mut self.log_decorations),
            query: std::mem::take(&mut self.terminal_search_query),
            options: std::mem::take(&mut self.terminal_search_options),
            search_open: self.terminal_search_open,
            last_log_page: self.submitted_log_page,
        };
        if let Some(id) = self.current_workspace_tab {
            self.cached_tab_views.insert(id, old);
        }
        let next = self.cached_tab_views.remove(&tab).unwrap_or_default();
        self.terminal_reading = next.reading;
        self.terminal_surface = next.surface;
        self.terminal_decorations = next.decorations;
        self.terminal_clipboard = next.clipboard;
        self.log_surface = next.log_surface;
        self.terminal_scrollback = next.terminal_scrollback;
        self.log_decorations = next.log_decorations;
        self.terminal_search_query = next.query;
        self.terminal_search_options = next.options;
        self.terminal_search_open = next.search_open;
        self.terminal_search_revision = self.terminal_search_revision.saturating_add(1);
        self.terminal_search_requested_generation = None;
        self.submitted_log_page = next.last_log_page;
        self.submitted_history_search = None;
        self.history_search_direction = None;
        self.terminal_search_focus_requested = false;
        self.terminal_search_error = None;
        self.pending_paste = None;
        self.terminal_selecting = false;
        self.terminal_clipboard.menu = None;
        self.wheel_row_accumulator = 0.0;
        self.current_workspace_tab = Some(tab);
        self.view_model.unread_output_tabs.remove(&tab);
    }

    fn activate_workspace_tab(&mut self, tab: cshell_domain::TabId) {
        if self
            .workspace_connection
            .as_ref()
            .is_some_and(|c| c.view().paused)
        {
            return;
        }
        if self.workspace.pending_launch.is_some() {
            self.view_model.selected_tab = self.workspace.active();
            return;
        }
        if self.workspace.window_of_tab(tab) != Some(self.workspace.window_id) {
            return;
        }
        self.switch_tab_view(tab);
        self.workspace.select(tab);
        self.resume_workspace_tab_runtime(tab);
        self.refresh_workspace_tabs();
    }

    /// Restore an existing runtime view without modifying the saved layout.
    /// Process-close recovery must also work while autosave is paused.
    fn resume_workspace_tab_runtime(&mut self, tab: cshell_domain::TabId) {
        self.switch_tab_view(tab);
        self.view_model.selected = self.workspace.sessions.get(&tab).copied();
        if let Some(daemon) = &self.daemon {
            if let Some(id) = self.view_model.selected {
                if daemon.view().session_id != Some(id) {
                    self.pending_tab = Some(id);
                    daemon.attach_session(id);
                } else {
                    self.pending_tab = None;
                }
            } else {
                self.pending_tab = None;
                daemon.detach_for_placeholder();
            }
        }
    }

    fn prepare_profile_tab(&mut self, id: cshell_domain::ProfileId) -> bool {
        if self.workspace.pending_launch.is_some()
            || self
                .workspace_connection
                .as_ref()
                .is_some_and(|c| c.view().paused)
            || (!self.workspace_loaded && self.workspace_connection.is_some())
        {
            return false;
        }
        if let Some(tab) = self.current_workspace_tab
            && !self.workspace.sessions.contains_key(&tab)
            && self.workspace.document.bindings.iter().any(|binding| binding.tab_id == tab && binding.profile_id == Some(id))
            && self.daemon.as_ref().is_some_and(|daemon| {
                let view = daemon.view();
                view.session_profile_id == Some(id) && view.launch_error.is_some()
            })
        {
            self.workspace.pending_launch = Some(tab);
            return true;
        }
        self.pending_terminal_context = None;
        let title = self
            .profiles
            .as_ref()
            .and_then(|p| p.view().catalog)
            .and_then(|catalog| catalog.profiles.into_iter().find(|p| p.id == id))
            .map_or("Profile".into(), |p| p.name);
        let Some(tab) = self.workspace.add(Some(id), title) else {
            self.view_model.workspace_status = "Workspace tab limit reached".into();
            return false;
        };
        self.switch_tab_view(tab);
        self.workspace.pending_launch = Some(tab);
        self.view_model.selected = None;
        self.refresh_workspace_tabs();
        true
    }

    fn apply_log_reflow(&mut self, completed: TabReflowResult) -> bool {
        let active = completed.tab_id == self.current_workspace_tab;
        let surface = if active {
            self.log_surface.as_mut()
        } else {
            completed
                .tab_id
                .and_then(|tab| self.cached_tab_views.get_mut(&tab))
                .and_then(|v| v.log_surface.as_mut())
        };
        let Some(surface) = surface else {
            return false;
        };
        match completed.result {
            Ok(layout) => {
                let promoted = surface.submit_reflow(layout);
                if let Some(e2e) = &mut self.window_e2e {
                    e2e.note_reflow(promoted);
                }
                promoted
                    && (active
                        || completed
                            .tab_id
                            .is_some_and(|tab| self.workspace.visible_tabs().contains(&tab)))
            }
            Err(error) => {
                surface.reflow_failed();
                tracing::error!(%error, "background log reflow failed");
                false
            }
        }
    }

    fn discard_closed_tab_view(&mut self, tab: cshell_domain::TabId) {
        self.ssh_tab_states.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&tab);
        self.cached_tab_views.remove(&tab);
        if let Some(connection) = self.pane_daemons.remove(&tab) {
            connection.retire();
        }
        if self.current_workspace_tab == Some(tab) {
            self.current_workspace_tab = None;
            if let Some(connection) = self.daemon.take() {
                connection.retire();
            }
            self.terminal_surface = Default::default();
            self.terminal_decorations = Default::default();
            self.terminal_clipboard = terminal_clipboard::State::default();
            self.log_surface = None;
                    self.terminal_scrollback = false;
            self.log_decorations = Default::default();
            self.terminal_search_query.clear();
            self.terminal_search_open = false;
            self.terminal_search_revision = self.terminal_search_revision.saturating_add(1);
            self.pending_paste = None;
            self.view_model.selected = None;
            self.view_model.daemon_connected = false;
            self.view_model.can_reconnect_ssh = false;
        }
    }

    fn ensure_metadata_connection(&mut self) {
        if self.daemon.is_none() && let Some(config) = &self.daemon_config {
            match DesktopDaemonConnection::start_idle(config.clone()) {
                Ok(connection) => self.daemon = Some(connection),
                Err(error) => self.view_model.input_warning = Some(error.to_string()),
            }
        }
    }

    fn request_termination(&mut self, id: SessionId) {
        self.ensure_metadata_connection();
        let observed = self.view_model.sessions.iter().any(|session| session.id == id)
            || self.workspace.sessions.values().any(|session| *session == id);
        let error = if self.workspace.pending_launch.is_some() {
            Some("当前连接操作尚未完成，请等待后重试。".to_owned())
        } else if let Some(daemon) = &self.daemon {
            if observed && daemon.terminate_observed_session(id) {
                None
            } else {
                Some("未能提交终止请求：会话列表已变化或另一个关闭操作仍在进行，请核对后重试。".to_owned())
            }
        } else {
            Some("后台服务尚未连接，未提交终止请求。".to_owned())
        };
        self.view_model.terminate_pending = error.is_none();
        self.view_model.terminate_error = error;
        if self.view_model.terminate_pending {
            self.closing_workspace_tab = self.workspace.sessions.iter()
                .find(|(tab, session)| **session == id && self.workspace.window_of_tab(**tab) == Some(self.workspace.window_id))
                .map(|(tab, _)| *tab);
            self.pending_paste = None;
        }
    }

    fn sync_termination(&mut self, view: &daemon_connection::DesktopDaemonView) -> bool {
        if !self.view_model.terminate_pending { return false; }
        if view.closed_session == self.view_model.terminate_target && view.closed_session.is_some() {
            self.view_model.terminate_pending = false;
            self.view_model.confirm_terminate = false;
            self.view_model.terminate_target = None;
            self.view_model.terminate_error = None;
            true
        } else if view.close_pending.is_none() && let Some(error) = &view.close_error {
            self.view_model.terminate_pending = false;
            self.view_model.terminate_error = Some(error.clone());
            self.closing_workspace_tab = None;
            true
        } else { false }
    }

    fn close_workspace_tab(&mut self, tab: cshell_domain::TabId) {
        if self
            .workspace_connection
            .as_ref()
            .is_some_and(|c| c.view().paused)
        {
            return;
        }
        if self.workspace.pending_launch.is_some() {
            return;
        }
        if let Some(id) = self.workspace.sessions.get(&tab).copied() {
            self.ensure_metadata_connection();
            if self
                .daemon
                .as_ref()
                .is_some_and(|daemon| daemon.terminate_observed_session(id))
            {
                self.closing_workspace_tab = Some(tab);
                self.pending_paste = None;
            } else {
                self.view_model.input_warning = Some("未能提交关闭请求，请等待当前操作完成后重试。".into());
            }
        } else {
            self.workspace.remove(tab);
            self.discard_closed_tab_view(tab);
            if let Some(next) = self.workspace.active() {
                self.activate_workspace_tab(next);
            } else {
                self.current_workspace_tab = None;
                self.terminal_surface = Default::default();
            }
            self.refresh_workspace_tabs();
            self.ensure_metadata_connection();
        }
    }

    fn sync_workspace(&mut self) -> bool {
        let mut changed = false;
        if let Some(connection) = &self.workspace_connection {
            let view = connection.view();
            let status = self
                .workspace_layout_error
                .clone()
                .or(view.error.clone())
                .unwrap_or_else(|| view.status.clone());
            changed |= status != self.view_model.workspace_status;
            self.view_model.workspace_status = status;
            if view.ready && !self.workspace_loaded {
                self.initialize_workspace(&view);
                changed = true;
            }
        }
        // A window without terminal tabs still needs the daemon's session list
        // and close commands for sessions retained by previous versions/windows.
        self.ensure_metadata_connection();
        let visible = self.workspace.visible_tabs();
        let hidden: Vec<_> = self
            .pane_daemons
            .keys()
            .copied()
            .filter(|tab| !self.workspace.sessions.contains_key(tab) || self.workspace.window_of_tab(*tab) != Some(self.workspace.window_id))
            .collect();
        for tab in hidden {
            if let Some(connection) = self.pane_daemons.remove(&tab) {
                connection.retire();
            }
        }
        // Keep one IPC attachment per open tab, including hidden recipients and
        // activity monitors. This attaches existing sessions, never starts SSH.
        let opened: Vec<_> = self.workspace.sessions.keys().copied().filter(|tab| self.workspace.window_of_tab(*tab) == Some(self.workspace.window_id)).collect();
        for tab in &opened {
            if Some(*tab) == self.current_workspace_tab || self.pane_daemons.contains_key(tab) {
                continue;
            }
            if let Some(connection) = self.sync_daemons.remove(tab) { self.pane_daemons.insert(*tab, connection); continue; }
            if let (Some(session), Some(config)) = (
                self.workspace.sessions.get(tab),
                self.daemon_config.as_ref(),
            ) && let Ok(connection) = DesktopDaemonConnection::start_idle(config.clone())
            {
                connection.attach_session(*session);
                self.pane_daemons.insert(*tab, connection);
            }
        }
        for (tab, connection) in &self.pane_daemons {
            // Hidden workers retain one latest snapshot for activity and fast
            // activation; only visible panes need surface updates and redraws.
            if !visible.contains(tab) { continue; }
            let view = connection.view();
            if view.session_id != self.workspace.sessions.get(tab).copied() {
                continue;
            }
            let cached = self.cached_tab_views.entry(*tab).or_default();
            if let Some(snapshot) = view.snapshot {
                changed |= cached.surface.submit_snapshot(snapshot);
            }
            if let (Some(page), Some(surface)) = (view.log_page, cached.log_surface.as_mut()) {
                let key = (page.source_id, page.revision);
                if cached.last_log_page != Some(key) && view.log_page_terminal_history == cached.terminal_scrollback {
                    if let Err(error) = surface.submit_page(page) {
                        tracing::warn!(%error, "background pane log page rejected");
                    }
                    cached.last_log_page = Some(key);
                    changed = true;
                }
            }
        }
        let view = self.daemon.as_ref().map(DesktopDaemonConnection::view);
        if let Some(view) = view {
            if self.finish_cancelled_launch(&view) {return true;}
            if self.ssh_control.is_some() {
                self.poll_ssh_control(&view);
                return true;
            }
            changed |= self.sync_termination(&view);
            changed |= self.sync_opened_sessions(&view);
            if self.closing_workspace_tab.is_some() && view.close_pending.is_none()
                && let Some(error) = &view.close_error
            {
                self.closing_workspace_tab = None;
                self.view_model.input_warning = Some(error.clone());
                changed = true;
            }
            if !view.session_opening && !view.connected && self.workspace.pending_launch.is_some() {
                self.workspace.pending_launch = None;
                self.pending_terminal_context = None;
            }
            if let Some(tab) = self.closing_workspace_tab
                && view.closed_session == self.workspace.sessions.get(&tab).copied()
            {
                let paused = !self.workspace.writable || self.workspace_connection.as_ref().is_some_and(|connection| connection.view().paused);
                if paused {
                    // Terminating a process is independent of autosave. Preserve
                    // the saved placeholder until layout editing becomes writable.
                    self.workspace.sessions.remove(&tab);
                } else {
                    self.workspace.remove(tab);
                }
                self.discard_closed_tab_view(tab);
                self.closing_workspace_tab = None;
                if let Some(next) = self.workspace.active() {
                    self.resume_workspace_tab_runtime(next);
                } else {
                    self.current_workspace_tab = None;
                    self.view_model.selected = None;
                    self.terminal_surface = Default::default();
                }
                self.refresh_workspace_tabs();
                self.ensure_metadata_connection();
                return true;
            }
            // Removing a retained session without a workspace tab must not leave
            // the previously active terminal detached from its existing process.
            if let Some(closed) = view.closed_session
                && self.closing_workspace_tab.is_none()
                && let Some(tab) = self.current_workspace_tab
                && self.workspace.sessions.get(&tab).is_some_and(|session| *session != closed)
            {
                self.resume_workspace_tab_runtime(tab);
            }
            if view.closed_session.is_some()
                && self.closing_workspace_tab.is_none()
                && self.current_workspace_tab.is_none()
                && let Some(daemon) = &self.daemon
            {
                daemon.detach_for_placeholder();
            }
            changed |= self
                .profile_panel
                .set_launch_error(view.launch_error.clone(), view.session_profile_id);
            let matching = self
                .current_workspace_tab
                .and_then(|tab| self.workspace.sessions.get(&tab))
                .copied()
                == view.session_id
                && view.session_id.is_some();
            if !matching {
                changed |= self.view_model.daemon_connected
                    || self.view_model.daemon_status_detail != view.detail;
                self.view_model.daemon_connected = false;
                self.view_model.can_reconnect_ssh = false;
                self.view_model
                    .daemon_status_detail
                    .clone_from(&view.detail);
            }
            if view.connected
                && !view.session_opening
                && view.close_pending.is_none()
                && self.workspace.writable
                && !self.workspace_connection.as_ref().is_some_and(|connection| connection.view().paused)
                && let Some(id) = view.session_id
            {
                let should_observe = self.workspace.pending_launch.is_some()
                    || self.current_workspace_tab.is_none()
                    || self
                        .current_workspace_tab
                        .and_then(|t| self.workspace.sessions.get(&t))
                        .is_some_and(|bound| *bound == id);
                if should_observe {
                    let profile = view
                        .sessions
                        .as_ref()
                        .and_then(|sessions| {
                            sessions
                                .iter()
                                .find(|s| s.session_id == id.as_uuid().as_bytes())
                        })
                        .and_then(|s| s.profile_id.as_deref())
                        .and_then(|b| <[u8; 16]>::try_from(b).ok())
                        .map(cshell_domain::ProfileId::from_bytes)
                        .or(view.session_profile_id);
                    let before = self.workspace.document.clone();
                    let previous_session = self
                        .current_workspace_tab
                        .and_then(|t| self.workspace.sessions.get(&t))
                        .copied();
                    if let Some(tab) = self.workspace.observe(
                        id,
                        profile,
                        view.session_title.unwrap_or("Terminal".into()),
                    ) {
                        self.switch_tab_view(tab);
                        if let Some(report) = self.pending_terminal_context.take()
                            && profile == Some(report.context.profile_id)
                        {
                            if !self.terminal_policy_panel.bind_launch(tab, id, report) {
                                self.view_model.input_warning =
                                    Some("新会话已打开，但临时主题绑定预算已满".into());
                            }
                        }
                        if before != self.workspace.document || previous_session != Some(id) {
                            self.refresh_workspace_tabs();
                            changed = true;
                        }
                    }
                }
            }
        }
        changed |= self.refresh_sync_input();
        changed |= self.refresh_tab_activity();
        if let Some(hosts) = &self.host_groups {
            let view = hosts.view();
            let stamp = (view.generation, view.busy);
            changed |= self.last_host_stamp != Some(stamp);
            self.last_host_stamp = Some(stamp);
        }
        if let Some(tasks) = &self.tasks {
            let view = tasks.view();
            let stamp = (view.generation, view.busy);
            changed |= self.last_task_stamp != Some(stamp);
            self.last_task_stamp = Some(stamp);
            if self.task_panel.open {
                changed |= self.task_panel.update_output(&view);
            }
        }
        changed
    }

    fn sync_blocks_input(&self) -> bool {
        self.host_panel.open
            || self.task_panel.open
            || self.sync_input.as_ref().is_some_and(|sync| {
                sync.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .blocks_input()
            })
    }

    fn submit_terminal_action(&mut self, action: InputAction) -> bool {
        self.clear_clipboard_selection();
        self.return_to_live_terminal();
        if self.host_panel.open || self.task_panel.open {
            return false;
        }
        let Some(session) = self.active_terminal_session_id() else {
            return false;
        };
        if let (Some(sync), Some(tab)) = (&self.sync_input, self.current_workspace_tab) {
            let mut sync = sync
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if sync.is_realtime() {
                let submitted = sync.route(tab, action);
                drop(sync);
                if submitted {
                    self.terminal_tools_panel
                        .record_input_omission(session, true);
                }
                return submitted;
            }
        }
        let submitted = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.send_input(action));
        if submitted {
            self.terminal_tools_panel
                .record_input_omission(session, false);
        }
        submitted
    }

    fn refresh_tab_activity(&mut self) -> bool {
        let opened: std::collections::BTreeSet<_> = self.workspace.sessions.keys().copied()
            .filter(|tab| self.workspace.window_of_tab(*tab) == Some(self.workspace.window_id)).collect();
        let viewed = if self.window_active { self.workspace.visible_tabs().into_iter().collect() } else { std::collections::BTreeSet::new() };
        for tab in &opened {
            let connection = if Some(*tab) == self.current_workspace_tab { self.daemon.as_ref() }
                else { self.pane_daemons.get(tab).or_else(|| self.sync_daemons.get(tab)) };
            if let Some(view) = connection.map(DesktopDaemonConnection::view)
                && view.session_id == self.workspace.sessions.get(tab).copied()
                && let (Some(session), Some(snapshot)) = (view.session_id, view.snapshot) {
                self.tab_activity.observe(*tab, session, &snapshot, viewed.contains(tab));
            }
        }
        let unread = self.tab_activity.refresh(&opened, &viewed);
        let changed = unread != self.view_model.unread_output_tabs;
        self.view_model.unread_output_tabs = unread;
        changed
    }

    fn refresh_sync_input(&mut self) -> bool {
        let Some(sync) = self.sync_input.as_ref().cloned() else {
            return false;
        };
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let visible = self.workspace.visible_tabs();
        let obsolete: Vec<_> = self
            .sync_daemons
            .keys()
            .copied()
            .filter(|tab| {
                !self.workspace.sessions.contains_key(tab)
                    || self.workspace.window_of_tab(*tab) != Some(self.workspace.window_id)
                    || visible.contains(tab)
                    || self.pane_daemons.contains_key(tab)
            })
            .collect();
        for tab in obsolete {
            if let Some(connection) = self.sync_daemons.remove(&tab) {
                connection.retire();
            }
        }
        for (tab, session) in &self.workspace.sessions {
            if visible.contains(tab)
                || self.workspace.window_of_tab(*tab) != Some(self.workspace.window_id)
                || self.pane_daemons.contains_key(tab)
                || self.sync_daemons.contains_key(tab)
            {
                continue;
            }
            if let Some(config) = &self.daemon_config
                && let Ok(connection) =
                    DesktopDaemonConnection::start_idle(config.clone().with_log_pages(false))
            {
                connection.attach_session(*session);
                self.sync_daemons.insert(*tab, connection);
            }
        }
        let catalog = self
            .profiles
            .as_ref()
            .and_then(|profiles| profiles.view().catalog);
        let targets = self
            .workspace
            .sessions
            .iter()
            .filter_map(|(tab, session)| {
                if self.workspace.window_of_tab(*tab) != Some(self.workspace.window_id) {
                    return None;
                }
                let binding = self
                    .workspace
                    .document
                    .bindings
                    .iter()
                    .find(|binding| binding.tab_id == *tab)?;
                let group = self
                    .workspace
                    .document
                    .tab_groups
                    .iter()
                    .find(|group| group.tabs.contains(tab))?;
                let kind = match binding.profile_id {
                    None => cshell_application::SyncInputTargetKind::Local,
                    Some(id) => match catalog
                        .as_ref()?
                        .profiles
                        .iter()
                        .find(|profile| profile.id == id)?
                        .kind
                    {
                        cshell_domain::ProfileKind::Local => {
                            cshell_application::SyncInputTargetKind::Local
                        }
                        cshell_domain::ProfileKind::Ssh => {
                            cshell_application::SyncInputTargetKind::Ssh
                        }
                    },
                };
                let connection = if self.current_workspace_tab == Some(*tab) {
                    self.daemon.as_ref()
                } else {
                    self.pane_daemons
                        .get(tab)
                        .or_else(|| self.sync_daemons.get(tab))
                };
                Some(sync_input::DesktopSyncTarget {
                    opened: cshell_application::OpenedSyncInputTarget {
                        tab_id: *tab,
                        session_id: *session,
                        window_id: self.workspace.window_id,
                        group_id: group.id,
                        kind,
                        title: binding.title.clone(),
                        eligibility: cshell_application::SyncInputEligibility::Disconnected,
                    },
                    sender: connection.map(DesktopDaemonConnection::input_sender),
                    control_error: connection
                        .and_then(|connection| connection.view().control_error),
                    configured_endpoint: binding.profile_id.and_then(|id| self.view_model.saved_ssh_profiles.iter().find(|profile| profile.id == id)).map(|profile| profile.target.clone()),
                })
            })
            .collect();
        sync.replace_window(self.workspace.window_id, targets);
        self.view_model.input_target_tabs = sync.target_badges();
        let stamp = sync.view_stamp(self.current_workspace_tab);
        let changed = self.last_sync_view.as_ref() != Some(&stamp);
        self.last_sync_view = Some(stamp);
        changed
    }
}

impl LogReflowWorker {
    fn start() -> Result<Self, std::io::Error> {
        let (request_sender, request_receiver) = std::sync::mpsc::channel::<TabReflowRequest>();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("cshell-log-reflow".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    if result_sender
                        .send(TabReflowResult {
                            tab_id: request.tab_id,
                            result: request.request.execute(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests: Some(request_sender),
            results: result_receiver,
            worker: Some(worker),
        })
    }

    fn submit(&self, request: LogReflowRequest, tab_id: Option<cshell_domain::TabId>) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|sender| sender.send(TabReflowRequest { tab_id, request }).is_ok())
    }
}

impl Drop for LogReflowWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _result = worker.join();
        }
    }
}

impl ApplicationHandler<DesktopEvent> for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window_number = self
            .workspace
            .document
            .windows
            .iter()
            .position(|w| w.id == self.workspace.window_id)
            .map_or(1, |index| index + 1);
        match event_loop.create_window(
            WindowAttributes::default()
                .with_title(format!("CShell - Window {window_number}"))
                .with_window_icon(app_icon::window_icon())
                .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0))
                .with_min_inner_size(winit::dpi::LogicalSize::new(900.0, 600.0))
                .with_theme(Some(winit::window::Theme::Dark))
                .with_visible(false),
        ) {
            Ok(window) => {
                let window = Arc::new(window);
                self.window_active = true;
                info!(window_id = ?window.id(), "desktop shell created");
                ui_fonts::install(&self.egui_context);
                let mut egui_state = egui_winit::State::new(
                    self.egui_context.clone(),
                    egui::ViewportId::ROOT,
                    window.as_ref(),
                    Some(window.scale_factor() as f32),
                    window.theme(),
                    None,
                );
                if let Some(proxy) = &self.accesskit_proxy {
                    egui_state.init_accesskit(event_loop, &window, proxy.clone());
                }
                self.egui_state = Some(egui_state);
                cshell_ui::install_workbench_style(&self.egui_context);
                self.last_renderer_attempt = Some(std::time::Instant::now());
                match initialize_renderer(Arc::clone(&window), false) {
                    Ok(renderer) => self.renderer = Some(renderer),
                    Err(error) => tracing::error!(%error, "cannot initialize terminal renderer"),
                }
                window.set_visible(true);
                window.request_redraw();
                self.window = Some(window);
            }
            Err(error) => {
                tracing::error!(%error, "cannot create desktop window");
                if !self.secondary_window {
                    event_loop.exit();
                }
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: DesktopEvent) {
        let Some(window) = &self.window else {
            return;
        };
        let DesktopEvent::AccessKit(event) = event;
        if event.window_id != window.id() {
            return;
        }
        match event.window_event {
            egui_winit::accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.egui_context.enable_accesskit();
            }
            egui_winit::accesskit_winit::WindowEvent::ActionRequested(request) => {
                if request.action == egui::accesskit::Action::Focus
                    && request.target_node
                        == terminal_accessibility::terminal_node_id().accesskit_id()
                    && (self.log_surface.is_none() || self.terminal_scrollback)
                    && self.pending_paste.is_none()
                    && !self.terminal_tools_panel.open
                    && !self.profile_batch_panel.open
                    && !self.terminal_policy_panel.open
                    && !self.workspace_recovery_panel.open
                    && !self.view_model.about_open
                    && !self.view_model.confirm_terminate
                    && !self.view_model.tab_navigator.open
                {
                    self.egui_context.memory_mut(|memory| {
                        if let Some(focused) = memory.focused() {
                            memory.surrender_focus(focused);
                        }
                    });
                } else if let Some(state) = &mut self.egui_state {
                    state.on_accesskit_action_request(request);
                }
            }
            egui_winit::accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                self.egui_context.disable_accesskit();
            }
        }
        window.request_redraw();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = std::time::Instant::now();
        let mut redraw_needed = self.sync_workspace();
        redraw_needed |= self.zmodem.poll(self.daemon_config.as_ref().or(self.workspace_config.as_ref()));
        redraw_needed |= self.terminal_upload.poll();
        redraw_needed |= self.poll_egui_repaint(now);
        if let Some(policy) = &self.clipboard_policy {
            policy.select(self.active_terminal_session_id());
            let generation = policy.generation();
            if self.last_clipboard_policy_generation != Some(generation) {
                self.last_clipboard_policy_generation = Some(generation);
                redraw_needed = true;
            }
        }
        if let Some(clipboard) = &self.clipboard {
            let selected = self.active_terminal_session_id();
            clipboard.select(selected.filter(|_| self.window_active));
            let generation = clipboard.generation();
            if self.last_clipboard_generation != Some(generation) {
                self.last_clipboard_generation = Some(generation);
                redraw_needed = true;
            }
            if let Some(error) = clipboard.take_error() {
                self.view_model.input_warning = Some(error);
            }
            if let Some(text) = clipboard.take_approved(selected, self.window_active)
                && let Some(state) = &mut self.egui_state
            {
                state.set_clipboard_text(text.to_string());
            }
        }
        if self.tunnel_panel.open {
            let generation = self.tunnel_panel.generation();
            if self.last_tunnel_generation != generation {
                self.last_tunnel_generation = generation;
                redraw_needed = true;
            }
        }
        redraw_needed |= self
            .workspace_recovery_panel
            .poll(self.workspace_connection.as_deref());
        redraw_needed |= self.terminal_policy_panel.poll(
            self.daemon_config
                .as_ref()
                .or(self.workspace_config.as_ref()),
        );
        redraw_needed |= self
            .terminal_tools_panel
            .poll(self.workspace_config.as_ref());
        while let Some(notice) = self.terminal_tools_panel.take_notice() {
            self.view_model.input_warning = Some(notice.message);
            if notice.bell
                && let Some(window) = &self.window
            {
                window.request_user_attention(Some(winit::window::UserAttentionType::Critical));
            }
            redraw_needed = true;
        }
        if !self.highlight_attempted && self.workspace_config.is_some() {
            self.highlight_attempted = true;
            self.highlight_worker = cshell_render::HighlightWorker::start().ok();
        }
        if let Some(worker) = &self.highlight_worker {
            let generation = worker.generation();
            if generation != self.last_highlight_generation {
                self.last_highlight_generation = generation;
                redraw_needed = true;
            }
        }
        if self.profile_batch_panel.open {
            let generation = self.profile_batch_panel.generation();
            if self.last_profile_batch_generation != generation {
                self.last_profile_batch_generation = generation;
                redraw_needed = true;
            }
        }
        if self.ssh_provider_panel.open {
            let generation = self.ssh_provider_panel.generation();
            if self.last_ssh_provider_generation != generation {
                self.last_ssh_provider_generation = generation;
                redraw_needed = true;
            }
        }
        if self.host_ca_panel.open {
            let generation = self.host_ca_panel.generation();
            if self.last_host_ca_generation != generation {
                self.last_host_ca_generation = generation;
                redraw_needed = true;
            }
        }
        if self.keyboard_auth.sync(self.daemon_config.as_ref().or(self.workspace_config.as_ref())) {
            redraw_needed=true;
            if let Some(window)=&self.sftp_window {window.window.request_redraw();}
        }
        self.keyboard_auth.refresh_profiles(self.profiles.as_ref());
        self.sync_sftp_lifecycle();
        self.sync_sftp_window(event_loop);
        if self.sftp_panel.open {
            let generation = self.sftp.as_ref().map(DesktopSftpConnection::generation);
            if self.last_sftp_generation != generation {
                self.last_sftp_generation = generation;
                if let Some(window) = &self.sftp_window { window.window.request_redraw(); }
            }
            if let Some(window) = &mut self.sftp_window { window.poll(now); }
        }
        while let Some(completed) = self
            .log_reflow
            .as_ref()
            .and_then(|worker| worker.results.try_recv().ok())
        {
            redraw_needed |= self.apply_log_reflow(completed);
        }
        redraw_needed |= self.poll_terminal_search();
        if let Some(view) = self.profiles.as_ref().map(DesktopProfileConnection::view) {
            let status=view.vault.as_ref().map(|v| if v.manually_locked {"凭据：手动锁定"}else if !v.exists || v.system {"凭据：系统托管"}else if v.unlocked {"凭据：已解锁"}else{"凭据：需主密码"}.to_owned());
            if self.view_model.vault_status!=status {self.view_model.vault_status=status;redraw_needed=true;}

            redraw_needed |= self.update_profile_themes(&view);
            redraw_needed |= self.profile_panel.sync(&view);
            if self.saved_profiles_generation != Some(view.generation) {
                self.saved_profiles_generation = Some(view.generation);
                self.view_model.profile_folders = view
                    .catalog
                    .as_ref()
                    .map_or_else(Vec::new, |c| c.folders.clone());
                let mut saved_ssh_profiles: Vec<_> =
                    view.catalog.as_ref().map_or_else(Vec::new, |catalog| {
                        catalog
                            .profiles
                            .iter()
                            .filter_map(|profile| {
                                let target = match profile.kind {
                                    cshell_domain::ProfileKind::Ssh => {
                                        let connection = catalog
                                            .ssh_connections
                                            .iter()
                                            .find(|c| c.profile_id == profile.id)?;
                                        ssh_session_controls::configured_target(connection)
                                    }
                                    cshell_domain::ProfileKind::Local => "本地终端".to_owned(),
                                };
                                Some(cshell_ui::SavedSshProfileViewModel {
                                    id: profile.id,
                                    name: profile.name.clone(),
                                    target,
                                    favorite: profile.favorite,
                                    folder_id: profile.folder_id,
                                    kind: profile.kind,
                                })
                            })
                            .collect()
                    });
                saved_ssh_profiles.sort_by(|left, right| {
                    right
                        .favorite
                        .cmp(&left.favorite)
                        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                });
                redraw_needed |= self.view_model.saved_ssh_profiles != saved_ssh_profiles;
                self.view_model.saved_ssh_profiles = saved_ssh_profiles;
                let environments=view.catalog.as_ref().map_or_else(Default::default,|c|c.profiles.iter().filter_map(|p|cshell_ui::environment::Environment::from_tags(p.tags.iter().map(String::as_str)).map(|env|(p.id,env))).collect());
                redraw_needed|=self.view_model.profile_environments!=environments;
                self.view_model.profile_environments=environments;
                if self.view_model.selected_profile.is_some_and(|id| {
                    !self
                        .view_model
                        .saved_ssh_profiles
                        .iter()
                        .any(|p| p.id == id)
                }) {
                    self.view_model.selected_profile = None;
                }
            }
        }
        redraw_needed |= self.poll_primary_terminal(now);
        if self.log_surface.is_some() {
            redraw_needed |= self.cursor_blink.synchronize(None, false, now);
        }
        let current_generation = self.terminal_clipboard.snapshot.as_ref()
            .or_else(|| self.terminal_surface.latest_snapshot())
            .map(|snapshot| snapshot.generation);
        if self.terminal_search_open
            && self.log_surface.is_none()
            && !self.terminal_search_query.is_empty()
            && current_generation != self.terminal_search_requested_generation
        {
            self.schedule_terminal_search(false);
        }
        if self.renderer.is_none()
            && self
                .last_renderer_attempt
                .is_none_or(|attempt| attempt.elapsed() >= std::time::Duration::from_secs(2))
            && let Some(window) = &self.window
        {
            self.last_renderer_attempt = Some(std::time::Instant::now());
            match initialize_renderer(Arc::clone(window), self.renderer_recovering) {
                Ok(renderer) => {
                    self.renderer_needs_font_upload = true;
                    if self.renderer_recovering
                        && let Some(e2e) = &mut self.window_e2e
                    {
                        e2e.note_renderer_recovered(renderer.is_cpu_adapter());
                    }
                    self.renderer_recovering = false;
                    self.renderer = Some(renderer);
                    redraw_needed = true;
                }
                Err(error) => tracing::error!(%error, "terminal renderer recovery failed"),
            }
        }
        self.refresh_history_drag(self.terminal_selecting);
        redraw_needed |= self.tick_selection_autoscroll();
        if redraw_needed && let Some(window) = &self.window {
            window.request_redraw();
        }
        if self
            .window_e2e
            .as_mut()
            .is_some_and(window_e2e::WindowE2e::should_exit)
        {
            event_loop.exit();
        }
        if self.window_e2e.is_some()
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
        let poll_interval = if self.view_model.daemon_connected {
            std::time::Duration::from_millis(16)
        } else {
            std::time::Duration::from_millis(100)
        };
        let poll_deadline = now + poll_interval;
        let wake_deadline = self.next_wake_deadline(poll_deadline);
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake_deadline));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        // Release local pointer ownership even if a modal or remote mouse
        // handler consumes the event later in this function.
        self.handle_clipboard_lifecycle(&event);
        if !self.window_active
            && matches!(
                &event,
                WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_)
            )
        {
            return;
        }
        let modal_input_blocked = (self.profile_panel.is_dialog_open() || self.zmodem.blocks_input() || self.keyboard_auth.blocks_input())
            || self.tunnel_panel.open
            || self.host_ca_panel.open
            || self.terminal_tools_panel.open
            || self.profile_batch_panel.open
            || self.terminal_policy_panel.open
            || self.workspace_recovery_panel.open
            || self.ssh_provider_panel.open
            || self.host_panel.open
            || self.task_panel.open
            || self.view_model.about_open
            || self.view_model.confirm_terminate
            || self.view_model.tab_navigator.open
            || self.command_center.blocks_input();
        if let Some(sync) = &self.sync_input { sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner).set_modal_blocked(modal_input_blocked); }
        if !modal_input_blocked && self.pending_paste.is_none()
            && let WindowEvent::KeyboardInput { event, is_synthetic: false, .. } = &event
            && (matches!(&event.logical_key,Key::Character(key) if key.eq_ignore_ascii_case("k")) || (!self.terminal_search_open && !self.egui_context.egui_wants_keyboard_input() && !self.sync_blocks_input()))
            && self.route_workbench_shortcut(event) {
            if let Some(sync)=&self.sync_input {sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner).set_modal_blocked(true);}
            window.request_redraw();
            return;
        }
        if let WindowEvent::KeyboardInput {event,is_synthetic:false,..}=&event
            && (self.view_model.tab_navigator.open || self.tab_navigation_available())
            && workbench_navigation::shortcut(event.state,&event.logical_key,self.modifiers,"p") {
            if !event.repeat && !self.view_model.tab_navigator.open {
                self.view_model.tab_navigator.show(self.current_workspace_tab);
                if let Some(sync)=&self.sync_input {sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_modal_blocked(true);}
            }
            window.request_redraw();
            return;
        }
        if !self.command_center.blocks_input() && matches!(&event, WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. })
            && let Some(position) = self.cursor_position {
            let scale = window.scale_factor() as f32;
            let point = egui::pos2(position.x as f32 / scale, position.y as f32 / scale);
            if self.view_model.pane_views.iter().any(|pane| pane.rect.contains(point))
                && let Some(sync) = &self.sync_input
                && sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner).focus_terminal() {
                self.egui_context.memory_mut(|memory| { if let Some(id) = memory.focused() { memory.surrender_focus(id); } });
            }
        }
        // egui-winit consumes Tab even without a focused widget. A terminal Tab
        // must bypass egui entirely, or it also focuses the first toolbar button.
        let terminal_tab = matches!(&event, WindowEvent::KeyboardInput { event, is_synthetic: false, .. }
            if terminal_tab_bypasses_egui(&event.logical_key,
                self.terminal_keyboard_available(), self.egui_context.egui_wants_keyboard_input()));
        // The transparent CentralPanel is an egui area too. Its generic
        // `consumed` flag cannot decide whether the GPU terminal owns a wheel.
        let terminal_wheel = !self.command_center.blocks_input() && matches!(&event, WindowEvent::MouseWheel { .. })
            && self.terminal_wheel_available(window.scale_factor() as f32);
        let terminal_mouse = !self.command_center.blocks_input() && matches!(&event, WindowEvent::MouseInput { .. })
            && self.clipboard_pointer_available(window.scale_factor() as f32);
        let terminal_history_key = matches!(&event, WindowEvent::KeyboardInput { event, is_synthetic: false, .. }
            if self.terminal_keyboard_available() && !self.egui_context.egui_wants_keyboard_input()
                && terminal_scrollback::history_shortcut(event, self.modifiers,
                    self.terminal_scrollback || self.terminal_clipboard.snapshot.is_some()).is_some());
        let mut egui_consumed = false;
        if !terminal_tab && !terminal_history_key && !terminal_wheel
            && !interaction_regressions::intercepts_native_press(&event, terminal_mouse)
            && let Some(state) = &mut self.egui_state {
            let response = state.on_window_event(&window, &event);
            egui_consumed = response.consumed;
            // RedrawRequested is already being serviced. Re-requesting it here
            // creates a WM_PAINT loop that can starve other Windows windows.
            // Further egui frames use the deadline from FullOutput instead.
            if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
                window.request_redraw();
            }
        }
        if self.terminal_clipboard.menu.is_some() && let WindowEvent::KeyboardInput { event, .. } = &event {
            if is_terminal_copy_shortcut(event, self.modifiers) {
                self.dismiss_clipboard_menu();
                self.copy_terminal_selection();
            } else if is_terminal_paste_shortcut(event, self.modifiers) {
                self.dismiss_clipboard_menu();
                self.paste_from_clipboard(&window, false);
            } else if event.state == ElementState::Pressed && event.logical_key == Key::Named(NamedKey::Escape) {
                self.dismiss_clipboard_menu();
            }
            window.request_redraw();
            return;
        }
        if self.command_center.blocks_input() && matches!(&event,
            WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_) | WindowEvent::MouseInput { .. }
            | WindowEvent::MouseWheel { .. } | WindowEvent::CursorMoved { .. }) {
            window.request_redraw();
            return;
        }
        if !egui_consumed
            && !self.task_panel.open
            && self.pending_paste.is_none()
            && !self.terminal_tools_panel.open
            && !self.profile_batch_panel.open
            && !self.terminal_policy_panel.open
            && !self.workspace_recovery_panel.open
            && !self.view_model.about_open
            && !self.view_model.confirm_terminate
            && !self.view_model.tab_navigator.open
            && matches!(
                event,
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left | MouseButton::Right | MouseButton::Middle,
                    ..
                } | WindowEvent::MouseWheel { .. }
            )
            && let Some(position) = self.cursor_position
        {
            let scale = window.scale_factor() as f32;
            let logical = egui::pos2(position.x as f32 / scale, position.y as f32 / scale);
            if let Some(pane) = self
                .view_model
                .pane_views
                .iter()
                .find(|p| p.rect.contains(logical))
                .cloned()
                && let Some(tab) = pane.tab_id
                && Some(tab) != self.current_workspace_tab
            {
                self.activate_workspace_tab(tab);
                let viewport = TerminalViewport::from_logical_rect(pane.rect, scale);
                self.terminal_viewport = Some(viewport);
                if let Some(renderer) = &self.renderer {
                    self.viewport_rows = renderer.viewport_rows(viewport);
                }
            }
        }
        if let WindowEvent::CursorMoved { position, .. } = &event { self.cursor_position = Some(*position); }
        if self.route_terminal_zoom(&event,&window) { window.request_redraw(); return; }
        if self.route_remote_mouse(&event, &window) { window.request_redraw(); return; }
        match event {
            WindowEvent::CloseRequested => {
                if let Some(sync) = &self.sync_input {
                    sync.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .disarm(cshell_application::SyncInputDisarmReason::Shutdown);
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.clear_clipboard_selection();
                if let Some(e2e) = &mut self.window_e2e {
                    e2e.note_resize(size);
                }
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.clear_clipboard_selection();
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(window.inner_size());
                }
                window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some(position);
                if self.terminal_selecting && let Some(point) = self.terminal_point_at(position) {
                    self.update_clipboard_selection(point);
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                if state == ElementState::Released {
                    self.finish_clipboard_selection();
                    window.request_redraw();
                } else if terminal_mouse && !egui_consumed && let Some(position) = self.cursor_position
                    && let Some(point) = self.terminal_point_at(position) {
                    self.egui_context.memory_mut(|m| { if let Some(id) = m.focused() { m.surrender_focus(id); } });
                    if !self.ctrl_click_terminal_link() { self.begin_clipboard_selection(point); }
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. }
                if terminal_mouse && !egui_consumed && matches!(button, MouseButton::Right | MouseButton::Middle) => {
                let settings = terminal_clipboard_settings::get();
                if button == MouseButton::Right && (!settings.right_click_paste || self.modifiers.shift_key()) {
                    if let Some(position) = self.cursor_position {
                        let scale = window.scale_factor() as f32;
                        self.open_clipboard_menu(egui::pos2(position.x as f32 / scale, position.y as f32 / scale));
                    }
                } else if (button == MouseButton::Right && settings.right_click_paste)
                    || (button == MouseButton::Middle && settings.middle_click_paste) {
                    self.egui_context.memory_mut(|m| { if let Some(id) = m.focused() { m.surrender_focus(id); } });
                    self.paste_from_clipboard(&window, false);
                }
                window.request_redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::Focused(active) => {
                self.restore_window_focus(active);
                if active { self.key_picker_focus_deferred = false; }
                if !active {
                    self.release_remote_mouse();
                    self.terminal_selecting = false;
                    self.terminal_clipboard.menu = None;
                    if let Some(sync) = &self.sync_input {
                        sync.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .focus_lost(self.current_workspace_tab);
                    }
                    self.pending_paste = None;
                    self.profile_panel.clear_secrets();
                    self.zmodem.clear_secrets();
                    if self.profile_panel.owns_native_key_picker() {
                        // An explicitly opened picker belongs to this workflow.
                        // Focus changes clear transient inputs, not the unlocked Vault.
                        self.key_picker_focus_deferred = true;
                    }
                    window.request_redraw();
                }
                if self.cursor_blink.set_window_active(
                    active && self.view_model.daemon_connected,
                    std::time::Instant::now(),
                ) {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state == ElementState::Pressed
                && self.modifiers.control_key()
                && self.modifiers.shift_key()
                && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                if let Some(sync) = &self.sync_input {
                    sync.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .disarm(cshell_application::SyncInputDisarmReason::User);
                }
                self.pending_paste = None;
                window.request_redraw();
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if !self.view_model.about_open
                && !self.view_model.confirm_terminate
                && !self.view_model.tab_navigator.open
                && !(self.profile_panel.is_dialog_open() || self.zmodem.blocks_input() || self.keyboard_auth.blocks_input())
                && !self.tunnel_panel.open
                && !self.host_ca_panel.open
                && !self.terminal_tools_panel.open
                && !self.profile_batch_panel.open
                && !self.terminal_policy_panel.open
                && !self.workspace_recovery_panel.open
                && !self.ssh_provider_panel.open
                && self.pending_paste.is_none()
                && !self.terminal_tools_panel.open
                && !self.profile_batch_panel.open
                && !self.terminal_policy_panel.open
                && !self.workspace_recovery_panel.open
                && !self.sync_blocks_input()
                && !self.egui_context.egui_wants_keyboard_input()
                && command_center::uses_default_search(&self.view_model)
                && is_terminal_search_shortcut(&event, self.modifiers) =>
            {
                self.open_terminal_search();
                window.request_redraw();
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if self.terminal_search_open
                && !self.view_model.about_open
                && !self.view_model.confirm_terminate
                && !self.view_model.tab_navigator.open
                && event.state == ElementState::Pressed
                && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                self.close_terminal_search();
                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, is_synthetic:false, .. }
                if !egui_consumed && self.terminal_keyboard_available()
                    && terminal_scrollback::history_shortcut(&event,self.modifiers,self.terminal_scrollback || self.terminal_clipboard.snapshot.is_some()).is_some() => {
                match terminal_scrollback::history_shortcut(&event,self.modifiers,self.terminal_scrollback || self.terminal_clipboard.snapshot.is_some()) {
                    Some(0)=>self.return_to_live_terminal(),
                    Some(direction)=>self.scroll_terminal_history(direction*i32::from(self.viewport_rows.saturating_sub(1).max(1))),
                    None=>{},
                }
                window.request_redraw();
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if !egui_consumed
                && !self.terminal_search_open
                && (self.log_surface.is_none() || self.terminal_scrollback)
                && self.pending_paste.is_none()
                && !self.terminal_tools_panel.open
                && !self.profile_batch_panel.open
                && !self.terminal_policy_panel.open
                && !self.workspace_recovery_panel.open
                && !self.sync_blocks_input()
                && !(self.profile_panel.is_dialog_open() || self.zmodem.blocks_input() || self.keyboard_auth.blocks_input())
                && !self.tunnel_panel.open
                && !self.host_ca_panel.open
                && !self.terminal_tools_panel.open
                && !self.profile_batch_panel.open
                && !self.terminal_policy_panel.open
                && !self.workspace_recovery_panel.open
                && !self.ssh_provider_panel.open
                && !self.view_model.about_open
                && !self.view_model.confirm_terminate
                && !self.view_model.tab_navigator.open =>
            {
                if event.state == ElementState::Pressed && event.logical_key == Key::Named(NamedKey::Escape)
                    && self.modifiers.is_empty() && self.terminal_decorations.selection.is_some() {
                    self.clear_clipboard_selection();
                    self.return_to_live_terminal();
                    window.request_redraw();
                    return;
                }
                if is_terminal_copy_shortcut(&event, self.modifiers) {
                    self.copy_terminal_selection();
                    window.request_redraw();
                    return;
                }
                if is_terminal_paste_shortcut(&event, self.modifiers) {
                    self.paste_from_clipboard(&window, false);
                    return;
                }
                let terminal_modes = self
                    .terminal_surface
                    .latest_snapshot()
                    .map_or_else(cshell_terminal::TerminalModes::default, |snapshot| {
                        snapshot.terminal_modes
                    });
                let broadcast = self.sync_input.as_ref().is_some_and(|sync| {
                    sync.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_realtime()
                });
                if let Some(action) =
                    terminal_key_action(&event, self.modifiers, terminal_modes, broadcast)
                    && self.active_terminal_session_id().is_some()
                {
                    if self.submit_terminal_action(action) {
                        if self.cursor_blink.note_activity(std::time::Instant::now()) {
                            window.request_redraw();
                        }
                    } else {
                        tracing::warn!("terminal input was not accepted by the daemon client");
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(text))
                if !egui_consumed && !self.terminal_search_open
                    && (self.log_surface.is_none() || self.terminal_scrollback)
                    && self.pending_paste.is_none()
                    && !self.terminal_tools_panel.open
                    && !self.profile_batch_panel.open
                    && !self.terminal_policy_panel.open
                    && !self.workspace_recovery_panel.open
                    && !self.sync_blocks_input()
                    && !(self.profile_panel.is_dialog_open() || self.zmodem.blocks_input() || self.keyboard_auth.blocks_input())
                    && !self.tunnel_panel.open
                    && !self.host_ca_panel.open
                    && !self.terminal_tools_panel.open
                    && !self.profile_batch_panel.open
                    && !self.terminal_policy_panel.open
                    && !self.workspace_recovery_panel.open
                    && !self.ssh_provider_panel.open
                    && !self.view_model.about_open
                    && !self.view_model.confirm_terminate
                    && !self.view_model.tab_navigator.open
                    && !text.is_empty() =>
            {
                if self.active_terminal_session_id().is_some() {
                    if self.submit_terminal_action(InputAction::Text(text)) {
                        if self.cursor_blink.note_activity(std::time::Instant::now()) {
                            window.request_redraw();
                        }
                    } else {
                        tracing::warn!("IME commit was not accepted by the daemon client");
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if terminal_wheel {
                    self.handle_terminal_wheel(delta);
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                if let Some(e2e) = &mut self.window_e2e {
                    e2e.note_redraw(self.log_surface.is_some(), self.renderer.is_some());
                }
                let Some(egui_state) = &mut self.egui_state else {
                    return;
                };
                let raw_input = egui_state.take_egui_input(&window);
                let mut terminal_rect = egui::Rect::NOTHING;
                let scrollbar_state = self
                    .log_surface
                    .as_ref()
                    .and_then(|surface| surface.scrollbar_state(self.viewport_rows.max(1)));
                let mut scrollbar_action = None;
                let mut return_to_live=false;
                self.refresh_terminal_reading();
                self.terminal_ui_regions.clear();
                let context = self.egui_context.clone();
                let mut search_changed = false;
                let mut search_navigation = 0_i8;
                let mut close_search = false;
                let mut confirm_paste = false;
                let mut cancel_paste = false;
                let mut profile_command = None;
                let mut clipboard_command = None;
                let mut clipboard_action = None;
                let sftp_session = self.active_terminal_session_id();
                let tunnel_profile = sftp_session
                    .and_then(|session| self.daemon.as_ref()?.session_profile(session))
                    .zip(self.profiles.as_ref().and_then(|p| p.catalog_revision()));
                let selected_before_draw = self.view_model.selected;
                let tab_before_draw = self.view_model.selected_tab;
                let mut return_auxiliary_focus=false;
                self.refresh_ssh_controls();
                let full_output = context.run_ui(raw_input, |ui| {
                    terminal_appearance::set_font(ui.ctx(),self.view_model.terminal_font_pixels.unwrap_or(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS));
                    self.view_model.task_view = self.task_panel.open;
                    self.view_model.layout_paused = self
                        .workspace_connection
                        .as_ref()
                        .is_some_and(|c| c.view().paused);
                    self.view_model.can_sftp = self.sftp_profile_for_open().is_some();
                    self.view_model.terminal_zoom_pixels=self.terminal_mouse.zoom;
                    cshell_ui::draw_workbench_chrome(ui, &mut self.view_model);
                    self.draw_connection_diagnostics(ui);
                    self.terminal_mouse.zoom=self.view_model.terminal_zoom_pixels;
                    if let Some(hosts) = &self.host_groups {
                        self.host_panel.draw(ui.ctx(), hosts, &hosts.view());
                    }
                    if let Some(sync) = self.sync_input.clone() {
                        let mut sync = sync
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        sync.set_modal_blocked((self.profile_panel.is_dialog_open() || self.zmodem.blocks_input() || self.keyboard_auth.blocks_input())
                            || self.tunnel_panel.open
                            || self.host_ca_panel.open
                            || self.terminal_tools_panel.open
                            || self.profile_batch_panel.open
                            || self.terminal_policy_panel.open
                            || self.workspace_recovery_panel.open
                            || self.ssh_provider_panel.open
                            || self.host_panel.open
                            || self.task_panel.open
                            || self.view_model.about_open
                            || self.view_model.confirm_terminate
                            || self.view_model.tab_navigator.open || self.command_center.blocks_input());
                        let search_requested=sync.draw(
                            ui,self.workspace.window_id,self.workspace.focused_group(),self.current_workspace_tab,self.terminal_search_open,
                        );
                        if search_requested {
                            if self.terminal_search_open {self.close_terminal_search();}
                            else {self.open_terminal_search();}
                        }
                        return_auxiliary_focus |= sync.take_focus_return();
                        if let Some((definition, selection)) =
                            sync.take_task_draft(self.workspace.window_id)
                        {
                            if self.tasks.is_some() {
                                self.pending_paste = None;
                                self.task_panel.use_template(definition, selection);
                            } else {
                                sync.status =
                                    "任务服务不可用，请恢复 daemon 连接后重新打开模板".into();
                            }
                        }
                    }
                    if self.terminal_search_open
                        && !self.task_panel.open
                        && !self.tunnel_panel.open
                        && !self.host_ca_panel.open
                        && !self.terminal_tools_panel.open
                        && !self.profile_batch_panel.open
                        && !self.terminal_policy_panel.open
                        && !self.workspace_recovery_panel.open
                        && !self.ssh_provider_panel.open
                        && !self.view_model.about_open
                        && !self.view_model.confirm_terminate
                        && !self.view_model.tab_navigator.open
                    {
                        egui::Panel::bottom("terminal-search-dock").exact_size(104.0)
                            .frame(cshell_ui::form::utility_bar()).show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    let response = ui.add(
                                        egui::TextEdit::singleline(&mut self.terminal_search_query)
                                            .desired_width((ui.available_width()-260.0).clamp(80.0,450.0))
                                            .char_limit(MAX_TERMINAL_SEARCH_QUERY_BYTES)
                                            .hint_text("查找当前终端内容"),
                                    );
                                    if self.terminal_search_focus_requested {
                                        response.request_focus();
                                        self.terminal_search_focus_requested = false;
                                    }
                                    if response.changed() {
                                        truncate_utf8_bytes(
                                            &mut self.terminal_search_query,
                                            MAX_TERMINAL_SEARCH_QUERY_BYTES,
                                        );
                                        search_changed = true;
                                    }
                                    if ui.button("上一个").clicked() {
                                        search_navigation = -1;
                                    }
                                    if ui.button("下一个").clicked() {
                                        search_navigation = 1;
                                    }
                                    if ui.button("收起查找").on_hover_text("Esc：返回终端").clicked() {
                                        close_search = true;
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    search_changed |= ui
                                        .checkbox(
                                            &mut self.terminal_search_options.case_sensitive,
                                            "区分大小写",
                                        )
                                        .changed();
                                    search_changed |= ui
                                        .checkbox(
                                            &mut self.terminal_search_options.whole_word,
                                            "全词匹配",
                                        )
                                        .changed();
                                    search_changed |= ui
                                        .checkbox(
                                            &mut self.terminal_search_options.regex,
                                            "正则表达式",
                                        )
                                        .changed();
                                    if let Some(matched) = &self.log_decorations.active_search_match
                                    {
                                        ui.label(format!("line {}", matched.line_id));
                                    } else {
                                        let count = self.terminal_decorations.search.matches.len();
                                        let active = self
                                            .terminal_decorations
                                            .active_search_match
                                            .map_or(0, |index| index + 1);
                                        ui.label(format!("{active}/{count}"));
                                    }
                                });
                                if let Some(error) = &self.terminal_search_error {
                                    ui.add(egui::Label::new(egui::RichText::new(error).color(cshell_ui::theme::DANGER)).truncate()).on_hover_text(error);
                                }
                                if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                                    search_navigation = if ui.input(|input| input.modifiers.shift) {
                                        -1
                                    } else {
                                        1
                                    };
                                }
                            });
                    }
                    self.terminal_tools_panel.draw_command_dock(ui,sftp_session);
                    if self.task_panel.open
                        && let Some(tasks) = &self.tasks
                    {
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(200));
                        let hosts = self.host_groups.as_ref().map(|hosts| hosts.view());
                        terminal_rect =
                            self.task_panel
                                .draw(ui, tasks, &tasks.view(), hosts.as_ref());
                    } else {
                        terminal_rect = cshell_ui::draw_workbench_content(ui, &mut self.view_model);
                    }
                    let pane=self.view_model.pane_views.iter().find(|p|p.tab_id==self.current_workspace_tab).map_or(terminal_rect,|p|p.rect);
                    return_to_live = self.draw_terminal_return_button(ui, pane);
                    if let Some(sync) = &self.sync_input {
                        let sync = sync
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        for pane in &self.view_model.pane_views {
                            if pane.tab_id.is_some_and(|tab| sync.targeted(tab)) {
                                ui.painter().rect_stroke(
                                    pane.rect.shrink(1.0),
                                    0.0,
                                    egui::Stroke::new(2.0, cshell_ui::theme::WARNING),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    }
                    profile_command = self.profile_panel.draw(ui.ctx());
                    let drop_rect=self.view_model.pane_views.iter().find(|p|p.tab_id==self.current_workspace_tab).map_or(terminal_rect,|p|p.rect);
                    let drop_source=sftp_session.filter(|_|self.view_model.can_disconnect_ssh);
                    let drop_blocked=self.profile_panel.is_dialog_open() || self.keyboard_auth.blocks_input() || self.zmodem.blocks_input() || self.task_panel.open || self.view_model.confirm_terminate
                    || self.view_model.tab_navigator.open;
                    let verification=drop_source.and_then(|s|self.daemon.as_ref()?.session_profile(s)).and_then(|id|self.profiles.as_ref()?.view().catalog?.ssh_connections.into_iter().find(|p|p.profile_id==id)?.options.sftp_verification);
                    self.terminal_upload.drop_target(ui,drop_rect,drop_source,self.daemon_config.as_ref().or(self.workspace_config.as_ref()),drop_blocked,verification);
                    self.terminal_upload.draw(ui.ctx());
                    if self.key_picker_focus_deferred && (!self.profile_panel.owns_native_key_picker() || !managed_keys_panel::picker_is_foreground()) {
                        self.key_picker_focus_deferred = false;
                    }
                    let owned:Vec<_>=self.workspace.sessions.iter()
                        .filter(|(tab,_)| self.workspace.window_of_tab(**tab)==Some(self.workspace.window_id))
                        .map(|(_,session)|session.as_uuid().as_bytes().to_vec())
                        .chain(self.daemon.as_ref().and_then(|d|d.view().session_id).map(|s|s.as_uuid().as_bytes().to_vec())).collect();
                    let catalog=self.profiles.as_ref().and_then(|p|p.view().catalog);
                    if let Some(profiles)=&self.profiles {self.zmodem.sync_security(&profiles.view());}
                    self.zmodem.sync_display(self.view_model.terminal_font_pixels.unwrap_or(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS));
                    self.zmodem.sync_layout(cshell_ui::workbench_layout::Layout::capture(&self.view_model));
                    self.zmodem.draw(ui.ctx(),catalog.as_ref(),&owned);
                    if self.zmodem.take_keyboard_settings_request(){self.command_center.show_settings();}
                    if let Some(pixels)=self.zmodem.take_display_request(){self.view_model.terminal_font_pixels=Some(pixels);}
                    if let Some(layout)=self.zmodem.take_layout_request(){layout.apply(&mut self.view_model);}
                    if let Some(request)=self.zmodem.take_security_request(){profile_command=Some(profile_connection::ProfileClientCommand::Vault(request));}

                    let recovery_profiles =
                        self.profiles.as_ref().map(DesktopProfileConnection::view);
                    self.workspace_recovery_panel.draw(
                        ui.ctx(),
                        self.workspace_connection.as_deref(),
                        &self.workspace.document,
                        &self.workspace.sessions,
                        recovery_profiles.as_ref(),
                    );
                    let profile_view = self.profiles.as_ref().map(DesktopProfileConnection::view);
                    let tools_profile =
                        sftp_session.and_then(|id| self.daemon.as_ref()?.session_profile(id));
                    self.terminal_policy_panel.draw(
                        ui.ctx(),
                        profile_view.as_ref(),
                        tools_profile,
                        self.current_workspace_tab,
                        sftp_session,
                    );
                    if self.profile_batch_panel.draw(
                        ui.ctx(),
                        self.daemon_config
                            .as_ref()
                            .or(self.workspace_config.as_ref()),
                        profile_view.as_ref(),
                        tools_profile,
                    ) {
                        profile_command = Some(profile_connection::ProfileClientCommand::Refresh);
                    }
                    if let Some(command) = self.terminal_tools_panel.draw(
                        ui.ctx(),
                        sftp_session,
                        tools_profile,
                        profile_view.as_ref(),
                    ) {
                        profile_command = Some(command);
                    }
                    if let Some(policy) = &mut self.clipboard_policy {
                        policy.draw(ui.ctx());
                    }
                    if let Some(clipboard) = &self.clipboard {
                        let title = self
                            .view_model
                            .sessions
                            .iter()
                            .find(|session| Some(session.id) == sftp_session)
                            .map(|session| session.title.as_str())
                            .unwrap_or("SSH");
                        clipboard_command =
                            clipboard.draw(ui.ctx(), sftp_session, self.window_active, title);
                    }
                    self.tunnel_panel.draw(
                        ui.ctx(),
                        sftp_session,
                        self.workspace_config.as_ref(),
                        tunnel_profile,
                    );
                    self.ssh_provider_panel
                        .draw(ui.ctx(), self.workspace_config.as_ref());
                    self.host_ca_panel
                        .draw(ui.ctx(), self.workspace_config.as_ref());
                    if !self.task_panel.open
                        && let Some(state) = scrollbar_state
                    {
                        scrollbar_action = self.draw_terminal_history_scrollbar(ui, &mut terminal_rect, state);
                    }
                    if self.log_surface.is_none()
                        && !self.task_panel.open
                        && !self.tunnel_panel.open
                        && !self.host_ca_panel.open
                        && !self.terminal_tools_panel.open
                        && !self.profile_batch_panel.open
                        && !self.terminal_policy_panel.open
                        && !self.workspace_recovery_panel.open
                        && !self.ssh_provider_panel.open
                        && !self.view_model.about_open
                        && !self.view_model.confirm_terminate
                        && !self.view_model.tab_navigator.open
                    {
                        terminal_accessibility::add_terminal_node(
                            ui.ctx(),
                            terminal_rect,
                            self.terminal_surface.latest_snapshot().map(AsRef::as_ref),
                            self.terminal_decorations.selection,
                        );
                    }
                    if let Some(pending) = &self.pending_paste
                        && !self.view_model.about_open
                        && !self.view_model.confirm_terminate
                        && !self.view_model.tab_navigator.open
                    {
                        if let Some(tab) = self.current_workspace_tab {
                            let endpoint = self.workspace.document.bindings.iter().find(|b| b.tab_id == tab).and_then(|b| b.profile_id)
                                .and_then(|id| self.view_model.saved_ssh_profiles.iter().find(|p| p.id == id)).map(|p| p.target.as_str());
                            match self.terminal_clipboard.review.show(ui.ctx(), self.workspace.window_id, tab,
                                pending.session_id, &pending.session_title, endpoint, self.view_model.ssh_tab_states.contains_key(&tab), &pending.text) {
                                sync_input::compose::PasteReviewDecision::Cancel => cancel_paste = true,
                                sync_input::compose::PasteReviewDecision::Send => confirm_paste = true,
                                _ => {}
                            }
                        }
                    }
                    clipboard_action = self.draw_clipboard_ui(ui.ctx());
                    self.keyboard_auth.draw(ui.ctx());
                    if self.keyboard_auth.blocks_input() || self.profile_panel.is_dialog_open() || self.pending_paste.is_some(){self.command_center.suspend();}
                    else {
                        if self.command_center.open {self.command_center.quick=self.sync_input.as_ref().map(|s|s.lock().unwrap_or_else(std::sync::PoisonError::into_inner).palette_commands()).unwrap_or_default();}
                        self.command_center.draw(ui.ctx(), &mut self.view_model);
                    }
                });
                if let Some(text)=self.terminal_tools_panel.take_command_load() {
                    let loaded=self.sync_input.as_ref().is_some_and(|sync|sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner).load_history_command(self.workspace.window_id,self.current_workspace_tab,text));
                    if !loaded{self.view_model.input_warning=Some("命令栏已有草稿或当前会话不可用；请先处理草稿后再载入。".into());}
                }
                if let Some(line)=self.terminal_tools_panel.take_command_jump() {self.jump_to_command(line);}
                self.return_focus_from_tab_navigation();
                let layout=cshell_ui::workbench_layout::Layout::capture(&self.view_model);
                if self.saved_workbench_layout.as_ref()!=Some(&layout) && !context.input(|i|i.pointer.any_down()) {
                    self.saved_workbench_layout=Some(layout.clone());
                    if let Some(path)=&self.workbench_settings_path {
                        if let Err(error)=workbench_settings::save(path,&layout){self.view_model.input_warning=Some(format!("无法保存工作台布局：{error}"));}
                    }
                }
                if return_auxiliary_focus && self.terminal_keyboard_available() {
                    self.egui_context.memory_mut(|m|{if let Some(id)=m.focused(){m.surrender_focus(id);}});
                }
                if self
                    .workspace_connection
                    .as_ref()
                    .is_some_and(|c| c.view().paused)
                {
                    self.view_model.selected_tab = tab_before_draw;
                    self.view_model.selected = selected_before_draw;
                }
                if !self
                    .workspace_connection
                    .as_ref()
                    .is_some_and(|c| c.view().paused)
                    && self.view_model.selected_tab != tab_before_draw
                    && let Some(tab) = self.view_model.selected_tab
                {
                    self.activate_workspace_tab(tab);
                } else if self.view_model.selected != selected_before_draw
                    && let Some(id) = self.view_model.selected
                {
                    if self.workspace.pending_launch.is_some()
                        || self
                            .workspace_connection
                            .as_ref()
                            .is_some_and(|c| c.view().paused)
                    {
                        self.view_model.selected = selected_before_draw;
                    } else {
                        let existing = self
                            .workspace
                            .sessions
                            .iter()
                            .find(|(tab, value)| {
                                **value == id
                                    && self.workspace.window_of_tab(**tab)
                                        == Some(self.workspace.window_id)
                            })
                            .map(|(tab, _)| *tab);
                        let tab = existing.or_else(|| {
                            let summary = self
                                .daemon
                                .as_ref()?
                                .view()
                                .sessions?
                                .into_iter()
                                .find(|s| s.session_id == id.as_uuid().as_bytes())?;
                            let profile = summary
                                .profile_id
                                .as_deref()
                                .and_then(|b| <[u8; 16]>::try_from(b).ok())
                                .map(cshell_domain::ProfileId::from_bytes);
                            let tab = self.workspace.add(profile, summary.title)?;
                            self.workspace.sessions.insert(tab, id);
                            Some(tab)
                        });
                        if let Some(tab) = tab {
                            self.activate_workspace_tab(tab);
                        }
                    }
                }
                if self.view_model.about_open {
                    self.terminal_selecting = false;
        self.terminal_clipboard = terminal_clipboard::State::default();
                }
                let paused = self
                    .workspace_connection
                    .as_ref()
                    .is_some_and(|c| c.view().paused);
                let menu = self.view_model.menu_command.take().filter(|command| {
                    !paused
                        || !matches!(
                            command,
                            WorkbenchMenuCommand::NewWorkspaceTab
                                | WorkbenchMenuCommand::CloneWorkspaceSession(_)
                                | WorkbenchMenuCommand::CloneSshChannel(_)
                                | WorkbenchMenuCommand::NewWorkspaceWindow
                                | WorkbenchMenuCommand::MoveTabToWindow(..)
                                | WorkbenchMenuCommand::OpenWorkspaceTab(_)
                                | WorkbenchMenuCommand::CloseWorkspaceTab(_)
                                | WorkbenchMenuCommand::MoveWorkspaceTab(..)
                                | WorkbenchMenuCommand::MoveTabToGroup(..)
                                | WorkbenchMenuCommand::SplitWorkspace(_)
                                | WorkbenchMenuCommand::ResizeSplit(..)
                                | WorkbenchMenuCommand::ReconnectNewShell
                                | WorkbenchMenuCommand::OpenSavedProfile(_)
                                | WorkbenchMenuCommand::DetachWorkspaceGroup
                                | WorkbenchMenuCommand::CloseView
                        )
                });
                match menu {
                    Some(WorkbenchMenuCommand::CommandCenter) => {self.release_remote_mouse();self.dismiss_clipboard_menu();self.command_center.show();window.request_redraw();},
                    Some(WorkbenchMenuCommand::KeyboardSettings) => {self.release_remote_mouse();self.dismiss_clipboard_menu();self.command_center.show_settings();window.request_redraw();},
                    Some(WorkbenchMenuCommand::TerminalView) => {
                        self.return_to_live_terminal();
                        self.task_panel.close();
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::Hosts) => {
                        self.pending_paste = None;
                        self.host_panel.open = true;
                        self.task_panel.close();
                        window.request_redraw();
                    }
                    Some(
                        command @ (WorkbenchMenuCommand::Tasks
                        | WorkbenchMenuCommand::UploadTask
                        | WorkbenchMenuCommand::DownloadTask
                        | WorkbenchMenuCommand::MixedTask),
                    ) => {
                        if self.tasks.is_some() {
                            self.pending_paste = None;
                            let selection = self
                                .host_panel
                                .open
                                .then(|| self.host_panel.selection())
                                .flatten();
                            self.host_panel.open = false;
                            match (command, selection) {
                                (WorkbenchMenuCommand::Tasks, Some(selection)) => {
                                    self.task_panel.use_selection(selection)
                                }
                                (WorkbenchMenuCommand::UploadTask, Some(selection)) => {
                                    self.task_panel.use_upload_selection(selection)
                                }
                                (WorkbenchMenuCommand::DownloadTask, Some(selection)) => {
                                    self.task_panel.use_download_selection(selection)
                                }
                                (WorkbenchMenuCommand::MixedTask, Some(selection)) => {
                                    self.task_panel.use_mixed_selection(selection)
                                }
                                (WorkbenchMenuCommand::Tasks, None) => {
                                    self.task_panel.open_commands()
                                }
                                (WorkbenchMenuCommand::UploadTask, None) => {
                                    self.task_panel.open_upload()
                                }
                                (WorkbenchMenuCommand::DownloadTask, None) => {
                                    self.task_panel.open_download()
                                }
                                _ => self.task_panel.open_mixed(),
                            }
                            window.request_redraw();
                        }
                    }
                    Some(WorkbenchMenuCommand::SyncInput) => {
                        self.pending_paste = None;
                        if let Some(sync) = &self.sync_input {
                            sync.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .panel_open = true;
                        }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::QuickCommands) => {
                        self.close_terminal_search();
                        if let Some(sync)=&self.sync_input {sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                            .open_quick_commands(self.workspace.window_id,self.current_workspace_tab);}
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::LoadQuickCommand(id))=>{
                        let loaded=self.sync_input.as_ref().is_some_and(|sync|sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner).load_palette_command(self.workspace.window_id,self.current_workspace_tab,id));
                        if loaded {self.close_terminal_search();}else{self.view_model.input_warning=Some("快捷命令未载入：请先处理命令栏草稿，或刷新快捷命令列表。".into());}window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::Compose) => {
                        self.close_terminal_search();
                        self.pending_paste = None;
                        if let Some(sync) = &self.sync_input {
                            sync.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .open_compose(self.workspace.window_id, self.current_workspace_tab);
                        }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::ShellHistory) => {self.terminal_tools_panel.open_commands();window.request_redraw();}
                    Some(WorkbenchMenuCommand::SearchTerminal) => {
                        self.open_terminal_search();
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::Profiles) => {
                        self.profile_panel.open = true;
                        if let Some(profiles) = &self.profiles {
                            profiles.request(profile_connection::ProfileClientCommand::Refresh);
                        }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::SftpFiles) => {
                        if let Some(profile)=self.sftp_profile_for_open() {self.open_sftp_profile(profile);}
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::OpenSavedSftp(profile)) => {
                        self.open_sftp_profile(profile);
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::GlobalSettings) => {self.zmodem.open_defaults(false);if let Some(profiles)=&self.profiles {profiles.request(profile_connection::ProfileClientCommand::Vault(cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::Status)));}window.request_redraw();}
                    Some(WorkbenchMenuCommand::VaultSettings) => {self.zmodem.open_defaults(true);if let Some(profiles)=&self.profiles {profiles.request(profile_connection::ProfileClientCommand::Vault(cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::Status)));}window.request_redraw();}
                    Some(WorkbenchMenuCommand::Credentials) => {
                        self.pending_paste = None;
                        let command = self.profile_panel.open_credentials();
                        if let Some(profiles) = &self.profiles { profiles.request(command); }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::TerminalUploads) => {self.terminal_upload.open=true;window.request_redraw();}
                    Some(WorkbenchMenuCommand::ZmodemTransfers) => {self.zmodem.open=true;window.request_redraw();}
                    Some(WorkbenchMenuCommand::Tunnels) => {
                        self.pending_paste = None;
                        self.tunnel_panel.open = true;
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::ProfileBatch) => {
                        self.pending_paste = None;
                        self.profile_batch_panel.open = true;
                        window.set_ime_allowed(false);
                        if let Some(profiles) = &self.profiles {
                            profiles.request(profile_connection::ProfileClientCommand::Refresh);
                        }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::WorkspaceRecovery) => {
                        if self.workspace_loaded
                            && !self.workspace_runtime_busy
                            && self.workspace.pending_launch.is_none()
                        {
                            self.workspace_layout_error = None;
                            if let Some(c) = &self.workspace_connection {
                                c.pause_reload();
                            }
                            self.workspace_recovery_panel.open = true;
                            self.pending_paste = None;
                            window.set_ime_allowed(false);
                            window.request_redraw();
                        } else {
                            self.view_model.input_warning = Some(
                                "请等待所有窗口的会话创建/视图关闭完成，再进入工作区恢复".into(),
                            );
                        }
                    }
                    Some(WorkbenchMenuCommand::RetryWorkspaceWindows) => {
                        self.retry_workspace_windows = true;
                    }
                    Some(WorkbenchMenuCommand::DetachWorkspaceGroup) => {
                        if !self.workspace_runtime_busy
                            && let Some((_window, tabs)) = self.workspace.detach_group()
                        {
                            self.transfer_group_to = Some(tabs);
                            self.refresh_workspace_tabs();
                        }
                    }
                    Some(WorkbenchMenuCommand::TerminalPolicies) => {
                        self.pending_paste = None;
                        self.terminal_policy_panel.open = true;
                        window.set_ime_allowed(false);
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::TerminalTools) => {
                        self.pending_paste = None;
                        self.terminal_tools_panel.open = true;
                        window.set_ime_allowed(false);
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::SshProviders) => {
                        self.pending_paste = None;
                        self.ssh_provider_panel.open = true;
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::HostCa) => {
                        self.pending_paste = None;
                        self.host_ca_panel.open = true;
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::ClipboardPolicy) => {
                        if let Some(policy) = &mut self.clipboard_policy {
                            policy.open = true;
                            policy.refresh();
                        }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::SshConnections) => {
                        self.profile_panel.open_ssh_connections();
                        if let Some(profiles) = &self.profiles {
                            profiles.request(profile_connection::ProfileClientCommand::Refresh);
                        }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::NewSshProfile) => {
                        self.profile_panel.open_new_ssh_profile();
                        if let Some(profiles) = &self.profiles {
                            profiles.request(profile_connection::ProfileClientCommand::Refresh);
                        }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::OpenSavedProfile(id)) => {
                        self.task_panel.close();
                        profile_command =
                            Some(profile_connection::ProfileClientCommand::OpenProfile(id));
                    }
                    Some(WorkbenchMenuCommand::EditSavedProfile(id)) => {
                        if self.profile_panel.open_profile_editor(id) {
                            window.request_redraw();
                        }
                    }
                    Some(WorkbenchMenuCommand::Quit) => event_loop.exit(),
                    Some(WorkbenchMenuCommand::DisconnectSsh) => {
                        self.request_ssh_control(false);
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::ReconnectSshTab(tab)) => {
                        self.activate_workspace_tab(tab);
                        if self.current_workspace_tab == Some(tab) { self.request_ssh_control(true); }
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::ReconnectSsh | WorkbenchMenuCommand::ReconnectNewShell) => {
                        self.request_ssh_control(true);
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::TerminateSession(id)) => {
                        self.request_termination(id);
                        window.request_redraw();
                    }
                    Some(WorkbenchMenuCommand::CloseView) => {
                        if let Some(tab) = self.current_workspace_tab {
                            self.close_workspace_tab(tab);
                            window.request_redraw();
                        }
                    }
                    Some(WorkbenchMenuCommand::NewWorkspaceWindow) => {
                        if self.workspace_loaded
                            && self.workspace.writable
                            && self.workspace.new_window().is_some()
                        {
                            self.refresh_workspace_tabs();
                        }
                    }
                    Some(WorkbenchMenuCommand::MoveTabToWindow(tab, target)) => {
                        if self.workspace_loaded
                            && self.workspace.writable
                            && self.workspace.move_to_window(tab, target)
                        {
                            self.transfer_to = Some((tab, target));
                            self.refresh_workspace_tabs();
                        }
                    }
                    Some(WorkbenchMenuCommand::SplitWorkspace(axis)) => {
                        if self.workspace_loaded
                            && self.workspace.pending_launch.is_none()
                            && let Some(tab) = self.workspace.split(axis)
                        {
                            self.activate_workspace_tab(tab);
                        }
                    }
                    Some(WorkbenchMenuCommand::MoveTabToGroup(tab, group)) => {
                        if self.workspace.pending_launch.is_none() {
                            self.workspace.move_to_group(tab, group);
                            self.activate_workspace_tab(tab);
                        }
                    }
                    Some(WorkbenchMenuCommand::ResizeSplit(pane, ratio)) => {
                        self.workspace.set_ratio(pane, ratio);
                        self.refresh_workspace_tabs();
                    }
                    Some(WorkbenchMenuCommand::NewWorkspaceTab) => {
                        if self.workspace_loaded
                            && self.workspace.pending_launch.is_none()
                            && let Some(tab) = self.workspace.add(None, "Local shell".into())
                        {
                            self.switch_tab_view(tab);
                            self.view_model.selected = None;
                            self.workspace.pending_launch = Some(tab);
                            if !self
                                .daemon
                                .as_ref()
                                .is_some_and(DesktopDaemonConnection::new_default_shell)
                            {
                                self.workspace.pending_launch = None;
                            }
                            self.refresh_workspace_tabs();
                        }
                    }
                    Some(WorkbenchMenuCommand::CloneWorkspaceSession(source)) => self.clone_terminal_tab(source,false),
                    Some(WorkbenchMenuCommand::CloneSshChannel(source)) => self.clone_terminal_tab(source,true),
                    Some(WorkbenchMenuCommand::OpenWorkspaceTab(tab)) => {
                        if self.workspace.pending_launch.is_none()
                            && let Some(binding) = self
                                .workspace
                                .document
                                .bindings
                                .iter()
                                .find(|b| b.tab_id == tab)
                                .cloned()
                        {
                            self.workspace.pending_launch = Some(tab);
                            let accepted = self.daemon.as_ref().is_some_and(|daemon| {
                                binding.profile_id.map_or_else(
                                    || daemon.new_default_shell(),
                                    |id| daemon.open_profile(id),
                                )
                            });
                            if !accepted {
                                self.workspace.pending_launch = None;
                            }
                        }
                    }
                    Some(WorkbenchMenuCommand::CloseWorkspaceTab(tab)) => {
                        self.close_workspace_tab(tab);
                    }
                    Some(WorkbenchMenuCommand::MoveWorkspaceTab(tab, delta))
                        if self.workspace.pending_launch.is_none() =>
                    {
                        self.workspace.move_tab(tab, delta);
                        self.refresh_workspace_tabs();
                    }
                    Some(WorkbenchMenuCommand::MoveWorkspaceTab(_, _)) => {}
                    None => {}
                }
                if let Some(report) = self.terminal_policy_panel.take_launch() {
                    // The normal workspace launch lifecycle owns the new tab and
                    // prevents switching another launch into its pending slot.
                    if self.prepare_profile_tab(report.context.profile_id)
                        && let Some(daemon) = &self.daemon
                    {
                        if daemon.open_terminal_context(report.clone()) {
                            self.pending_terminal_context = Some(report);
                            self.pending_tab = None;
                            self.terminal_surface = TerminalSurfaceModel::default();
                            self.terminal_decorations = TerminalDecorations::default();
                            self.terminal_clipboard = terminal_clipboard::State::default();
                            self.log_surface = None;
                    self.terminal_scrollback = false;
                        } else {
                            self.workspace.pending_launch = None;
                        }
                    }
                }
                if let Some(profile)=self.profile_panel.take_cancelled_connection() {self.dismiss_failed_connection(profile);}
                if let Some(command) = profile_command {
                    match command {
                        profile_connection::ProfileClientCommand::OpenProfile(id) => {
                            let prepared = self.prepare_profile_tab(id);
                            if prepared && let Some(daemon) = &self.daemon {
                                self.pending_tab = None;
                                if daemon.open_profile(id) {
                                    self.profile_panel.finish_open();
                                    self.terminal_surface = TerminalSurfaceModel::default();
                                    self.terminal_decorations = TerminalDecorations::default();
                                    self.terminal_clipboard = terminal_clipboard::State::default();
                                    self.log_surface = None;
                    self.terminal_scrollback = false;
                                } else {
                                    self.workspace.pending_launch = None;
                                    self.view_model.input_warning = Some("连接请求未提交，请等待当前操作完成后重试。".into());
                                }
                            }
                        }
                        profile_connection::ProfileClientCommand::OpenLocal(id, options) => {
                            let prepared = self.prepare_profile_tab(id);
                            if prepared && let Some(daemon) = &self.daemon {
                                self.pending_tab = None;
                                if daemon.open_local_profile(id, options) {
                                    self.profile_panel.finish_open();
                                    self.terminal_surface = TerminalSurfaceModel::default();
                                    self.terminal_decorations = TerminalDecorations::default();
                                    self.terminal_clipboard = terminal_clipboard::State::default();
                                    self.log_surface = None;
                    self.terminal_scrollback = false;
                                } else {
                                    self.workspace.pending_launch = None;
                                    self.view_model.input_warning = Some("连接请求未提交，请等待当前操作完成后重试。".into());
                                }
                            }
                        }
                        other => {
                            if let Some(profiles) = &self.profiles {
                                if !profiles.request(other) { self.profile_panel.request_failed(); }
                            }
                        }
                    }
                }
                if let Some(command) = clipboard_command
                    && let Some(clipboard) = &self.clipboard
                    && !clipboard.decide(command)
                {
                    self.view_model.input_warning = Some("剪贴板确认队列已满；请重新确认。".into());
                }
                self.apply_terminal_scroll_navigation(return_to_live, scrollbar_action);
                self.apply_search_return_request();
                self.schedule_egui_repaint(&full_output, std::time::Instant::now());
                let egui::FullOutput {
                    mut platform_output,
                    mut textures_delta,
                    shapes,
                    pixels_per_point,
                    ..
                } = full_output;
                let egui_ime_active = platform_output.ime.is_some();
                if self.renderer_needs_font_upload {
                    let image = self.egui_context.fonts(|fonts| fonts.image());
                    textures_delta.push(
                        egui::TextureId::default(),
                        egui::epaint::ImageDelta::full(image, Default::default()),
                    );
                }
                if self.window_active
                    && (self.log_surface.is_none() || self.terminal_scrollback)
                    && self.pending_paste.is_none()
                    && !self.terminal_tools_panel.open
                    && !self.profile_batch_panel.open
                    && !self.terminal_policy_panel.open
                    && !self.workspace_recovery_panel.open
                    && !self.view_model.about_open
                    && !self.view_model.confirm_terminate
                    && !self.view_model.tab_navigator.open
                    && self
                        .egui_context
                        .memory(|memory| memory.focused().is_none())
                    && let Some(update) = &mut platform_output.accesskit_update
                {
                    update.focus = terminal_accessibility::terminal_node_id().accesskit_id();
                }
                if let Some(egui_state) = &mut self.egui_state {
                    egui_state.handle_platform_output_with_event_loop(
                        &window,
                        event_loop,
                        platform_output,
                    );
                }
                if clipboard_action.is_some() {
                    window.request_redraw();
                    self.egui_context.memory_mut(|m| { if let Some(id) = m.focused() { m.surrender_focus(id); } });
                }
                match clipboard_action {
                    Some(terminal_clipboard::Action::Copy) => self.copy_terminal_selection(),
                    Some(terminal_clipboard::Action::Paste) => self.paste_from_clipboard(&window, false),
                    Some(terminal_clipboard::Action::Edit) => self.paste_from_clipboard(&window, true),
                    Some(action) => self.handle_terminal_context_action(action),
                    None => {}
                }
                if cancel_paste || confirm_paste {
                    self.egui_context.memory_mut(|m| { if let Some(id) = m.focused() { m.surrender_focus(id); } });
                    if let Some(sync) = &self.sync_input { sync.lock().unwrap_or_else(std::sync::PoisonError::into_inner).focus_terminal(); }
                }
                if cancel_paste {
                    self.pending_paste = None;
                } else if confirm_paste && let Some(pending) = self.pending_paste.take() {
                    if self.active_terminal_session_id() == Some(pending.session_id) && self.terminal_clipboard.review.is_valid() {
                        self.submit_paste(pending.text, &window);
                    } else {
                        self.view_model.input_warning =
                            Some("目标已变化或预览已过期，请重新粘贴".to_owned());
                    }
                }
                if close_search {
                    self.close_terminal_search();
                } else {
                    if search_changed {
                        if self.log_surface.is_some() {
                            self.schedule_history_search(
                                HistorySearchDirection::Backward,
                                None,
                                true,
                            );
                        } else {
                            self.schedule_terminal_search(true);
                        }
                    }
                    if search_navigation != 0 {
                        if self.log_surface.is_some() {
                            self.navigate_history_search(search_navigation);
                        } else {
                            self.navigate_terminal_search(search_navigation);
                        }
                    }
                }
                let font_pixels = self.view_model.terminal_font_pixels.unwrap_or(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS)
                    .clamp(cshell_domain::MIN_TERMINAL_FONT_PIXELS, cshell_domain::MAX_TERMINAL_FONT_PIXELS);
                self.view_model.terminal_font_pixels = Some(font_pixels);
                if let Some(renderer) = &mut self.renderer {
                    if self.saved_font_pixels.is_some_and(|saved|saved!=font_pixels){self.terminal_mouse.zoom=None;self.view_model.terminal_zoom_pixels=None;}
                    let display_pixels=self.terminal_mouse.zoom.unwrap_or(font_pixels);
                    let font_result = renderer.set_font_size(display_pixels);
                    if let Err(error) = &font_result {
                        self.terminal_mouse.zoom=None;self.view_model.terminal_zoom_pixels=None;
                        self.view_model.input_warning = Some(format!("字号未能更新：{error}"));
                    } else {
                        if matches!(font_result, Ok(true)) { self.clear_clipboard_selection(); }
                        if matches!(font_result, Ok(true)) && let Some(e2e) = &mut self.window_e2e {
                            e2e.note_font_size_change();
                        }
                        if self.saved_font_pixels != Some(font_pixels) {
                            if let Some(path) = &self.display_settings_path {
                                if let Err(error) = display_settings::save(path, font_pixels) {
                                    self.view_model.input_warning = Some(format!("字号已生效，但设置未能保存：{error}"));
                                }
                            }
                            self.saved_font_pixels = Some(font_pixels);
                        }
                    }
                }
                let split_frames = if self.task_panel.open {
                    None
                } else {
                    self.prepare_split_frames(pixels_per_point)
                };
                let active_theme = self.theme_for_tab(self.current_workspace_tab);
                let highlight_setup = self.active_terminal_session_id().map(|session| {
                    let profile = self
                        .daemon
                        .as_ref()
                        .and_then(|d| d.session_profile(session));
                    let (revision, rules) =
                        self.terminal_tools_panel.highlight_rules(session, profile);
                    (session, revision, rules)
                });
                let show_terminal_cursor = self.cursor_blink.visible()
                    && self.current_workspace_tab.and_then(|tab| self.view_model.ssh_tab_states.get(&tab))
                        .is_none_or(|state| *state == cshell_ui::SshConnectionState::Connected);
                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                renderer.set_theme(active_theme);
                let paint_jobs = self.egui_context.tessellate(shapes, pixels_per_point);
                let viewport = TerminalViewport::from_logical_rect(terminal_rect, pixels_per_point);
                let viewport_rows = renderer.viewport_rows(viewport);
                self.terminal_viewport = Some(viewport);
                self.viewport_rows = viewport_rows;
                if self.log_surface.is_none()
                    && !self.task_panel.open
                    && let Some(daemon) = &self.daemon
                {
                    daemon.request_resize(
                        viewport_rows,
                        renderer.viewport_columns(viewport),
                        viewport.width.min(u32::from(u16::MAX)) as u16,
                        viewport.height.min(u32::from(u16::MAX)) as u16,
                    );
                }
                let egui_frame = EguiFrame {
                    paint_jobs: &paint_jobs,
                    textures_delta: &mut textures_delta,
                    pixels_per_point,
                };
                if self.log_surface.is_none()
                    && !self.task_panel.open
                    && !egui_ime_active
                    && !self.terminal_tools_panel.open
                    && !self.profile_batch_panel.open
                    && !self.terminal_policy_panel.open
                    && !self.workspace_recovery_panel.open
                {
                    window.set_ime_allowed(true);
                    if let Some(snapshot) = self.terminal_surface.latest_snapshot() {
                        position_terminal_ime(&window, viewport, snapshot);
                    }
                }
                let render_result = if self.task_panel.open {
                    let frame = self
                        .task_panel
                        .frame(renderer.viewport_columns(viewport), viewport_rows);
                    renderer.render_log(
                        frame.as_ref(),
                        viewport,
                        &self.task_panel.output_decorations,
                        Some(egui_frame),
                    )
                } else if let Some(frames) = &split_frames {
                    let panes: Vec<_> = frames
                        .iter()
                        .map(|frame| cshell_render::PaneFrame {
                            viewport: frame.viewport,
                            theme: frame.theme,
                            content: if frame.log.is_some() {
                                cshell_render::PaneContent::Log {
                                    frame: frame.log.as_ref(),
                                    decorations: &frame.log_decorations,
                                }
                            } else {
                                cshell_render::PaneContent::Terminal {
                                    frame: frame.terminal.as_ref(),
                                    decorations: &frame.decorations,
                                    cursor_visible: frame.focused && show_terminal_cursor,
                                }
                            },
                        })
                        .collect();
                    renderer.render_panes(&panes, Some(egui_frame))
                } else if let Some(log_surface) = &mut self.log_surface {
                    let viewport_columns = renderer.viewport_columns(viewport);
                    if let Some(request) = log_surface.request_reflow(viewport_columns)
                        && self.log_reflow.as_ref().is_none_or(|worker| {
                            !worker.submit(request, self.current_workspace_tab)
                        })
                    {
                        log_surface.reflow_failed();
                    }
                    if let Some(request) = log_surface.take_page_request(viewport_rows) {
                        if let Some(daemon) = &self.daemon {
                            if self.terminal_scrollback{daemon.request_terminal_history_page(request);}else{daemon.request_log_page(request);}
                        } else if let Some(e2e) = &mut self.window_e2e {
                            let page = e2e.page_for(request);
                            if let Err(error) = log_surface.submit_page(page) {
                                e2e.fail(format!("generated log page was rejected: {error}"));
                            }
                        }
                    }
                    let frame = self.terminal_clipboard.history_frame.clone().or_else(|| log_surface.prepare_frame(viewport_rows));
                    if let (Some((session, revision, rules)), Some(worker), Some(frame)) =
                        (&highlight_setup, &mut self.highlight_worker, &frame)
                    {
                        worker.log(
                            *session,
                            *revision,
                            rules.clone(),
                            frame,
                            &mut self.log_decorations,
                        );
                    }
                    renderer.render_log(
                        frame.as_ref(),
                        viewport,
                        &self.log_decorations,
                        Some(egui_frame),
                    )
                } else {
                    let mut frame = self.terminal_surface.prepare_frame(0, viewport_rows);
                    if let (Some(frame), Some(snapshot)) = (&mut frame, &self.terminal_clipboard.snapshot) {
                        frame.snapshot = snapshot.clone();
                        frame.plan.generation = snapshot.generation;
                    }
                    if let (Some((session, revision, rules)), Some(worker), Some(frame)) =
                        (&highlight_setup, &mut self.highlight_worker, &frame)
                    {
                        worker.terminal(
                            *session,
                            *revision,
                            rules.clone(),
                            frame.snapshot.clone(),
                            &mut self.terminal_decorations,
                        );
                    }
                    if !egui_ime_active
                        && !self.terminal_tools_panel.open
                        && !self.profile_batch_panel.open
                        && !self.terminal_policy_panel.open
                        && !self.workspace_recovery_panel.open
                    {
                        window.set_ime_allowed(true);
                        if let Some(frame) = &frame {
                            position_terminal_ime(&window, viewport, &frame.snapshot);
                        }
                    }
                    renderer.render(
                        frame.as_ref(),
                        viewport,
                        &self.terminal_decorations,
                        show_terminal_cursor,
                        Some(egui_frame),
                    )
                };
                // A surface/device failure can return before GPU texture upload. The next
                // renderer recreates egui's font texture; clear this frame's abandoned deltas.
                textures_delta.clear();
                match render_result {
                    Ok(outcome) => {
                        if outcome == RenderOutcome::Skipped
                            && let Some(e2e) = &mut self.window_e2e
                        {
                            e2e.note_skipped_present();
                        }
                        if outcome == RenderOutcome::Presented && !self.terminal_frame_presented {
                            let has_terminal_text = split_frames.as_ref().map_or_else(
                                || {
                                    self.log_surface.is_none()
                                        && self.terminal_surface.latest_snapshot().is_some_and(
                                            |s| {
                                                s.cells.iter().any(|cell| {
                                                    cell.character != ' ' && cell.character != '\0'
                                                })
                                            },
                                        )
                                },
                                |frames| {
                                    frames.iter().any(|frame| {
                                        frame.terminal.as_ref().is_some_and(|f| {
                                            f.snapshot.cells.iter().any(|cell| {
                                                cell.character != ' ' && cell.character != '\0'
                                            })
                                        })
                                    })
                                },
                            );
                            self.terminal_frame_presented =
                                renderer.rendered_vertices() > 0 && has_terminal_text;
                        }
                        self.renderer_needs_font_upload = false;
                        if outcome == RenderOutcome::Presented
                            && let (Some(e2e), Some(log_surface)) =
                                (&mut self.window_e2e, &mut self.log_surface)
                        {
                            e2e.on_present(
                                log_surface,
                                &window,
                                viewport_rows,
                                renderer.rendered_vertices(),
                            );
                            if let Some(pixels) = e2e.requested_font_size() {
                                self.view_model.terminal_font_pixels = Some(pixels);
                            }
                            if e2e.should_inject_device_loss() {
                                renderer.simulate_device_loss();
                            }
                        }
                    }
                    Err(WindowRendererError::DeviceLost) => {
                        tracing::warn!("window rendering device was lost; recreating renderer");
                        self.renderer = None;
                        self.renderer_recovering = true;
                    }
                    Err(error) => {
                        if let Some(e2e) = &mut self.window_e2e {
                            e2e.fail(format!("window render/present failed: {error}"));
                        }
                        tracing::error!(%error, "terminal frame rendering failed");
                        self.renderer = None;
                        self.renderer_recovering = true;
                    }
                }
            }
            _ => {}
        }
    }
}

impl DesktopApp {
    fn active_terminal_session_id(&self) -> Option<SessionId> {
        let view = self.daemon.as_ref()?.view();
        let session_id = view.session_id?;
        (view.connected
            && self.view_model.selected == Some(session_id)
            && self
                .current_workspace_tab
                .is_none_or(|tab| self.workspace.sessions.get(&tab) == Some(&session_id)))
        .then_some(session_id)
    }

    fn submit_paste(&mut self, text: String, window: &Window) {
        let accepted = self.submit_terminal_action(InputAction::Paste {
            text,
            bracketed: true,
        });
        if accepted {
            self.view_model.input_warning = None;
            self.cursor_blink.note_activity(std::time::Instant::now());
        } else {
            self.view_model.input_warning = Some("终端未连接或输入队列已满，粘贴未发送".to_owned());
            tracing::warn!("terminal paste was not accepted by the daemon client");
        }
        window.request_redraw();
    }

    fn bump_terminal_decorations(&mut self) {
        self.terminal_decorations.revision = self.terminal_decorations.revision.wrapping_add(1);
    }

    fn schedule_terminal_search(&mut self, reset_active: bool) {
        self.terminal_search_revision = self.terminal_search_revision.wrapping_add(1);
        self.terminal_decorations.search = Default::default();
        self.terminal_search_error = None;
        if reset_active {
            self.terminal_decorations.active_search_match = None;
        }
        self.bump_terminal_decorations();
        let Some(snapshot) = self.terminal_clipboard.snapshot.as_ref()
            .or_else(|| self.terminal_surface.latest_snapshot()).cloned() else {
            self.terminal_search_requested_generation = None;
            return;
        };
        self.terminal_search_requested_generation = Some(snapshot.generation);
        if self.terminal_search_query.is_empty() {
            return;
        }
        if let Some(search) = &self.terminal_search {
            search.submit(TerminalSearchRequest {
                revision: self.terminal_search_revision,
                snapshot,
                query: self.terminal_search_query.clone(),
                options: self.terminal_search_options,
            });
        }
    }

    fn navigate_terminal_search(&mut self, direction: i8) {
        let count = self.terminal_decorations.search.matches.len();
        if count == 0 {
            return;
        }
        let current = self
            .terminal_decorations
            .active_search_match
            .unwrap_or(0)
            .min(count - 1);
        self.terminal_decorations.active_search_match = Some(if direction < 0 {
            current.checked_sub(1).unwrap_or(count - 1)
        } else {
            (current + 1) % count
        });
        self.bump_terminal_decorations();
    }

    fn history_search_request(
        &self,
        revision: u64,
        direction: HistorySearchDirection,
        cursor: Option<(u64, u32)>,
    ) -> DesktopHistorySearchRequest {
        DesktopHistorySearchRequest {
            revision,
            query: self.terminal_search_query.clone(),
            case_sensitive: self.terminal_search_options.case_sensitive,
            whole_word: self.terminal_search_options.whole_word,
            regex: self.terminal_search_options.regex,
            direction,
            cursor,
        }
    }

    fn schedule_history_search(
        &mut self,
        direction: HistorySearchDirection,
        cursor: Option<(u64, u32)>,
        reset_active: bool,
    ) {
        self.remember_search_position();
        self.terminal_search_revision = self.terminal_search_revision.wrapping_add(1);
        self.terminal_search_error = None;
        self.history_search_direction = Some(direction);
        if reset_active {
            self.log_decorations.revision = self.log_decorations.revision.wrapping_add(1);
            self.log_decorations.active_search_match = None;
        }
        let Some(daemon) = &self.daemon else {
            self.terminal_search_error =
                Some("history search requires a daemon session".to_owned());
            return;
        };
        if self.terminal_search_query.is_empty() {
            daemon.cancel_history_search();
            return;
        }
        if let Err(error) = validate_terminal_search_query(
            &self.terminal_search_query,
            self.terminal_search_options,
        ) {
            self.terminal_search_error = Some(error.to_string());
            daemon.cancel_history_search();
            return;
        }
        daemon.request_history_search(self.history_search_request(
            self.terminal_search_revision,
            direction,
            cursor,
        ));
    }

    fn navigate_history_search(&mut self, direction: i8) {
        let search_direction = if direction < 0 {
            HistorySearchDirection::Backward
        } else {
            HistorySearchDirection::Forward
        };
        let cursor = self
            .log_decorations
            .active_search_match
            .as_ref()
            .map(|matched| {
                (
                    matched.line_id,
                    if direction < 0 {
                        matched.byte_range.start
                    } else {
                        matched.byte_range.end
                    },
                )
            });
        self.schedule_history_search(search_direction, cursor, false);
    }

    fn close_terminal_search(&mut self) {
        self.terminal_search_open = false;
        self.terminal_search_focus_requested = false;
        self.egui_context.memory_mut(|m|{if let Some(id)=m.focused(){m.surrender_focus(id);}});
        self.terminal_search_revision = self.terminal_search_revision.wrapping_add(1);
        self.terminal_search_requested_generation = None;
        self.terminal_search_error = None;
        self.history_search_direction = None;
        self.submitted_history_search = None;
        if let Some(daemon) = &self.daemon {
            daemon.cancel_history_search();
        }
        self.log_decorations.revision = self.log_decorations.revision.wrapping_add(1);
        self.log_decorations.active_search_match = None;
        self.terminal_decorations.search = Default::default();
        self.terminal_decorations.active_search_match = None;
        self.bump_terminal_decorations();
    }

    fn terminal_point_at(&self, position:winit::dpi::PhysicalPosition<f64>) -> Option<TerminalCellPoint> {
        let (rows,cols)=if let Some(snapshot)=&self.terminal_clipboard.snapshot {(snapshot.rows,snapshot.cols)}
            else if let Some(frame)=self.log_surface.as_ref().and_then(|s|s.prepare_frame(self.viewport_rows)) {(self.viewport_rows,frame.layout_columns)}
            else {let snapshot=self.terminal_surface.latest_snapshot()?;(snapshot.rows,snapshot.cols)};
        self.terminal_point_for_grid(position,rows,cols,false)

    }
}

fn terminal_point_at(
    position: winit::dpi::PhysicalPosition<f64>,
    viewport: TerminalViewport,
    rows: u16,
    cols: u16,
) -> Option<TerminalCellPoint> {
    if rows == 0
        || cols == 0
        || position.x < f64::from(viewport.x)
        || position.y < f64::from(viewport.y)
        || position.x >= f64::from(viewport.x.saturating_add(viewport.width))
        || position.y >= f64::from(viewport.y.saturating_add(viewport.height))
    {
        return None;
    }
    let x = position.x - f64::from(viewport.x);
    let y = position.y - f64::from(viewport.y);
    Some(TerminalCellPoint {
        row: ((y * f64::from(rows) / f64::from(viewport.height.max(1))) as u16).min(rows - 1),
        column: ((x * f64::from(cols) / f64::from(viewport.width.max(1))) as u16).min(cols - 1),
    })
}

fn is_terminal_copy_shortcut(event: &winit::event::KeyEvent, modifiers: ModifiersState) -> bool {
    matches_terminal_copy_shortcut(event.state, event.repeat, &event.logical_key, modifiers)
}
fn matches_terminal_copy_shortcut(state: ElementState, repeat: bool, key: &Key, modifiers: ModifiersState) -> bool {
    state == ElementState::Pressed && !repeat && !modifiers.alt_key()
        && ((matches!(key, Key::Character(text) if text.eq_ignore_ascii_case("c"))
            && (modifiers.super_key() || (modifiers.control_key() && modifiers.shift_key())))
            || (key == &Key::Named(NamedKey::Insert) && modifiers.control_key()
                && !modifiers.shift_key() && !modifiers.super_key()))
}

fn is_terminal_paste_shortcut(event: &winit::event::KeyEvent, modifiers: ModifiersState) -> bool {
    matches_terminal_paste_shortcut(event.state, event.repeat, &event.logical_key, modifiers)
}

fn matches_terminal_paste_shortcut(
    state: ElementState,
    repeat: bool,
    key: &Key,
    modifiers: ModifiersState,
) -> bool {
    state == ElementState::Pressed
        && !repeat
        && !modifiers.alt_key()
        && ((matches!(key, Key::Character(text) if text.eq_ignore_ascii_case("v"))
            && (modifiers.super_key() || (modifiers.control_key() && modifiers.shift_key())))
            || (key == &Key::Named(NamedKey::Insert)
                && modifiers.shift_key()
                && !modifiers.control_key()
                && !modifiers.super_key()))
}

fn requires_paste_confirmation(text: &str) -> bool {
    text.chars().any(char::is_control)
}

fn decide_terminal_paste(text: Option<String>) -> Result<PasteDecision, String> {
    let text = text
        .filter(|text| !text.is_empty())
        .ok_or("剪贴板没有可用文本")?;
    if text.len() > MAX_TERMINAL_INPUT_BYTES {
        return Err(format!(
            "粘贴内容超过 {} MiB 上限，未发送",
            MAX_TERMINAL_INPUT_BYTES / (1024 * 1024)
        ));
    }
    if requires_paste_confirmation(&text) {
        Ok(PasteDecision::Confirm(text))
    } else {
        Ok(PasteDecision::Immediate(text))
    }
}

#[cfg(test)]
fn paste_preview(text: &str) -> String {
    let mut preview = String::new();
    let mut chars = text.chars();
    for ch in chars.by_ref().take(400) {
        match ch {
            '\n' => preview.push_str("\\n\n"),
            '\r' => preview.push_str("\\r"),
            '\t' => preview.push_str("\\t"),
            ch if ch.is_control() => preview.push_str(&format!("\\u{{{:x}}}", u32::from(ch))),
            ch => preview.push(ch),
        }
    }
    if chars.next().is_some() {
        preview.push_str("\n…（仅预览前 400 个字符）");
    }
    preview
}

fn is_terminal_search_shortcut(event: &winit::event::KeyEvent, modifiers: ModifiersState) -> bool {
    workbench_navigation::shortcut(event.state,&event.logical_key,modifiers,"f")
}

fn truncate_utf8_bytes(text: &mut String, maximum: usize) {
    if text.len() <= maximum {
        return;
    }
    let mut boundary = maximum;
    while !text.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    text.truncate(boundary);
}

fn register_terminal_click(
    state: &mut TerminalClickState,
    point: TerminalCellPoint,
    now: std::time::Instant,
) -> u8 {
    let continues = state.last.is_some_and(|(last_at, last_point)| {
        last_point == point && now.duration_since(last_at) <= TERMINAL_MULTI_CLICK_INTERVAL
    });
    state.count = if continues && state.count < 3 {
        state.count + 1
    } else {
        1
    };
    state.last = Some((now, point));
    state.count
}

fn position_terminal_ime(
    window: &Window,
    viewport: TerminalViewport,
    snapshot: &cshell_terminal::FrameSnapshot,
) {
    let cell_width = (viewport.width / u32::from(snapshot.cols.max(1))).max(1);
    let cell_height = (viewport.height / u32::from(snapshot.rows.max(1))).max(1);
    let x = viewport
        .x
        .saturating_add(u32::from(snapshot.cursor_col).saturating_mul(cell_width));
    let y = viewport.y.saturating_add(
        u32::from(snapshot.cursor_row)
            .saturating_add(1)
            .saturating_mul(cell_height),
    );
    window.set_ime_cursor_area(
        winit::dpi::PhysicalPosition::new(x, y),
        winit::dpi::PhysicalSize::new(cell_width, cell_height),
    );
}

impl DesktopApp {
    fn terminal_keyboard_available(&self) -> bool {
        !self.command_center.blocks_input() && self.active_terminal_session_id().is_some()
            && (self.log_surface.is_none() || self.terminal_scrollback)
            && self.pending_paste.is_none()
            && !self.task_panel.open
            && !self.host_panel.open
            && !self.terminal_search_open
            && !(self.profile_panel.is_dialog_open() || self.zmodem.blocks_input() || self.keyboard_auth.blocks_input())
            && !self.tunnel_panel.open
            && !self.host_ca_panel.open
            && !self.terminal_tools_panel.open
            && !self.profile_batch_panel.open
            && !self.terminal_policy_panel.open
            && !self.workspace_recovery_panel.open
            && !self.ssh_provider_panel.open
            && !self.view_model.about_open
            && !self.view_model.confirm_terminate
            && !self.view_model.tab_navigator.open
            && !self.sync_blocks_input()
    }
}

fn terminal_tab_bypasses_egui(key: &Key, terminal_available: bool, egui_wants_keyboard: bool) -> bool {
    terminal_available && !egui_wants_keyboard && key == &Key::Named(NamedKey::Tab)
}

fn terminal_key_action(
    event: &winit::event::KeyEvent,
    modifiers: ModifiersState,
    modes: cshell_terminal::TerminalModes,
    broadcast: bool,
) -> Option<InputAction> {
    desktop_key_action(
        DesktopKeyInput {
            key: &event.logical_key,
            text: event.text.as_deref(),
            pressed: event.state == ElementState::Pressed,
            repeated: event.repeat,
        },
        modifiers,
        modes,
        broadcast,
    )
}

struct DesktopKeyInput<'a> {
    key: &'a Key,
    text: Option<&'a str>,
    pressed: bool,
    repeated: bool,
}

fn desktop_key_action(
    event: DesktopKeyInput<'_>,
    modifiers: ModifiersState,
    modes: cshell_terminal::TerminalModes,
    broadcast: bool,
) -> Option<InputAction> {
    let pressed = event.pressed;
    let report_events =
        broadcast || modes.kitty_enabled(cshell_terminal::TerminalModes::KITTY_REPORT_EVENTS);
    let report_all =
        broadcast || modes.kitty_enabled(cshell_terminal::TerminalModes::KITTY_REPORT_ALL_KEYS);
    if (!pressed && !report_events)
        || (!broadcast && modifiers.super_key() && modes.kitty_keyboard_flags == 0)
    {
        return None;
    }
    let modifiers = Modifiers {
        ctrl: modifiers.control_key(),
        alt: modifiers.alt_key(),
        shift: modifiers.shift_key(),
        super_key: modifiers.super_key(),
    };
    let code = match event.key {
        Key::Character(text)
            if modifiers.ctrl || modifiers.alt || modifiers.super_key || report_all =>
        {
            KeyCode::Character(
                if broadcast && !modifiers.ctrl && !modifiers.alt && !modifiers.super_key {
                    event.text.unwrap_or(text.as_str()).to_owned()
                } else {
                    text.to_string()
                },
            )
        }
        Key::Character(_) if pressed => {
            return event
                .text
                .filter(|text| !text.is_empty())
                .map(|text| InputAction::Text(text.to_string()));
        }
        Key::Named(NamedKey::Space) => KeyCode::Character(" ".into()),
        Key::Named(NamedKey::Home) => KeyCode::Home,
        Key::Named(NamedKey::End) => KeyCode::End,
        Key::Named(NamedKey::Insert) => KeyCode::Insert,
        Key::Named(NamedKey::Delete) => KeyCode::Delete,
        Key::Named(NamedKey::PageUp) => KeyCode::PageUp,
        Key::Named(NamedKey::PageDown) => KeyCode::PageDown,
        Key::Named(NamedKey::Enter) => KeyCode::Enter,
        Key::Named(NamedKey::Tab) => KeyCode::Tab,
        Key::Named(NamedKey::Backspace) => KeyCode::Backspace,
        Key::Named(NamedKey::Escape) => KeyCode::Escape,
        Key::Named(NamedKey::ArrowUp) => KeyCode::ArrowUp,
        Key::Named(NamedKey::ArrowDown) => KeyCode::ArrowDown,
        Key::Named(NamedKey::ArrowLeft) => KeyCode::ArrowLeft,
        Key::Named(NamedKey::ArrowRight) => KeyCode::ArrowRight,
        Key::Named(named) => KeyCode::Function(function_key_number(*named)?),
        _ => return None,
    };
    Some(InputAction::Key(KeyEvent {
        code,
        modifiers,
        pressed,
        repeated: event.repeated,
    }))
}

const fn function_key_number(key: NamedKey) -> Option<u8> {
    match key {
        NamedKey::F1 => Some(1),
        NamedKey::F2 => Some(2),
        NamedKey::F3 => Some(3),
        NamedKey::F4 => Some(4),
        NamedKey::F5 => Some(5),
        NamedKey::F6 => Some(6),
        NamedKey::F7 => Some(7),
        NamedKey::F8 => Some(8),
        NamedKey::F9 => Some(9),
        NamedKey::F10 => Some(10),
        NamedKey::F11 => Some(11),
        NamedKey::F12 => Some(12),
        NamedKey::F13 => Some(13),
        NamedKey::F14 => Some(14),
        NamedKey::F15 => Some(15),
        NamedKey::F16 => Some(16),
        NamedKey::F17 => Some(17),
        NamedKey::F18 => Some(18),
        NamedKey::F19 => Some(19),
        NamedKey::F20 => Some(20),
        NamedKey::F21 => Some(21),
        NamedKey::F22 => Some(22),
        NamedKey::F23 => Some(23),
        NamedKey::F24 => Some(24),
        _ => None,
    }
}

fn log_scrollbar_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2((rect.max.x - 12.0).max(rect.min.x), rect.min.y), rect.max)
}

fn draw_log_scrollbar(
    ui: &mut egui::Ui,
    terminal_rect: &mut egui::Rect,
    state: LogScrollbarState,
) -> Option<LogScrollbarAction> {
    const WIDTH: f32 = 12.0;
    const MIN_THUMB: f32 = 24.0;
    if terminal_rect.width() <= WIDTH || terminal_rect.height() <= 0.0 {
        return None;
    }
    let track = log_scrollbar_rect(*terminal_rect);
    terminal_rect.max.x -= WIDTH;
    let first = state.first_line_id.max(1);
    let total = state.total_line_count.saturating_sub(first).saturating_add(1).max(1);
    let visible_fraction = (f64::from(state.viewport_rows) / total as f64).clamp(0.03, 1.0);
    let thumb_height = (track.height() * visible_fraction as f32)
        .max(MIN_THUMB)
        .min(track.height());
    let progress = if state.follow_tail || total <= 1 {
        1.0
    } else {
        (state.top_line_id.saturating_sub(first) as f64 / total.saturating_sub(1) as f64)
            .clamp(0.0, 1.0) as f32
    };
    let thumb_top = track.top() + (track.height() - thumb_height) * progress;
    let thumb = egui::Rect::from_min_size(
        egui::pos2(track.left() + 2.0, thumb_top),
        egui::vec2(WIDTH - 4.0, thumb_height),
    );
    ui.painter()
        .rect_filled(track, 0.0, egui::Color32::from_black_alpha(70));
    ui.painter()
        .rect_filled(thumb, 2.0, egui::Color32::from_gray(130));
    let response = ui.interact(
        track,
        ui.make_persistent_id("cshell-log-scrollbar"),
        egui::Sense::click_and_drag(),
    );
    if !(response.clicked() || response.dragged()) {
        return None;
    }
    let pointer = response.interact_pointer_pos()?;
    let progress = ((pointer.y - track.top()) / track.height()).clamp(0.0, 1.0);
    Some(match log_scrollbar_action(progress, total) {
        LogScrollbarAction::JumpTo(line) => LogScrollbarAction::JumpTo(first.saturating_add(line - 1)),
        action => action,
    })
}

fn log_scrollbar_action(progress: f32, total_line_count: u64) -> LogScrollbarAction {
    let progress = progress.clamp(0.0, 1.0);
    if progress >= 0.999 {
        return LogScrollbarAction::FollowTail;
    }
    let total = total_line_count.max(1);
    let line_id = 1 + (f64::from(progress) * total.saturating_sub(1) as f64).round() as u64;
    LogScrollbarAction::JumpTo(line_id)
}

fn initialize_renderer(
    window: Arc<Window>,
    prefer_software: bool,
) -> Result<WindowRenderer, Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    if prefer_software {
        match runtime.block_on(WindowRenderer::new_software(Arc::clone(&window))) {
            Ok(renderer) => return Ok(renderer),
            Err(error) => tracing::warn!(%error, "software window adapter unavailable"),
        }
    }
    Ok(runtime.block_on(WindowRenderer::new(window))?)
}

#[derive(Default)]
struct DesktopWindows {
    primary: DesktopApp,
    others: std::collections::BTreeMap<WindowId, DesktopApp>,
    unavailable_windows: std::collections::BTreeSet<cshell_domain::WorkspaceWindowId>,
}

impl DesktopWindows {
    fn absorb(
        &mut self,
        document: cshell_domain::WorkspaceDocument,
        sessions: std::collections::BTreeMap<cshell_domain::TabId, SessionId>,
    ) {
        if self.primary.workspace.document != document
            || self.primary.workspace.sessions != sessions
        {
            self.primary.workspace.document.clone_from(&document);
            self.primary.workspace.sessions.clone_from(&sessions);
            self.primary.refresh_workspace_tabs();
            if let Some(window) = &self.primary.window {
                window.request_redraw();
            }
        }
        for child in self.others.values_mut() {
            if child.workspace.document != document || child.workspace.sessions != sessions {
                child.workspace.document.clone_from(&document);
                child.workspace.sessions.clone_from(&sessions);
                child.refresh_workspace_tabs();
                if let Some(window) = &child.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn distribute_views(
        &mut self,
        mut views: std::collections::BTreeMap<
            cshell_domain::TabId,
            (CachedTabView, Option<DesktopDaemonConnection>),
        >,
    ) {
        for app in std::iter::once(&mut self.primary).chain(self.others.values_mut()) {
            let tabs = views
                .keys()
                .copied()
                .filter(|tab| app.workspace.window_of_tab(*tab) == Some(app.workspace.window_id))
                .collect::<Vec<_>>();
            for tab in tabs {
                if app.current_workspace_tab == Some(tab) {
                    let (_, idle) = app.take_tab_for_transfer(tab);
                    if let Some(idle) = idle {
                        idle.retire();
                    }
                }
                if let Some((view, connection)) = views.remove(&tab) {
                    app.cached_tab_views.insert(tab, view);
                    if let Some(connection) = connection {
                        app.pane_daemons.insert(tab, connection);
                    }
                }
            }
            if let Some(active) = app.workspace.active() {
                app.activate_workspace_tab(active);
            } else {
                app.refresh_workspace_tabs();
            }
        }
        // A native window may be unavailable. Retiring a view leaves its daemon
        // process alive; the saved layout/runtime identity stays available.
        for (_, (_, connection)) in views {
            if let Some(connection) = connection {
                connection.retire();
            }
        }
    }

    fn transfer_group(
        &mut self,
        event_loop: &ActiveEventLoop,
        source_id: WindowId,
        tabs: Vec<cshell_domain::TabId>,
    ) {
        let source = if self
            .primary
            .window
            .as_ref()
            .is_some_and(|w| w.id() == source_id)
        {
            &mut self.primary
        } else if let Some(child) = self.others.get_mut(&source_id) {
            child
        } else {
            return;
        };
        let mut views = std::collections::BTreeMap::new();
        for tab in tabs {
            views.insert(tab, source.take_tab_for_transfer(tab));
        }
        let document = source.workspace.document.clone();
        let sessions = source.workspace.sessions.clone();
        self.absorb(document, sessions);
        self.reconcile(event_loop);
        self.distribute_views(views);
        self.absorb(
            self.primary.workspace.document.clone(),
            self.primary.workspace.sessions.clone(),
        );
    }

    fn apply_recovery(&mut self, event_loop: &ActiveEventLoop) {
        let Some(connection) = self.primary.workspace_connection.clone() else {
            return;
        };
        let Some(receipt) = connection.view().restored else {
            return;
        };
        if receipt.plan.before != self.primary.workspace.document
            || receipt.plan.validate().is_err()
            || std::iter::once(&self.primary)
                .chain(self.others.values())
                .any(|app| {
                    app.workspace.pending_launch.is_some() || app.closing_workspace_tab.is_some()
                })
        {
            self.primary.workspace_layout_error = Some(
                "布局已提交，但本地状态改变；自动保存保持暂停，请明确重新读取并再次预览".into(),
            );
            return;
        }
        if !connection.adopt(&receipt) {
            return;
        }
        if let Some(sync) = &self.primary.sync_input {
            let mut sync = sync
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sync.disarm(cshell_application::SyncInputDisarmReason::SourceUnavailable);
            for window in &receipt.plan.before.windows {
                sync.remove_window(window.id);
            }
        }
        let mut sessions = self.primary.workspace.sessions.clone();
        sessions.retain(|tab, _| receipt.plan.retain_tabs.contains(tab));
        let mut views = std::collections::BTreeMap::new();
        for app in std::iter::once(&mut self.primary).chain(self.others.values_mut()) {
            let tabs = app
                .workspace
                .document
                .bindings
                .iter()
                .map(|b| b.tab_id)
                .filter(|tab| app.workspace.window_of_tab(*tab) == Some(app.workspace.window_id))
                .collect::<Vec<_>>();
            for tab in tabs {
                let view = app.take_tab_for_transfer(tab);
                if receipt.plan.retain_tabs.contains(&tab) {
                    views.insert(tab, view);
                } else if let Some(connection) = view.1 {
                    connection.retire();
                }
            }
            if let Some(connection) = app.daemon.take() {
                connection.retire();
            }
            for (_, connection) in std::mem::take(&mut app.pane_daemons) {
                connection.retire();
            }
            for (_, connection) in std::mem::take(&mut app.sync_daemons) {
                connection.retire();
            }
            app.workspace.document = receipt.snapshot.document.clone();
            app.workspace.sessions = sessions.clone();
            app.workspace.pending_launch = None;
            app.workspace.writable = true;
            app.workspace_loaded = true;
            app.current_workspace_tab = None;
            app.pending_tab = None;
            app.pending_paste = None;
            app.pending_terminal_context = None;
            app.cached_tab_views.clear();
            app.workspace_layout_error = None;
            app.last_sync_view = None;
            app.terminal_surface = Default::default();
            app.terminal_decorations = Default::default();
            app.terminal_clipboard = terminal_clipboard::State::default();
            app.log_surface = None;
            app.terminal_scrollback = false;
            app.log_decorations = Default::default();
            app.view_model.daemon_connected = false;
            app.view_model.can_reconnect_ssh = false;
            app.view_model.selected = None;
        }
        if !receipt
            .snapshot
            .document
            .windows
            .iter()
            .any(|w| w.id == self.primary.workspace.window_id)
        {
            self.primary.workspace.window_id = receipt.snapshot.document.windows[0].id;
        }
        self.unavailable_windows.clear();
        self.reconcile(event_loop);
        self.distribute_views(views);
        self.absorb(self.primary.workspace.document.clone(), sessions);
        self.primary.refresh_workspace_tabs();
    }

    fn reconcile(&mut self, event_loop: &ActiveEventLoop) {
        if !self.primary.workspace_loaded {
            return;
        }
        let obsolete = self
            .others
            .iter()
            .filter(|(_, child)| {
                !self
                    .primary
                    .workspace
                    .document
                    .windows
                    .iter()
                    .any(|w| w.id == child.workspace.window_id)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in obsolete {
            if let Some(child) = self.others.remove(&id) {
                Self::retire_child(child);
            }
        }
        let windows = self.primary.workspace.document.windows.clone();
        for layout in windows {
            if layout.id == self.primary.workspace.window_id
                || self.unavailable_windows.contains(&layout.id)
                || self
                    .others
                    .values()
                    .any(|child| child.workspace.window_id == layout.id)
            {
                continue;
            }
            let mut child = DesktopApp {
                accesskit_proxy: self.primary.accesskit_proxy.clone(),
                display_settings_path: self.primary.display_settings_path.clone(),
                saved_font_pixels: self.primary.saved_font_pixels,
                workbench_settings_path: self.primary.workbench_settings_path.clone(),
                opened_sessions: self.primary.opened_sessions.clone(),
                ssh_tab_states: self.primary.ssh_tab_states.clone(),
                workspace: workspace_model::DesktopWorkspace {
                    document: self.primary.workspace.document.clone(),
                    sessions: self.primary.workspace.sessions.clone(),
                    pending_launch: None,
                    writable: self.primary.workspace.writable,
                    window_id: layout.id,
                },
                workspace_loaded: true,
                workspace_connection: self.primary.workspace_connection.clone(),
                secondary_window: true,
                daemon_config: self.primary.daemon_config.clone(),
                terminal_policy_panel: self.primary.terminal_policy_panel.fork(),
                sync_input: self.primary.sync_input.clone(),
                host_groups: self.primary.host_groups.clone(),
                tasks: self.primary.tasks.clone(),
                ..DesktopApp::default()
            };
            child.view_model.terminal_font_pixels = self.primary.view_model.terminal_font_pixels;
            let preferred_layout=self.primary.workbench_settings_path.as_deref().map(workbench_settings::read).unwrap_or_else(||cshell_ui::workbench_layout::Layout::capture(&self.primary.view_model));
            preferred_layout.apply(&mut child.view_model);
            child.saved_workbench_layout=Some(preferred_layout);
            if let Some(config) = &child.daemon_config {
                child.daemon = DesktopDaemonConnection::start_idle(config.clone()).ok();
                child.profiles = DesktopProfileConnection::start(config.clone()).ok();
                child.sftp = DesktopSftpConnection::start(config.clone()).ok();
                child.clipboard =
                    clipboard_connection::DesktopClipboardConnection::start(config.clone()).ok();
                child.clipboard_policy =
                    clipboard_policy::DesktopClipboardPolicy::start(config.clone()).ok();
            }
            child.terminal_search = TerminalSearchWorker::start().ok();
            child.log_reflow = LogReflowWorker::start().ok();
            child.refresh_workspace_tabs();
            child.resumed(event_loop);
            if let Some(tab) = child.workspace.active() {
                child.activate_workspace_tab(tab);
            }
            if self
                .primary
                .window_e2e
                .as_ref()
                .is_some_and(window_e2e::WindowE2e::multi_window)
            {
                child
                    .terminal_surface
                    .submit_snapshot(Arc::new(visual_corpus::snapshot()));
                if let Some(window) = &child.window {
                    window.request_redraw();
                }
            }
            if let Some(id) = child.window.as_ref().map(|w| w.id()) {
                self.others.insert(id, child);
            } else {
                self.unavailable_windows.insert(layout.id);
                let detail =
                    format!("Window {layout:?} could not be created; saved layout retained");
                self.primary.workspace_layout_error = Some(detail.clone());
                self.primary.view_model.workspace_status = detail.clone();
                if let Some(e2e) = &mut self.primary.window_e2e {
                    e2e.fail(detail);
                }
                if let Some(window) = &self.primary.window {
                    window.request_redraw();
                }
            }
        }
        self.publish_available();
    }

    fn publish_available(&mut self) {
        let busy = std::iter::once(&self.primary)
            .chain(self.others.values())
            .any(|app| {
                app.workspace.pending_launch.is_some() || app.closing_workspace_tab.is_some()
            });
        let mut available = Vec::new();
        if self.primary.window.is_some() {
            available.push(self.primary.workspace.window_id);
        }
        available.extend(self.others.values().map(|child| child.workspace.window_id));
        for app in std::iter::once(&mut self.primary).chain(self.others.values_mut()) {
            app.workspace_runtime_busy = busy;
            if app.view_model.available_windows != available {
                app.view_model.available_windows.clone_from(&available);
                if let Some(window) = &app.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn transfer(
        &mut self,
        source_id: WindowId,
        tab: cshell_domain::TabId,
        target_id: cshell_domain::WorkspaceWindowId,
    ) {
        let source = if self
            .primary
            .window
            .as_ref()
            .is_some_and(|w| w.id() == source_id)
        {
            &mut self.primary
        } else if let Some(child) = self.others.get_mut(&source_id) {
            child
        } else {
            return;
        };
        let (view, connection) = source.take_tab_for_transfer(tab);
        let next = source.workspace.active();
        if let Some(next) = next {
            source.activate_workspace_tab(next);
        } else {
            source.refresh_workspace_tabs();
        }
        let document = source.workspace.document.clone();
        let sessions = source.workspace.sessions.clone();
        self.absorb(document, sessions);
        let target = if self.primary.workspace.window_id == target_id {
            Some(&mut self.primary)
        } else {
            self.others
                .values_mut()
                .find(|child| child.workspace.window_id == target_id)
        };
        if let Some(target) = target {
            target.cached_tab_views.insert(tab, view);
            if let Some(connection) = connection {
                target.pane_daemons.insert(tab, connection);
            }
            target.activate_workspace_tab(tab);
            if let Some(window) = &target.window {
                window.request_redraw();
            }
        }
    }

    fn retire_child(mut child: DesktopApp) {
        if let Some(sync) = &child.sync_input {
            sync.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove_window(child.workspace.window_id);
        }
        if let Some(connection) = child.daemon.take() {
            connection.retire();
        }
        for (_, connection) in std::mem::take(&mut child.pane_daemons) {
            connection.retire();
        }
        for (_, connection) in std::mem::take(&mut child.sync_daemons) {
            connection.retire();
        }
    }
}

impl DesktopApp {
    fn take_tab_for_transfer(
        &mut self,
        tab: cshell_domain::TabId,
    ) -> (CachedTabView, Option<DesktopDaemonConnection>) {
        if self.current_workspace_tab != Some(tab) {
            return (
                self.cached_tab_views.remove(&tab).unwrap_or_default(),
                self.pane_daemons.remove(&tab),
            );
        }
        if let Some(window) = &self.window {
            window.set_ime_allowed(false);
        }
        self.current_workspace_tab = None;
        self.pending_tab = None;
        self.pending_paste = None;
        self.terminal_selecting = false;
        self.terminal_clipboard.menu = None;
        self.view_model.selected = None;
        self.view_model.daemon_connected = false;
        self.view_model.can_reconnect_ssh = false;
        let view = CachedTabView {
            reading: std::mem::take(&mut self.terminal_reading),
            clipboard: std::mem::take(&mut self.terminal_clipboard),
            surface: std::mem::take(&mut self.terminal_surface),
            decorations: std::mem::take(&mut self.terminal_decorations),
            log_surface: self.log_surface.take(),
            terminal_scrollback: self.terminal_scrollback,
            log_decorations: std::mem::take(&mut self.log_decorations),
            query: std::mem::take(&mut self.terminal_search_query),
            options: std::mem::take(&mut self.terminal_search_options),
            search_open: self.terminal_search_open,
            last_log_page: self.submitted_log_page.take(),
        };
        self.terminal_search_open = false;
        (view, self.daemon.take())
    }
}

impl ApplicationHandler<DesktopEvent> for DesktopWindows {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.primary.resumed(event_loop);
        self.reconcile(event_loop);
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: DesktopEvent) {
        let DesktopEvent::AccessKit(ref access) = event;
        for app in std::iter::once(&mut self.primary).chain(self.others.values_mut()) {
            if let Some(window) = &mut app.sftp_window
                && window.window.id() == access.window_id {
                window.accesskit(event);
                return;
            }
        }
        if self
            .primary
            .window
            .as_ref()
            .is_some_and(|w| w.id() == access.window_id)
        {
            self.primary.user_event(event_loop, event);
        } else if let Some(child) = self.others.get_mut(&access.window_id) {
            child.user_event(event_loop, event);
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        for app in std::iter::once(&mut self.primary).chain(self.others.values_mut()) {
            if app.sftp_window.as_ref().is_some_and(|window| window.window.id() == id) {
                app.sftp_window_event(event_loop, event);
                return;
            }
        }
        let font_before = if self.primary.window.as_ref().is_some_and(|window| window.id() == id) {
            self.primary.view_model.terminal_font_pixels
        } else { self.others.get(&id).and_then(|app| app.view_model.terminal_font_pixels) };
        if matches!(event, WindowEvent::CloseRequested)
            && !self.primary.window.as_ref().is_some_and(|w| w.id() == id)
            && self
                .primary
                .workspace_connection
                .as_ref()
                .is_some_and(|c| c.view().paused)
        {
            self.primary.view_model.input_warning =
                Some("工作区恢复期间暂停关闭布局窗口；请完成恢复，或从菜单退出应用".into());
            return;
        }
        if self.primary.window.as_ref().is_some_and(|w| w.id() == id) {
            self.primary.window_event(event_loop, id, event);
            if let Some(tabs) = self.primary.transfer_group_to.take() {
                self.transfer_group(event_loop, id, tabs);
            } else if let Some((tab, target)) = self.primary.transfer_to.take() {
                self.transfer(id, tab, target);
            } else {
                self.absorb(
                    self.primary.workspace.document.clone(),
                    self.primary.workspace.sessions.clone(),
                );
            }
        } else if matches!(event, WindowEvent::CloseRequested) {
            if let Some(mut child) = self.others.remove(&id) {
                let window_id = child.workspace.window_id;
                if child.workspace.close_window(window_id) {
                    let document = child.workspace.document.clone();
                    let sessions = child.workspace.sessions.clone();
                    Self::retire_child(child);
                    self.absorb(document, sessions);
                    self.publish_available();
                } else {
                    self.others.insert(id, child);
                }
            }
        } else if let Some(child) = self.others.get_mut(&id) {
            child.window_event(event_loop, id, event);
            if child.terminal_frame_presented
                && let Some(e2e) = &mut self.primary.window_e2e
                && e2e.note_secondary_presented()
                && let Some(window) = &self.primary.window
            {
                window.focus_window();
                window.request_redraw();
            }
            let transfer = child.transfer_to.take();
            let group_transfer = child.transfer_group_to.take();
            let document = child.workspace.document.clone();
            let sessions = child.workspace.sessions.clone();
            if let Some(tabs) = group_transfer {
                self.transfer_group(event_loop, id, tabs);
            } else if let Some((tab, target)) = transfer {
                self.transfer(id, tab, target);
            } else {
                self.absorb(document, sessions);
            }
        }
        let font_after = if self.primary.window.as_ref().is_some_and(|window| window.id() == id) {
            self.primary.view_model.terminal_font_pixels
        } else { self.others.get(&id).and_then(|app| app.view_model.terminal_font_pixels) };
        if font_after.is_some() && font_after != font_before {
            for app in std::iter::once(&mut self.primary).chain(self.others.values_mut()) {
                if app.view_model.terminal_font_pixels != font_after {
                    app.view_model.terminal_font_pixels = font_after;
                    if let Some(window) = &app.window { window.request_redraw(); }
                }
            }
        }
        if std::iter::once(&mut self.primary)
            .chain(self.others.values_mut())
            .fold(false, |requested, app| {
                std::mem::take(&mut app.retry_workspace_windows) || requested
            })
        {
            self.unavailable_windows.clear();
            self.primary.workspace_layout_error = None;
        }
        self.reconcile(event_loop);
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.apply_recovery(event_loop);
        self.primary.about_to_wait(event_loop);
        self.absorb(
            self.primary.workspace.document.clone(),
            self.primary.workspace.sessions.clone(),
        );
        self.reconcile(event_loop);
        let ids: Vec<_> = self.others.keys().copied().collect();
        for id in ids {
            if let Some(child) = self.others.get_mut(&id) {
                child.about_to_wait(event_loop);
                let document = child.workspace.document.clone();
                let sessions = child.workspace.sessions.clone();
                self.absorb(document, sessions);
            }
        }
        let now = std::time::Instant::now();
        let delay = if self.primary.view_model.daemon_connected
            || self
                .others
                .values()
                .any(|child| child.view_model.daemon_connected)
        {
            16
        } else {
            100
        };
        let mut deadline = now + std::time::Duration::from_millis(delay);
        for app in std::iter::once(&self.primary).chain(self.others.values()) {
            deadline = app.next_wake_deadline(deadline);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    diagnostics::install();
    run()
}

fn run() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let event_loop = EventLoop::<DesktopEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let arguments: Vec<_> = std::env::args().collect();
    let show_visual_corpus = arguments
        .iter()
        .any(|argument| argument == visual_corpus::ARGUMENT);
    let show_log_corpus = arguments
        .iter()
        .any(|argument| argument == visual_corpus::LOG_ARGUMENT);
    let show_session_log = arguments
        .iter()
        .any(|argument| argument == SESSION_LOG_ARGUMENT);
    let multi_window_e2e = arguments
        .iter()
        .any(|argument| argument == window_e2e::MULTI_ARGUMENT);
    let run_window_e2e = multi_window_e2e
        || arguments
            .iter()
            .any(|argument| argument == window_e2e::ARGUMENT);
    let daemon_config = if show_visual_corpus || show_log_corpus || run_window_e2e {
        None
    } else {
        DesktopConnectionConfig::from_env()?
    };
    // One lease per GUI process, independent of tabs and secondary windows.
    let _desktop_lifetime = daemon_config.as_ref()
        .map(daemon_connection::DesktopDaemonLifetime::start).transpose()?;
    let profiles = daemon_config
        .clone()
        .map(DesktopProfileConnection::start)
        .transpose()?;
    let sftp = daemon_config
        .clone()
        .map(DesktopSftpConnection::start)
        .transpose()?;
    let clipboard = daemon_config
        .clone()
        .map(clipboard_connection::DesktopClipboardConnection::start)
        .transpose()?;
    let workspace_connection = daemon_config
        .clone()
        .map(|config| workspace_connection::WorkspaceConnection::start(config).map(Arc::new))
        .transpose()?;
    let clipboard_policy = daemon_config
        .clone()
        .map(clipboard_policy::DesktopClipboardPolicy::start)
        .transpose()?;
    let workspace_config = daemon_config.map(|config| config.with_log_pages(true));
    let host_groups = workspace_config
        .as_ref()
        .map(|config| {
            host_group_connection::HostGroupConnection::start(config.clone()).map(Arc::new)
        })
        .transpose()?;
    let sync_input = workspace_config.as_ref().map(|config| {
        Arc::new(std::sync::Mutex::new(sync_input::DesktopSyncInput::start(
            config.clone(),
        )))
    });
    let tasks = workspace_config
        .as_ref()
        .map(|config| task_connection::TaskConnection::start(config.clone()).map(Arc::new))
        .transpose()?;
    let mut app = DesktopApp {
        accesskit_proxy: Some(event_loop.create_proxy()),
        workspace_config,
        sync_input,
        host_groups,
        tasks,
        workspace_connection,
        profiles,
        sftp,
        clipboard,
        clipboard_policy,
        terminal_search: Some(TerminalSearchWorker::start()?),
        ..DesktopApp::default()
    };
    if !show_visual_corpus && !show_log_corpus && !run_window_e2e {
        let (path, pixels) = display_settings::load();
        terminal_clipboard_settings::initialize();
        command_center::initialize();
        app.display_settings_path = path;
        app.saved_font_pixels = Some(pixels);
        app.view_model.terminal_font_pixels = Some(pixels);
        let (path,layout)=workbench_settings::load();
        app.workbench_settings_path=path;
        layout.apply(&mut app.view_model);
        app.saved_workbench_layout=Some(layout);
    }
    if show_visual_corpus {
        let snapshot = Arc::new(visual_corpus::snapshot());
        app.view_model.daemon_status_detail = "visual corpus: terminal-unicode-v1".to_owned();
        app.view_model.terminal_generation = Some(snapshot.generation);
        app.terminal_surface.submit_snapshot(snapshot);
    }
    if show_log_corpus {
        let mut log_surface = LogSurfaceModel::default();
        log_surface.submit_page(visual_corpus::log_page())?;
        app.view_model.daemon_status_detail = "log corpus: terminal-unicode-v1".to_owned();
        app.log_surface = Some(log_surface);
        app.log_reflow = Some(LogReflowWorker::start()?);
    } else if show_session_log {
        app.view_model.daemon_status_detail = "session log: loading".to_owned();
        app.log_surface = Some(LogSurfaceModel::default());
        app.log_reflow = Some(LogReflowWorker::start()?);
    }
    if run_window_e2e {
        let e2e = window_e2e::WindowE2e::new_with_windows(multi_window_e2e);
        let mut log_surface = LogSurfaceModel::default();
        log_surface.submit_page(e2e.initial_page())?;
        app.view_model.daemon_status_detail = "automated log window E2E".to_owned();
        app.log_surface = Some(log_surface);
        app.log_reflow = Some(LogReflowWorker::start()?);
        app.window_e2e = Some(e2e);
        if multi_window_e2e {
            app.workspace_loaded = true;
            app.workspace.writable = true;
            let _created = app.workspace.new_window();
            app.refresh_workspace_tabs();
        }
    }
    let mut windows = DesktopWindows {
        primary: app,
        ..DesktopWindows::default()
    };
    event_loop.run_app(&mut windows)?;
    if let Some(e2e) = &windows.primary.window_e2e {
        e2e.finish()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LogScrollbarAction, PasteDecision, TERMINAL_MULTI_CLICK_INTERVAL, TerminalCellPoint,
        TerminalClickState, TerminalViewport, decide_terminal_paste, log_scrollbar_action,
        matches_terminal_paste_shortcut, paste_preview, register_terminal_click, terminal_point_at,
        truncate_utf8_bytes,
    };
    use std::time::{Duration, Instant};
    use winit::{
        dpi::PhysicalPosition,
        event::ElementState,
        keyboard::{Key, ModifiersState, NamedKey},
    };

    #[test]
    fn egui_animation_deadline_wakes_once_and_idle_output_clears_it() {
        let mut app = super::DesktopApp::default();
        let context = app.egui_context.clone();
        // Settle egui's initial font/layout repaint requests before scheduling
        // a delayed animation through the actual context output.
        for _ in 0..4 {
            let mut output = context.run_ui(Default::default(), |ui| {
                ui.label("Animation timer");
            });
            // This CPU scheduling test has no texture upload target.
            output.textures_delta.clear();
        }
        let delay = Duration::from_millis(50);
        let mut output = context.run_ui(
            egui::RawInput {
                predicted_dt: 0.03125,
                ..Default::default()
            },
            |ui| {
                ui.label("Animation timer");
                ui.ctx().request_repaint_after(delay);
            },
        );
        output.textures_delta.clear();
        // egui subtracts the predicted 31.25 ms frame time. Schedule its
        // returned delay rather than the widget's original 50 ms request.
        let output_delay = Duration::from_micros(18_750);
        assert_eq!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay,
            output_delay
        );
        let now = Instant::now();
        app.schedule_egui_repaint(&output, now);
        assert_eq!(
            app.next_wake_deadline(now + Duration::from_secs(1)),
            now + output_delay
        );
        assert!(!app.poll_egui_repaint(now + output_delay / 2));
        assert!(app.poll_egui_repaint(now + output_delay));
        assert!(!app.poll_egui_repaint(now + output_delay));

        let mut idle_output = context.run_ui(Default::default(), |ui| {
            ui.label("Animation timer");
        });
        idle_output.textures_delta.clear();
        assert_eq!(
            idle_output.viewport_output[&egui::ViewportId::ROOT].repaint_delay,
            Duration::MAX
        );
        app.schedule_egui_repaint(&output, now);
        app.schedule_egui_repaint(&idle_output, now);
        assert_eq!(app.egui_repaint_deadline, None);
        assert_eq!(app.next_wake_deadline(now + delay), now + delay);
    }

    #[test]
    fn startup_discards_old_tab_views_but_preserves_saved_layout_and_error_barrier() {
        let mut old = super::workspace_model::DesktopWorkspace::default();
        old.add(Some(cshell_domain::ProfileId::new()), "Old SSH".into()).unwrap();
        old.split(cshell_domain::SplitAxis::Horizontal).unwrap();
        old.new_window().unwrap();
        let saved = cshell_application::WorkspaceSnapshot { revision: 12, document: old.document };
        for (snapshot, error) in [
            (Some(saved.clone()), None),
            (None, None),
            (None, Some("Storage unavailable".to_owned())),
        ] {
            let view = super::workspace_connection::WorkspaceView {
                ready: true, snapshot: snapshot.clone(), error: error.clone(),
                paused: error.is_some(), blocked: error.is_some(), ..Default::default()
            };
            let mut app = super::DesktopApp::default();
            app.initialize_workspace(&view);
            assert!(app.workspace_loaded);
            assert_eq!(app.workspace.writable, error.is_none());
            assert_eq!(app.workspace_layout_error, error);
            assert!(app.workspace.document.bindings.is_empty());
            assert_eq!(app.workspace.document.windows.len(), 1);
            assert!(app.workspace.sessions.is_empty());
            assert!(app.workspace.pending_launch.is_none());
            assert!(app.current_workspace_tab.is_none());
            assert!(app.view_model.tabs.is_empty());
            assert!(app.view_model.selected_tab.is_none());
            assert_eq!(view.snapshot, snapshot);
            assert!(cshell_application::validate_workspace(&app.workspace.document).is_ok());
        }
    }

    #[test]
    fn opened_session_scope_is_shared_by_windows_and_reset_by_restart() {
        use cshell_domain::SessionId;
        let old = SessionId::new();
        let current = SessionId::new();
        let peer = SessionId::new();
        let mut view = super::daemon_connection::DesktopDaemonView {
            sessions: Some([old, current, peer].into_iter().map(|id| cshell_ipc::SessionSummary {
                reconnecting: false,
                session_id: id.as_uuid().as_bytes().to_vec(), title: format!("{id:?}"), ..Default::default()
            }).collect()), ..Default::default()
        };
        let mut primary = super::DesktopApp::default();
        primary.sync_opened_sessions(&view);
        assert!(primary.view_model.sessions.is_empty());
        let tab = primary.workspace.add(None, "Current".into()).unwrap();
        primary.workspace.sessions.insert(tab, current);
        primary.sync_opened_sessions(&view);
        assert_eq!(primary.view_model.sessions.iter().map(|entry| entry.id).collect::<Vec<_>>(), vec![current]);
        let mut child = super::DesktopApp { opened_sessions: primary.opened_sessions.clone(), ..Default::default() };
        view.session_id = Some(peer);
        child.sync_opened_sessions(&view);
        view.session_id = None;
        primary.sync_opened_sessions(&view);
        assert_eq!(primary.view_model.sessions.len(), 2);
        assert!(!primary.view_model.sessions.iter().any(|entry| entry.id == old));
        let mut restarted = super::DesktopApp::default();
        restarted.sync_opened_sessions(&view);
        assert!(restarted.view_model.sessions.is_empty(), "the opened-session scope must never persist across GUI runs");
        view.closed_session = Some(current);
        primary.sync_opened_sessions(&view);
        assert_eq!(primary.view_model.sessions.iter().map(|entry| entry.id).collect::<Vec<_>>(), vec![peer]);
    }

    #[test]
    fn profile_theme_inheritance_updates_existing_tabs_without_changing_layout() {
        let mut app = super::DesktopApp::default();
        let folder = cshell_domain::FolderId::new();
        let profile = cshell_domain::ProfileId::new();
        let tab = app
            .workspace
            .add(Some(profile), "Saved".into())
            .unwrap_or_else(|| panic!("tab"));
        let record = cshell_domain::ProfileRecord {
            id: profile,
            name: "Saved".into(),
            kind: cshell_domain::ProfileKind::Local,
            folder_id: Some(folder),
            tags: Default::default(),
            favorite: false,
            terminal: Default::default(),
        };
        let mut view = super::profile_connection::DesktopProfileView {
            generation: 1,
            catalog: Some(super::profile_connection::DesktopProfileCatalog {
                themes: Vec::new(),
                revision: 1,
                defaults: Default::default(),
                folders: vec![cshell_domain::ProfileFolder {
                    id: folder,
                    name: "Light".into(),
                    parent_id: None,
                    terminal: cshell_domain::TerminalOverrides {
                        theme: Some("light".into()),
                        ..Default::default()
                    },
                }],
                profiles: vec![record],
                ssh_connections: vec![],
                local_connections: vec![],
            }),
            ..Default::default()
        };
        let original_layout = app.workspace.document.clone();
        assert!(app.update_profile_themes(&view));
        assert_eq!(
            app.theme_for_tab(Some(tab)),
            cshell_render::TerminalTheme::Light
        );
        assert!(!app.update_profile_themes(&view));
        view.generation += 1;
        view.catalog
            .as_mut()
            .unwrap_or_else(|| panic!("catalog"))
            .folders[0]
            .terminal
            .theme = Some("dark".into());
        assert!(app.update_profile_themes(&view));
        assert_eq!(
            app.theme_for_tab(Some(tab)),
            cshell_render::TerminalTheme::Dark
        );
        let palette = cshell_domain::TerminalPalette {
            foreground: [1, 2, 3],
            background: [4, 5, 6],
            cursor: [7, 8, 9],
            selection: [10, 11, 12],
            ansi: [[13, 14, 15]; 16],
        };
        view.generation += 1;
        let catalog = view.catalog.as_mut().unwrap_or_else(|| panic!("catalog"));
        catalog.themes.push(cshell_domain::ThemeRecord {
            name: "paper".into(),
            palette,
        });
        catalog.folders[0].terminal.theme = Some("paper".into());
        assert!(app.update_profile_themes(&view));
        assert_eq!(
            app.theme_for_tab(Some(tab)),
            cshell_render::TerminalTheme::Custom(palette)
        );
        view.generation += 1;
        let changed = view.catalog.as_mut().unwrap_or_else(|| panic!("catalog"));
        changed.themes[0].palette.foreground = [20, 30, 40];
        let expected = changed.themes[0].palette;
        assert!(app.update_profile_themes(&view));
        assert_eq!(
            app.theme_for_tab(Some(tab)),
            cshell_render::TerminalTheme::Custom(expected)
        );
        assert_eq!(app.workspace.document, original_layout);
    }

    #[test]
    fn pending_clone_keeps_launch_tab_selected_until_new_session_is_bound() {
        let mut app = super::DesktopApp::default();
        let source = app
            .workspace
            .add(None, "Source".into())
            .unwrap_or_else(|| panic!("source"));
        app.workspace
            .sessions
            .insert(source, cshell_domain::SessionId::new());
        app.switch_tab_view(source);
        let clone = app
            .workspace
            .clone_session_tab(source)
            .unwrap_or_else(|| panic!("clone"));
        app.switch_tab_view(clone);
        app.workspace.pending_launch = Some(clone);
        app.view_model.selected_tab = Some(source);
        app.activate_workspace_tab(source);
        assert_eq!(app.current_workspace_tab, Some(clone));
        assert_eq!(app.workspace.active(), Some(clone));
        assert_eq!(app.view_model.selected_tab, Some(clone));
        app.close_workspace_tab(clone);
        assert!(
            app.workspace
                .document
                .bindings
                .iter()
                .any(|b| b.tab_id == clone)
        );
        assert_eq!(app.workspace.pending_launch, Some(clone));
    }

    #[test]
    fn hidden_tab_receives_its_pending_log_reflow_and_closed_tab_cache_is_discarded() {
        let mut app = super::DesktopApp::default();
        let first = app
            .workspace
            .add(None, "First".into())
            .unwrap_or_else(|| panic!("tab"));
        let second = app
            .workspace
            .add(None, "Second".into())
            .unwrap_or_else(|| panic!("tab"));
        app.switch_tab_view(first);
        let mut surface = cshell_render::LogSurfaceModel::default();
        surface
            .submit_page(super::visual_corpus::log_page())
            .unwrap_or_else(|e| panic!("{e}"));
        let request = surface
            .request_reflow(40)
            .unwrap_or_else(|| panic!("reflow"));
        app.log_surface = Some(surface);
        app.switch_tab_view(second);
        assert!(!app.apply_log_reflow(super::TabReflowResult {
            tab_id: Some(first),
            result: request.execute()
        }));
        app.switch_tab_view(first);
        assert!(
            app.log_surface
                .as_ref()
                .and_then(|s| s.prepare_frame(10))
                .is_some()
        );
        app.close_workspace_tab(first);
        assert!(!app.cached_tab_views.contains_key(&first));
        assert_eq!(app.current_workspace_tab, Some(second));
        app.close_workspace_tab(second);
        assert!(app.log_surface.is_none());
        assert!(app.current_workspace_tab.is_none());
    }

    #[test]
    fn visible_background_pane_reflow_requests_redraw() {
        let mut app = super::DesktopApp::default();
        let first = app
            .workspace
            .add(None, "First".into())
            .unwrap_or_else(|| panic!("tab"));
        app.switch_tab_view(first);
        let mut surface = cshell_render::LogSurfaceModel::default();
        surface
            .submit_page(super::visual_corpus::log_page())
            .unwrap_or_else(|e| panic!("{e}"));
        let request = surface
            .request_reflow(40)
            .unwrap_or_else(|| panic!("reflow"));
        app.log_surface = Some(surface);
        let second = app
            .workspace
            .split(cshell_domain::SplitAxis::Horizontal)
            .unwrap_or_else(|| panic!("split"));
        app.switch_tab_view(second);
        assert!(app.apply_log_reflow(super::TabReflowResult {
            tab_id: Some(first),
            result: request.execute(),
        }));
    }

    #[test]
    fn moved_terminal_view_keeps_snapshot_and_search_but_drops_pending_paste() {
        let mut source = super::DesktopApp::default();
        let tab = source
            .workspace
            .add(None, "Live".into())
            .unwrap_or_else(|| panic!("tab"));
        let session = cshell_domain::SessionId::new();
        source.workspace.sessions.insert(tab, session);
        source.switch_tab_view(tab);
        let snapshot = std::sync::Arc::new(super::visual_corpus::snapshot());
        source.terminal_surface.submit_snapshot(snapshot.clone());
        source.terminal_search_query = "needle".into();
        source.terminal_search_open = true;
        source.pending_paste = Some(super::PendingPaste {
            session_id: session,
            session_title: "Live".into(),
            text: "unsafe\n".into(),
        });
        let target_id = source
            .workspace
            .new_window()
            .unwrap_or_else(|| panic!("window"));
        assert!(source.workspace.move_to_window(tab, target_id));
        let (view, connection) = source.take_tab_for_transfer(tab);
        assert!(connection.is_none());
        assert!(source.pending_paste.is_none());
        assert!(source.current_workspace_tab.is_none());
        let mut target = super::DesktopApp {
            workspace: workspace_for_window(&source.workspace, target_id),
            ..super::DesktopApp::default()
        };
        target.cached_tab_views.insert(tab, view);
        target.activate_workspace_tab(tab);
        assert_eq!(target.current_workspace_tab, Some(tab));
        assert_eq!(target.view_model.selected, Some(session));
        assert_eq!(target.terminal_search_query, "needle");
        assert!(target.terminal_search_open);
        assert!(std::sync::Arc::ptr_eq(
            target
                .terminal_surface
                .latest_snapshot()
                .unwrap_or_else(|| panic!("snapshot")),
            &snapshot
        ));
    }

    fn workspace_for_window(
        source: &super::workspace_model::DesktopWorkspace,
        id: cshell_domain::WorkspaceWindowId,
    ) -> super::workspace_model::DesktopWorkspace {
        super::workspace_model::DesktopWorkspace {
            document: source.document.clone(),
            sessions: source.sessions.clone(),
            pending_launch: None,
            writable: true,
            window_id: id,
        }
    }

    #[test]
    fn closing_terminal_search_releases_widget_focus_and_invalidates_pending_results() {
        let mut app=super::DesktopApp::default();
        app.terminal_search_open=true;
        app.terminal_search_focus_requested=true;
        app.terminal_search_error=Some("invalid expression".into());
        let id=egui::Id::new("search-focus-regression");
        app.egui_context.memory_mut(|m|m.request_focus(id));
        let revision=app.terminal_search_revision;
        app.close_terminal_search();
        assert!(!app.terminal_search_open && !app.terminal_search_focus_requested);
        assert!(app.egui_context.memory(|m|m.focused()).is_none());
        assert!(app.terminal_search_error.is_none());
        assert!(app.terminal_search_revision>revision);
    }

    #[test]
    fn tab_switch_preserves_search_state_and_cancels_stale_paste() {
        let mut app = super::DesktopApp::default();
        let first = app
            .workspace
            .add(None, "First".into())
            .unwrap_or_else(|| panic!("tab"));
        let second = app
            .workspace
            .add(None, "Second".into())
            .unwrap_or_else(|| panic!("tab"));
        app.switch_tab_view(first);
        app.terminal_search_query = "first query".into();
        app.terminal_search_open = true;
        app.terminal_search_options.case_sensitive = true;
        app.pending_paste = Some(super::PendingPaste {
            session_id: cshell_domain::SessionId::new(),
            session_title: "First".into(),
            text: "danger\n".into(),
        });
        let revision = app.terminal_search_revision;
        app.switch_tab_view(second);
        assert!(app.terminal_search_query.is_empty());
        assert!(!app.terminal_search_open);
        assert!(app.pending_paste.is_none());
        assert!(app.terminal_search_revision > revision);
        app.terminal_search_query = "second query".into();
        app.switch_tab_view(first);
        assert_eq!(app.terminal_search_query, "first query");
        assert!(app.terminal_search_open);
        assert!(app.terminal_search_options.case_sensitive);
        assert!(app.pending_paste.is_none());
        app.switch_tab_view(second);
        assert_eq!(app.terminal_search_query, "second query");
    }

    #[test]
    fn paste_shortcuts_require_a_single_pressed_command_chord() {
        let v = Key::Character("v".into());
        let insert = Key::Named(NamedKey::Insert);
        for modifiers in [
            ModifiersState::SUPER,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
        ] {
            assert!(matches_terminal_paste_shortcut(
                ElementState::Pressed,
                false,
                &v,
                modifiers
            ));
            assert!(!matches_terminal_paste_shortcut(
                ElementState::Released,
                false,
                &v,
                modifiers
            ));
            assert!(!matches_terminal_paste_shortcut(
                ElementState::Pressed,
                true,
                &v,
                modifiers
            ));
        }
        assert!(matches_terminal_paste_shortcut(
            ElementState::Pressed,
            false,
            &insert,
            ModifiersState::SHIFT
        ));
        assert!(!matches_terminal_paste_shortcut(
            ElementState::Pressed,
            false,
            &v,
            ModifiersState::CONTROL
        ));
        assert!(!matches_terminal_paste_shortcut(
            ElementState::Pressed,
            false,
            &v,
            ModifiersState::SUPER | ModifiersState::ALT
        ));
    }

    #[test]
    fn paste_decision_bounds_input_and_requires_explicit_confirmation_for_controls() {
        assert_eq!(
            decide_terminal_paste(Some("中文🙂".to_owned())),
            Ok(PasteDecision::Immediate("中文🙂".to_owned()))
        );
        for text in [
            "echo one\necho two",
            "echo one\r",
            "echo\tword",
            "x\u{1b}[31m",
        ] {
            assert_eq!(
                decide_terminal_paste(Some(text.to_owned())),
                Ok(PasteDecision::Confirm(text.to_owned()))
            );
        }
        assert!(decide_terminal_paste(None).is_err());
        assert!(decide_terminal_paste(Some(String::new())).is_err());
        assert!(
            decide_terminal_paste(Some("x".repeat(cshell_ipc::MAX_TERMINAL_INPUT_BYTES + 1)))
                .is_err()
        );
        assert_eq!(paste_preview("中\n\t\u{1b}"), "中\\n\n\\t\\u{1b}");
        assert!(paste_preview(&"🙂".repeat(401)).contains("仅预览前 400 个字符"));
    }

    #[test]
    fn scrollbar_maps_track_positions_to_stable_line_ids_and_tail_mode() {
        assert_eq!(
            log_scrollbar_action(0.0, 1_000),
            LogScrollbarAction::JumpTo(1)
        );
        assert_eq!(
            log_scrollbar_action(0.5, 1_000),
            LogScrollbarAction::JumpTo(501)
        );
        assert_eq!(
            log_scrollbar_action(1.0, 1_000),
            LogScrollbarAction::FollowTail
        );
    }

    #[test]
    fn terminal_pointer_mapping_is_bounded_and_rejects_viewport_edges() {
        let viewport = TerminalViewport {
            x: 10,
            y: 20,
            width: 800,
            height: 400,
        };
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(10.0, 20.0), viewport, 20, 80),
            Some(TerminalCellPoint { row: 0, column: 0 })
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(809.9, 419.9), viewport, 20, 80),
            Some(TerminalCellPoint {
                row: 19,
                column: 79,
            })
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(810.0, 100.0), viewport, 20, 80),
            None
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(100.0, 420.0), viewport, 20, 80),
            None
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(100.0, 100.0), viewport, 0, 80),
            None
        );
    }

    #[test]
    fn search_query_byte_limit_never_splits_utf8() {
        let mut query = "ab界cd".to_owned();
        truncate_utf8_bytes(&mut query, 4);
        assert_eq!(query, "ab");
        truncate_utf8_bytes(&mut query, 1);
        assert_eq!(query, "a");
    }

    #[test]
    fn triple_click_requires_the_same_cell_and_a_bounded_interval() {
        let mut state = TerminalClickState::default();
        let started = Instant::now();
        let point = TerminalCellPoint { row: 2, column: 3 };
        assert_eq!(register_terminal_click(&mut state, point, started), 1);
        assert_eq!(
            register_terminal_click(&mut state, point, started + Duration::from_millis(100)),
            2
        );
        assert_eq!(
            register_terminal_click(&mut state, point, started + Duration::from_millis(200)),
            3
        );
        assert_eq!(
            register_terminal_click(&mut state, point, started + Duration::from_millis(300)),
            1
        );
        assert_eq!(
            register_terminal_click(
                &mut state,
                TerminalCellPoint { row: 2, column: 4 },
                started + Duration::from_millis(350),
            ),
            1
        );
        assert_eq!(
            register_terminal_click(
                &mut state,
                TerminalCellPoint { row: 2, column: 4 },
                started + TERMINAL_MULTI_CLICK_INTERVAL + Duration::from_millis(400),
            ),
            1
        );
    }
}
mod connection_diagnostics;
