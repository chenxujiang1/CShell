//! Egui workbench controls. Terminal cells are never represented as egui widgets.

pub mod form;
pub mod feedback;
pub mod theme;
pub mod environment;
pub mod workbench_layout;

use cshell_domain::{
    FolderId, PaneId, ProfileFolder, ProfileId, ProfileKind, SessionId, SplitAxis, TabGroupId,
    TabId, WorkspaceDocument, WorkspacePaneContent, WorkspaceWindowId,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTabViewModel {
    pub id: SessionId,
    pub title: String,
    pub connected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedSshProfileViewModel {
    pub id: ProfileId,
    pub name: String,
    pub target: String,
    pub favorite: bool,
    pub folder_id: Option<FolderId>,
    pub kind: ProfileKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SshConnectionState {
    Connected,
    Connecting,
    Disconnecting,
    Reconnecting,
    Disconnected,
    Unconfirmed,
}

impl SshConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connected => "已连接",
            Self::Connecting => "连接中",
            Self::Disconnecting => "断开中",
            Self::Reconnecting => "重连中",
            Self::Disconnected => "已断开",
            Self::Unconfirmed => "状态待确认",
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceTabViewModel {
    pub id: TabId,
    pub title: String,
    pub session_id: Option<SessionId>,
}

#[derive(Clone, Debug)]
pub struct WorkspacePaneView {
    pub group_id: TabGroupId,
    pub tab_id: Option<TabId>,
    pub rect: egui::Rect,
}

#[derive(Debug, Default)]
pub struct TabNavigatorState {
    pub open: bool,
    pub query: String,
    pub selected: Option<TabId>,
    pub focus_requested: bool,
    pub return_focus_requested: bool,
}
impl TabNavigatorState {
    pub fn show(&mut self, current: Option<TabId>) {
        self.open = true;
        self.query.clear();
        self.selected = current;
        self.focus_requested = true;
        self.return_focus_requested = false;
    }
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = None;
        self.focus_requested = false;
        self.return_focus_requested = true;
    }
}

#[derive(Debug, Default)]
pub struct WorkbenchViewModel {
    pub toolbar_hidden: bool,
    pub connection_diagnostics_open: bool,
    pub profile_environments: std::collections::BTreeMap<ProfileId, environment::Environment>,
    pub terminal_font_pixels: Option<u16>,
    pub terminal_zoom_pixels: Option<u16>,
    pub sessions: Vec<SessionTabViewModel>,
    pub reusable_ssh_sessions: Vec<SessionId>,
    pub saved_ssh_profiles: Vec<SavedSshProfileViewModel>,
    pub profile_folders: Vec<ProfileFolder>,
    pub selected_profile: Option<ProfileId>,
    pub sidebar_hidden: bool,
    pub sidebar_width: Option<f32>,
    pub task_view: bool,
    pub layout_paused: bool,
    pub profile_search: String,
    pub tab_navigator: TabNavigatorState,
    pub tabs: Vec<WorkspaceTabViewModel>,
    pub selected_tab: Option<TabId>,
    pub workspace_document: Option<WorkspaceDocument>,
    pub workspace_window_id: Option<WorkspaceWindowId>,
    pub available_windows: Vec<WorkspaceWindowId>,
    pub pane_views: Vec<WorkspacePaneView>,
    pub workspace_status: String,
    pub selected: Option<SessionId>,
    pub daemon_connected: bool,
    pub daemon_status_detail: String,
    pub terminal_reading_status: Option<String>,
    pub vault_status: Option<String>,
    pub can_reconnect_ssh: bool,
    pub can_disconnect_ssh: bool,
    pub can_restart_ssh: bool,
    pub ssh_control_busy: bool,
    pub ssh_tab_states: std::collections::BTreeMap<TabId, SshConnectionState>,
    /// Selected recipients: false = command bar, true = live keyboard broadcast.
    pub input_target_tabs: std::collections::BTreeMap<TabId, bool>,
    pub unread_output_tabs: std::collections::BTreeSet<TabId>,
    pub can_sftp: bool,
    pub input_warning: Option<String>,
    pub terminal_generation: Option<u64>,
    pub about_open: bool,
    pub menu_command: Option<WorkbenchMenuCommand>,
    pub confirm_terminate: bool,
    pub terminate_target: Option<SessionId>,
    pub terminate_pending: bool,
    pub terminate_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkbenchMenuCommand {
    ShellHistory,
    CommandCenter,
    KeyboardSettings,
    GlobalSettings,
    VaultSettings,
    Credentials,
    TerminalUploads,
    ZmodemTransfers,
    SearchTerminal,
    Profiles,
    SshConnections,
    SftpFiles,
    Tunnels,
    HostCa,
    SshProviders,
    TerminalTools,
    ProfileBatch,
    TerminalPolicies,
    WorkspaceRecovery,
    DetachWorkspaceGroup,
    RetryWorkspaceWindows,
    ClipboardPolicy,
    Hosts,
    Tasks,
    UploadTask,
    DownloadTask,
    MixedTask,
    TerminalView,
    SyncInput,
    Compose,
    QuickCommands,
    LoadQuickCommand(cshell_domain::QuickCommandId),
    NewSshProfile,
    OpenSavedProfile(ProfileId),
    OpenSavedSftp(ProfileId),
    EditSavedProfile(ProfileId),
    ReconnectNewShell,
    DisconnectSsh,
    ReconnectSsh,
    ReconnectSshTab(TabId),
    CloseView,
    TerminateSession(SessionId),
    NewWorkspaceTab,
    CloneWorkspaceSession(TabId),
    CloneSshChannel(TabId),
    NewWorkspaceWindow,
    MoveTabToWindow(TabId, WorkspaceWindowId),
    OpenWorkspaceTab(TabId),
    CloseWorkspaceTab(TabId),
    MoveWorkspaceTab(TabId, isize),
    MoveTabToGroup(TabId, TabGroupId),
    SplitWorkspace(SplitAxis),
    ResizeSplit(PaneId, u16),
    Quit,
}

#[cfg(test)]
mod menu_layout_tests;
#[cfg(test)]
mod polish_tests;
mod workbench;
pub mod workbench_icons;
pub use workbench::{
    draw_workbench, draw_workbench_chrome, draw_workbench_content, install_workbench_style,
    searchable_window_tabs,
};

#[cfg(test)]
mod tests {
    use super::{WorkbenchMenuCommand, WorkbenchViewModel, draw_workbench};

    pub(super) fn classic_context() -> egui::Context {
        let context = egui::Context::default();
        super::install_workbench_style(&context);
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "workbench-cjk".into(),
            std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                "../../../apps/desktop/assets/fonts/NotoSansSC-Regular.ttf"
            ))),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("workbench-cjk".into());
        context.set_fonts(fonts);
        context
    }

    fn frame(
        context: &egui::Context,
        model: &mut WorkbenchViewModel,
        events: Vec<egui::Event>,
    ) -> egui::accesskit::TreeUpdate {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1_200.0, 760.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let mut output = context.run_ui(raw, |ui| {
            let _terminal_rect = draw_workbench(ui, model);
        });
        output.textures_delta.clear();
        let Some(update) = output.platform_output.accesskit_update else {
            panic!("an active accessibility client should receive a tree");
        };
        update
    }

    pub(super) fn button_center(update: &egui::accesskit::TreeUpdate, label: &str) -> egui::Pos2 {
        let Some((_, node)) = update.nodes.iter().find(|(_, node)| {
            node.role() == egui::accesskit::Role::Button && node.label() == Some(label)
        }) else {
            panic!("button {label} should be in the accessibility tree");
        };
        let Some(bounds) = node.bounds() else {
            panic!("button {label} should have bounds");
        };
        egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        )
    }

    fn click(
        context: &egui::Context,
        model: &mut WorkbenchViewModel,
        position: egui::Pos2,
    ) -> egui::accesskit::TreeUpdate {
        frame(
            context,
            model,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        )
    }

    fn split_document() -> cshell_domain::WorkspaceDocument {
        use cshell_domain::*;
        let mut doc = WorkspaceDocument::default();
        let group = TabGroupId::new();
        let first = PaneId::new();
        let second = PaneId::new();
        let old_group = doc.tab_groups[0].id;
        doc.panes[0].content = WorkspacePaneContent::Split {
            axis: SplitAxis::Horizontal,
            ratio_permille: 500,
            first,
            second,
        };
        doc.panes.extend([
            WorkspacePane {
                id: first,
                content: WorkspacePaneContent::Tabs {
                    group_id: old_group,
                },
            },
            WorkspacePane {
                id: second,
                content: WorkspacePaneContent::Tabs { group_id: group },
            },
        ]);
        doc.tab_groups.push(WorkspaceTabGroup {
            id: group,
            tabs: vec![],
            active_tab: None,
        });
        for (index, title) in ["Left pane", "Right pane"].into_iter().enumerate() {
            let tab = TabId::new();
            doc.bindings.push(WorkspaceBinding {
                tab_id: tab,
                profile_id: None,
                title: title.into(),
            });
            doc.tab_groups[index].tabs.push(tab);
            doc.tab_groups[index].active_tab = Some(tab);
        }
        doc
    }

    #[test]
    fn split_controls_select_group_tabs_and_drag_the_divider() {
        let context = classic_context();
        context.enable_accesskit();
        let doc = split_document();
        let root = doc.windows[0].root;
        let mut model = WorkbenchViewModel {
            selected_tab: doc.tab_groups[0].active_tab,
            workspace_document: Some(doc.clone()),
            ..Default::default()
        };
        frame(&context, &mut model, vec![]);
        let update = frame(&context, &mut model, vec![]);
        let split_menu = button_center(&update, "分屏");
        click(&context, &mut model, split_menu);
        let update = frame(&context, &mut model, vec![]);
        let split = button_center(&update, "向下分屏");
        click(&context, &mut model, split);
        assert_eq!(
            model.menu_command.take(),
            Some(WorkbenchMenuCommand::SplitWorkspace(
                cshell_domain::SplitAxis::Vertical
            ))
        );
        let update = frame(&context, &mut model, vec![]);
        let (_, node) = update
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some("Right pane"))
            .unwrap_or_else(|| panic!("right pane tab"));
        let bounds = node.bounds().unwrap_or_else(|| panic!("tab bounds"));
        click(
            &context,
            &mut model,
            egui::pos2(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            ),
        );
        assert_eq!(model.selected_tab, doc.tab_groups[1].active_tab);
        let left = model.pane_views[0].rect;
        let right = model.pane_views[1].rect;
        assert!(left.max.x < right.min.x);
        let start = egui::pos2((left.max.x + right.min.x) / 2.0, left.center().y);
        frame(
            &context,
            &mut model,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            &context,
            &mut model,
            vec![egui::Event::PointerMoved(start + egui::vec2(60.0, 0.0))],
        );
        assert!(
            matches!(model.menu_command, Some(WorkbenchMenuCommand::ResizeSplit(id, ratio)) if id == root && ratio > 500 && ratio <= 900)
        );
    }

    #[test]
    fn second_window_uses_its_own_pane_root_and_exposes_creation() {
        let context = classic_context();
        context.enable_accesskit();
        let mut doc = split_document();
        let second_window = cshell_domain::WorkspaceWindowId::new();
        let second_group = cshell_domain::TabGroupId::new();
        let second_root = cshell_domain::PaneId::new();
        let tab = cshell_domain::TabId::new();
        doc.windows.push(cshell_domain::WorkspaceWindow {
            id: second_window,
            root: second_root,
            focused_group: second_group,
        });
        doc.panes.push(cshell_domain::WorkspacePane {
            id: second_root,
            content: cshell_domain::WorkspacePaneContent::Tabs {
                group_id: second_group,
            },
        });
        doc.tab_groups.push(cshell_domain::WorkspaceTabGroup {
            id: second_group,
            tabs: vec![tab],
            active_tab: Some(tab),
        });
        doc.bindings.push(cshell_domain::WorkspaceBinding {
            tab_id: tab,
            profile_id: None,
            title: "Other window".into(),
        });
        let mut model = WorkbenchViewModel {
            selected_tab: Some(tab),
            workspace_window_id: Some(second_window),
            workspace_document: Some(doc),
            tabs: vec![super::WorkspaceTabViewModel {
                id: tab,
                title: "Other window".into(),
                session_id: None,
            }],
            ..Default::default()
        };
        frame(&context, &mut model, vec![]);
        let update = frame(&context, &mut model, vec![]);
        assert_eq!(model.pane_views.len(), 1);
        assert_eq!(model.pane_views[0].group_id, second_group);
        let file = button_center(&update, "文件");
        click(&context, &mut model, file);
        let menu = frame(&context, &mut model, vec![]);
        let create = button_center(&menu, "新建窗口");
        click(&context, &mut model, create);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::NewWorkspaceWindow)
        );
    }

    #[test]
    fn tab_context_menu_moves_to_a_live_other_window() {
        let context = classic_context();
        context.enable_accesskit();
        let mut doc = split_document();
        let source = doc.windows[0].id;
        let target = cshell_domain::WorkspaceWindowId::new();
        let group = cshell_domain::TabGroupId::new();
        let root = cshell_domain::PaneId::new();
        doc.windows.push(cshell_domain::WorkspaceWindow {
            id: target,
            root,
            focused_group: group,
        });
        doc.panes.push(cshell_domain::WorkspacePane {
            id: root,
            content: cshell_domain::WorkspacePaneContent::Tabs { group_id: group },
        });
        doc.tab_groups.push(cshell_domain::WorkspaceTabGroup {
            id: group,
            tabs: vec![],
            active_tab: None,
        });
        let tab = doc.tab_groups[0]
            .active_tab
            .unwrap_or_else(|| panic!("tab"));
        let mut model = WorkbenchViewModel {
            selected_tab: Some(tab),
            workspace_window_id: Some(source),
            available_windows: vec![source, target],
            workspace_document: Some(doc),
            tabs: vec![super::WorkspaceTabViewModel {
                id: tab,
                title: "Left pane".into(),
                session_id: None,
            }],
            ..Default::default()
        };
        frame(&context, &mut model, vec![]);
        let update = frame(&context, &mut model, vec![]);
        let (_, node) = update
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some("Left pane"))
            .unwrap_or_else(|| panic!("workspace tab"));
        let bounds = node.bounds().unwrap_or_else(|| panic!("tab bounds"));
        let position = egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        frame(
            &context,
            &mut model,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Secondary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Secondary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let menu = frame(&context, &mut model, vec![]);
        let move_button = button_center(&menu, "移到窗口 2");
        click(&context, &mut model, move_button);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::MoveTabToWindow(tab, target))
        );
    }

    #[test]
    fn clone_button_requires_running_tab_and_emits_explicit_command() {
        let context = classic_context();
        context.enable_accesskit();
        let tab = cshell_domain::TabId::new();
        let mut model = WorkbenchViewModel {
            selected_tab: Some(tab),
            tabs: vec![super::WorkspaceTabViewModel {
                id: tab,
                title: "Saved".into(),
                session_id: None,
            }],
            ..Default::default()
        };
        let _ = frame(&context, &mut model, vec![]);
        let update = frame(&context, &mut model, vec![]);
        let session_menu = button_center(&update, "会话");
        click(&context, &mut model, session_menu);
        let update = frame(&context, &mut model, vec![]);
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("复制为新会话") && node.is_disabled())
        );
        model.tabs[0].session_id = Some(cshell_domain::SessionId::new());
        let update = frame(&context, &mut model, vec![]);
        let clone = button_center(&update, "复制为新会话");
        click(&context, &mut model, clone);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::CloneWorkspaceSession(tab))
        );
    }

    #[test]
    fn shared_ssh_copy_menu_separates_channel_from_independent_session() {
        let context=classic_context();context.enable_accesskit();
        let tab=cshell_domain::TabId::new();let source=cshell_domain::SessionId::new();
        let mut model=WorkbenchViewModel{selected_tab:Some(tab),tabs:vec![super::WorkspaceTabViewModel{id:tab,title:"SSH host".into(),session_id:Some(source)}],reusable_ssh_sessions:vec![source],..Default::default()};
        model.ssh_tab_states.insert(tab,super::SshConnectionState::Connected);
        for _ in 0..2{frame(&context,&mut model,vec![]);}
        let tree=frame(&context,&mut model,vec![]);click(&context,&mut model,button_center(&tree,"会话"));
        let tree=frame(&context,&mut model,vec![]);let _=button_center(&tree,"复制 SSH 会话");
        click(&context,&mut model,button_center(&tree,"复制 SSH 渠道"));assert_eq!(model.menu_command,Some(WorkbenchMenuCommand::CloneSshChannel(tab)));
        model.menu_command=None;let tree=frame(&context,&mut model,vec![]);click(&context,&mut model,button_center(&tree,"会话"));
        let tree=frame(&context,&mut model,vec![]);click(&context,&mut model,button_center(&tree,"复制 SSH 会话"));assert_eq!(model.menu_command,Some(WorkbenchMenuCommand::CloneWorkspaceSession(tab)));
        model.reusable_ssh_sessions.clear();let tree=frame(&context,&mut model,vec![]);click(&context,&mut model,button_center(&tree,"会话"));
        let tree=frame(&context,&mut model,vec![]);assert!(tree.nodes.iter().any(|(_,n)|n.label()==Some("复制 SSH 渠道") && n.is_disabled()));
    }

    #[test]
    fn tiny_split_layout_keeps_pane_rectangles_nonnegative() {
        let context = classic_context();
        let doc = split_document();
        let mut model = WorkbenchViewModel::default();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            super::workbench::draw_pane(
                ui,
                &mut model,
                &doc,
                doc.windows[0].root,
                egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(3.0, 4.0)),
                0,
            );
        });
        output.textures_delta.clear();
        assert_eq!(model.pane_views.len(), 2);
        assert!(
            model
                .pane_views
                .iter()
                .all(|pane| pane.rect.width() >= 0.0 && pane.rect.height() >= 0.0)
        );
    }

    #[test]
    fn workspace_placeholder_requires_explicit_open_and_can_close_without_a_session() {
        let context = classic_context();
        context.enable_accesskit();
        let tab = cshell_domain::TabId::new();
        let mut model = WorkbenchViewModel {
            tabs: vec![super::WorkspaceTabViewModel {
                id: tab,
                title: "Restored".into(),
                session_id: None,
            }],
            selected_tab: Some(tab),
            ..Default::default()
        };
        let _initial = frame(&context, &mut model, vec![]);
        let update = frame(&context, &mut model, vec![]);
        assert!(model.menu_command.is_none());
        let open = button_center(&update, "连接此会话");
        click(&context, &mut model, open);
        assert_eq!(
            model.menu_command.take(),
            Some(WorkbenchMenuCommand::OpenWorkspaceTab(tab))
        );
        let update = frame(&context, &mut model, vec![]);
        let close = button_center(&update, "×");
        click(&context, &mut model, close);
        assert_eq!(
            model.menu_command.take(),
            Some(WorkbenchMenuCommand::CloseWorkspaceTab(tab))
        );
        let update = frame(&context, &mut model, vec![]);
        let new = button_center(&update, "+");
        click(&context, &mut model, new);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::NewWorkspaceTab)
        );
    }

    #[test]
    fn about_dialog_appears_in_accessibility_tree() {
        let context = classic_context();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel {
            about_open: true,
            ..WorkbenchViewModel::default()
        };
        let update = frame(&context, &mut model, Vec::new());
        assert!(
            update.nodes.iter().any(|(_, node)| {
                node.value()
                    .is_some_and(|value| value.contains("关于 CShell"))
            }),
            "accessibility nodes: {:?}",
            update
                .nodes
                .iter()
                .map(|(_, node)| (node.role(), node.label(), node.value()))
                .collect::<Vec<_>>()
        );
        assert!(update.nodes.iter().any(|(_, node)| {
            node.label() == Some("关闭") && node.role() == egui::accesskit::Role::Button
        }));
        assert!(model.about_open);
    }

    #[test]
    fn menu_search_action_is_dispatched_from_pointer_input() {
        let context = classic_context();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel::default();
        let first = frame(&context, &mut model, Vec::new());
        let menu_position = button_center(&first, "编辑");
        let _opened = click(&context, &mut model, menu_position);
        let menu = frame(&context, &mut model, Vec::new());
        let search_position = button_center(&menu, "查找终端");
        let _selected = click(&context, &mut model, search_position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::SearchTerminal)
        );
    }

    #[test]
    fn ssh_connection_entry_is_visible_without_opening_a_menu() {
        let context = classic_context();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel::default();
        let first = frame(&context, &mut model, Vec::new());
        let position = button_center(&first, "会话管理");
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::SshConnections)
        );
    }

    #[test]
    fn saved_profile_context_menu_can_open_sftp_without_any_terminal() {
        let context=classic_context();context.enable_accesskit();let id=cshell_domain::ProfileId::new();
        let mut model=WorkbenchViewModel{saved_ssh_profiles:vec![super::SavedSshProfileViewModel{id,name:"SFTP only".into(),target:"files@example.test:22".into(),favorite:false,folder_id:None,kind:cshell_domain::ProfileKind::Ssh}],..Default::default()};
        let tree=frame(&context,&mut model,vec![]);let pos=button_center(&tree,"SFTP only");
        for pressed in [true,false] {frame(&context,&mut model,vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton{pos,button:egui::PointerButton::Secondary,pressed,modifiers:Default::default()}]);}
        for _ in 0..3 {frame(&context,&mut model,vec![]);}
        let tree=frame(&context,&mut model,vec![]);let target=button_center(&tree,"打开 SFTP");click(&context,&mut model,target);
        assert_eq!(model.menu_command,Some(WorkbenchMenuCommand::OpenSavedSftp(id)));assert!(model.sessions.is_empty());
    }

    #[test]
    fn saved_ssh_connections_can_be_opened_from_the_workbench() {
        let context = classic_context();
        context.enable_accesskit();
        let id = cshell_domain::ProfileId::new();
        let mut model = WorkbenchViewModel {
            saved_ssh_profiles: vec![super::SavedSshProfileViewModel {
                id,
                name: "Production".into(),
                target: "alice@example.test:22".into(),
                favorite: false,
                folder_id: None,
                kind: cshell_domain::ProfileKind::Ssh,
            }],
            ..WorkbenchViewModel::default()
        };
        let first = frame(&context, &mut model, Vec::new());
        let position = button_center(&first, "Production");
        click(&context, &mut model, position);
        assert_eq!(model.selected_profile, Some(id));
        assert!(model.menu_command.is_none());
        let selected = frame(&context, &mut model, Vec::new());
        let connect = button_center(&selected, "连接");
        click(&context, &mut model, connect);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::OpenSavedProfile(id))
        );

        model.menu_command = None;
        let update = frame(&context, &mut model, Vec::new());
        let position = button_center(&update, "编辑");
        click(&context, &mut model, position);
        let menu = frame(&context, &mut model, Vec::new());
        let edit = button_center(&menu, "编辑选中会话…");
        click(&context, &mut model, edit);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::EditSavedProfile(id))
        );

        model.menu_command = None;
        let update = frame(&context, &mut model, Vec::new());
        let position = button_center(&update, "＋ 新建");
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::NewSshProfile)
        );
    }

    #[test]
    fn termination_confirmation_keeps_its_original_target_after_selection_changes() {
        let original = cshell_domain::SessionId::new();
        let another = cshell_domain::SessionId::new();
        let context = classic_context();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel {
            selected: Some(another),
            daemon_connected: true,
            confirm_terminate: true,
            terminate_target: Some(original),
            ..WorkbenchViewModel::default()
        };
        // Let the native dialog settle its initial size before pointer hit testing.
        let _initial = frame(&context, &mut model, vec![]);
        let update = frame(&context, &mut model, vec![]);
        let position = button_center(&update, "终止进程");
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::TerminateSession(original))
        );
        assert!(model.confirm_terminate);
        assert!(model.terminate_pending);
        assert_eq!(model.terminate_target, Some(original));
    }

    #[test]
    fn opened_session_remove_targets_its_row_without_changing_selection() {
        let context = classic_context();
        context.enable_accesskit();
        let target = cshell_domain::SessionId::new();
        let active = cshell_domain::SessionId::new();
        let mut model = WorkbenchViewModel {
            selected: Some(active),
            sessions: vec![super::SessionTabViewModel { id: target, title: "Retained session".into(), connected: false }],
            ..Default::default()
        };
        let first = frame(&context, &mut model, vec![]);
        let expand = button_center(&first, "已打开的会话");
        click(&context, &mut model, expand);
        let tree = frame(&context, &mut model, vec![]);
        let remove = button_center(&tree, "×");
        click(&context, &mut model, remove);
        assert_eq!(model.selected, Some(active));
        assert_eq!(model.terminate_target, Some(target));
        assert!(model.confirm_terminate);
        assert!(model.menu_command.is_none());
        let _ = frame(&context, &mut model, vec![]);
        let tree = frame(&context, &mut model, vec![]);
        let cancel = button_center(&tree, "取消");
        click(&context, &mut model, cancel);
        assert!(!model.confirm_terminate);
        assert!(model.menu_command.is_none());
    }

    #[test]
    fn classic_layout_reserves_terminal_space_and_has_one_tab_row() {
        let mut reports = Vec::new();
        for (width, height) in [(1280.0, 800.0), (900.0, 600.0)] {
            let context = classic_context();
            context.enable_accesskit();
            let tab = cshell_domain::TabId::new();
            let mut model = WorkbenchViewModel {
                selected_tab: Some(tab),
                tabs: vec![super::WorkspaceTabViewModel {
                    id: tab,
                    title: "Unique session tab".into(),
                    session_id: Some(cshell_domain::SessionId::new()),
                }],
                workspace_status: "Very long diagnostic detail ".repeat(500),
                ..Default::default()
            };
            let mut terminal = egui::Rect::NOTHING;
            let mut output = None;
            for _ in 0..2 {
                let mut rendered = context.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, height),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        super::draw_workbench_chrome(ui, &mut model);
                        egui::Panel::bottom("test-send-controls")
                            .exact_size(30.0)
                            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(8, 2)))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.button("同步输入").clicked();
                                    ui.button("发送栏 / 快捷命令").clicked();
                                });
                            });
                        terminal = super::draw_workbench_content(ui, &mut model);
                    },
                );
                rendered.textures_delta.clear();
                output = Some(rendered);
            }
            assert!(
                terminal.width() >= width * 0.70,
                "terminal {terminal:?} at {width}x{height}"
            );
            assert!(
                terminal.height() >= height * 0.75,
                "terminal {terminal:?} at {width}x{height}"
            );
            assert!(terminal.min.x >= 180.0 && terminal.max.x <= width + 1.0);
            assert!(terminal.min.y >= 90.0 && terminal.max.y <= height);
            let update = output
                .and_then(|o| o.platform_output.accesskit_update)
                .unwrap_or_else(|| panic!("tree"));
            assert_eq!(
                update
                    .nodes
                    .iter()
                    .filter(|(_, n)| n.role() == egui::accesskit::Role::Button
                        && n.label() == Some("Unique session tab"))
                    .count(),
                1
            );
            for hidden in [
                "CA 与证书…",
                "SSH 能力…",
                "Workspace recovery",
                "批量编辑 / 继承来源…",
            ] {
                assert!(
                    !update.nodes.iter().any(|(_, n)| n.label() == Some(hidden)),
                    "advanced control leaked into default layout"
                );
            }
            reports.push(format!("{{\"width\":{width},\"height\":{height},\"terminal_x\":{},\"terminal_y\":{},\"terminal_width\":{},\"terminal_height\":{}}}", terminal.min.x, terminal.min.y, terminal.width(), terminal.height()));
        }
        if let Some(path) = std::env::var_os("CSHELL_UI_LAYOUT_REPORT") {
            std::fs::write(path, format!("[{}]", reports.join(",")))
                .unwrap_or_else(|e| panic!("layout report: {e}"));
        }
    }

    #[test]
    fn sidebar_fits_content_resizes_and_collapses_with_one_click() {
        let context = classic_context();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel::default();
        frame(&context, &mut model, vec![]);
        frame(&context, &mut model, vec![]);
        let panel_width = || egui::containers::panel::PanelState::load(&context, egui::Id::new("classic-session-tree")).unwrap().size().x;
        let initial = panel_width();
        model.saved_ssh_profiles.push(super::SavedSshProfileViewModel {
            id: cshell_domain::ProfileId::new(), name: "Production SSH server with a long name".into(),
            target: "operator@very-long-production-host.example.test:2222".into(),
            favorite: false, folder_id: None, kind: cshell_domain::ProfileKind::Ssh,
        });
        frame(&context, &mut model, vec![]);
        let tree = frame(&context, &mut model, vec![]);
        let expanded = panel_width();
        assert!(expanded > initial + 40.0, "sidebar must grow for content: {initial} -> {expanded}");
        assert!(expanded <= 480.0 + 1.0, "auto width must leave terminal space");
        let collapse = button_center(&tree, "收起会话管理器");
        click(&context, &mut model, collapse);
        assert!(model.sidebar_hidden);
        let tree = frame(&context, &mut model, vec![]);
        let expand = button_center(&tree, "展开会话管理器");
        click(&context, &mut model, expand);
        assert!(!model.sidebar_hidden);
        frame(&context, &mut model, vec![]);
        let edge = egui::pos2(panel_width(), 350.0);
        frame(&context, &mut model, vec![egui::Event::PointerMoved(edge), egui::Event::PointerButton { pos: edge, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let dragged = edge - egui::vec2(100.0, 0.0);
        frame(&context, &mut model, vec![egui::Event::PointerMoved(dragged)]);
        frame(&context, &mut model, vec![egui::Event::PointerMoved(dragged)]);
        frame(&context, &mut model, vec![egui::Event::PointerButton { pos: dragged, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        let tree = frame(&context, &mut model, vec![]);
        assert!(model.sidebar_width.is_some(), "edge drag must enable manual sizing");
        let manual = panel_width();
        assert!(manual < expanded - 40.0, "manual drag must change width");
        assert!((manual - dragged.x).abs() <= 1.0, "release must stay at the pointer: {manual} vs {}", dragged.x);
        for _ in 0..120 {
            frame(&context, &mut model, vec![egui::Event::PointerMoved(egui::pos2(1000.0, 500.0))]);
            assert!((panel_width() - manual).abs() < 1.0, "manual width drifted after release: {manual} -> {}", panel_width());
        }
        let tree = frame(&context, &mut model, vec![]);
        click(&context, &mut model, button_center(&tree, "收起会话管理器"));
        let tree = frame(&context, &mut model, vec![]);
        click(&context, &mut model, button_center(&tree, "展开会话管理器"));
        let tree = frame(&context, &mut model, vec![]);
        assert!((panel_width() - manual).abs() < 1.0, "collapse/expand must retain the released width");
        let fit = button_center(&tree, "自动适应侧栏宽度");
        click(&context, &mut model, fit);
        frame(&context, &mut model, vec![]);
        assert!(model.sidebar_width.is_none());
        assert!((panel_width() - expanded).abs() < 1.0);
    }

    #[test]
    fn long_folder_does_not_expand_sidebar_after_drag_release() {
        let context = classic_context();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel {
            profile_folders: vec![cshell_domain::ProfileFolder {
                id: cshell_domain::FolderId::new(), name: "生产环境部署会话分组".repeat(12),
                parent_id: None, terminal: Default::default(),
            }], ..Default::default()
        };
        for _ in 0..2 { frame(&context, &mut model, vec![]); }
        let width = || egui::containers::panel::PanelState::load(&context, egui::Id::new("classic-session-tree")).unwrap().size().x;
        let edge = egui::pos2(width(), 350.0);
        frame(&context, &mut model, vec![egui::Event::PointerMoved(edge), egui::Event::PointerButton {pos: edge, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE}]);
        let end = egui::pos2(300.0, 350.0);
        frame(&context, &mut model, vec![egui::Event::PointerMoved(end)]);
        frame(&context, &mut model, vec![egui::Event::PointerMoved(end)]);
        frame(&context, &mut model, vec![
            egui::Event::PointerButton {pos: end, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE},
            egui::Event::PointerMoved(egui::pos2(1000.0, 500.0)),
        ]);
        for _ in 0..120 {
            frame(&context, &mut model, vec![egui::Event::PointerMoved(egui::pos2(1000.0, 500.0))]);
            assert!((width() - end.x).abs() <= 1.0, "folder content expanded the released sidebar: {}", width());
        }
    }

    #[test]
    fn session_tree_search_keeps_folder_ancestors_and_double_click_connects() {
        let context = classic_context();
        context.enable_accesskit();
        let folder = cshell_domain::FolderId::new();
        let id = cshell_domain::ProfileId::new();
        let mut model = WorkbenchViewModel {
            profile_search: "host.example".into(),
            profile_folders: vec![cshell_domain::ProfileFolder {
                id: folder,
                name: "Team".into(),
                parent_id: None,
                terminal: Default::default(),
            }],
            saved_ssh_profiles: vec![super::SavedSshProfileViewModel {
                id,
                name: "Server".into(),
                target: "operator@host.example:22".into(),
                favorite: false,
                folder_id: Some(folder),
                kind: cshell_domain::ProfileKind::Ssh,
            }],
            ..Default::default()
        };
        frame(&context, &mut model, vec![]);
        let update = frame(&context, &mut model, vec![]);
        assert!(update.nodes.iter().any(|(_, n)| n.label() == Some("Team")));
        let position = button_center(&update, "Server");
        click(&context, &mut model, position);
        assert_eq!(model.selected_profile, Some(id));
        assert!(model.menu_command.is_none());
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::OpenSavedProfile(id))
        );
    }
}

pub mod brand;
