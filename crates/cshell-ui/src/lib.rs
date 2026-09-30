//! Egui workbench controls. Terminal cells are never represented as egui widgets.

use cshell_domain::{
    PaneId, ProfileId, SessionId, SplitAxis, TabGroupId, TabId, WorkspaceDocument,
    WorkspacePaneContent, WorkspaceWindowId,
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
pub struct WorkbenchViewModel {
    pub sessions: Vec<SessionTabViewModel>,
    pub saved_ssh_profiles: Vec<SavedSshProfileViewModel>,
    pub profile_search: String,
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
    pub can_reconnect_ssh: bool,
    pub input_warning: Option<String>,
    pub terminal_generation: Option<u64>,
    pub about_open: bool,
    pub menu_command: Option<WorkbenchMenuCommand>,
    pub confirm_terminate: bool,
    pub terminate_target: Option<SessionId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkbenchMenuCommand {
    SearchTerminal,
    Profiles,
    SshConnections,
    SftpFiles,
    ClipboardPolicy,
    NewSshProfile,
    OpenSavedProfile(ProfileId),
    EditSavedProfile(ProfileId),
    ReconnectNewShell,
    CloseView,
    TerminateSession(SessionId),
    NewWorkspaceTab,
    CloneWorkspaceSession(TabId),
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

/// Draws the native workbench chrome and returns the logical-point rectangle
/// reserved for the GPU terminal surface.
#[must_use]
pub fn draw_workbench(ui: &mut egui::Ui, model: &mut WorkbenchViewModel) -> egui::Rect {
    egui::Panel::top("top_bar").exact_size(36.0).show(ui, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("菜单", |ui| {
                if ui.button("查找终端").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::SearchTerminal);
                    ui.close();
                }
                if ui.button("管理连接配置").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::Profiles);
                    ui.close();
                }
                if ui.button("关于 CShell").clicked() {
                    model.about_open = true;
                    ui.close();
                }
                if ui.button("剪贴板安全策略").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::ClipboardPolicy);
                    ui.close();
                }
                ui.separator();
                if ui.button("退出").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::Quit);
                    ui.close();
                }
            });
            ui.separator();
            ui.heading("CShell");
            if ui.button("SSH 连接").clicked() {
                model.menu_command = Some(WorkbenchMenuCommand::SshConnections);
            }
            if ui
                .add_enabled(model.selected.is_some(), egui::Button::new("SFTP 文件"))
                .clicked()
            {
                model.menu_command = Some(WorkbenchMenuCommand::SftpFiles);
            }
            ui.menu_button("会话", |ui| {
                if ui
                    .add_enabled(
                        model.can_reconnect_ssh,
                        egui::Button::new("重新连接并打开新 Shell"),
                    )
                    .clicked()
                {
                    model.menu_command = Some(WorkbenchMenuCommand::ReconnectNewShell);
                    ui.close();
                }
                if ui
                    .add_enabled(model.daemon_connected, egui::Button::new("关闭当前视图"))
                    .clicked()
                {
                    model.menu_command = Some(WorkbenchMenuCommand::CloseView);
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(model.daemon_connected, egui::Button::new("终止会话进程"))
                    .clicked()
                {
                    model.terminate_target = model.selected;
                    model.confirm_terminate = model.terminate_target.is_some();
                    ui.close();
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let status = if model.daemon_connected {
                    "● 终端就绪"
                } else {
                    "○ 终端未连接"
                };
                ui.label(status).on_hover_text(&model.daemon_status_detail);
                if let Some(warning) = &model.input_warning {
                    ui.colored_label(egui::Color32::LIGHT_RED, warning);
                }
            });
        });
    });

    if model.confirm_terminate {
        egui::Window::new("Terminate this process?")
            .collapsible(false).resizable(false)
            .show(ui.ctx(), |ui| {
                if let Some(id) = model.terminate_target {
                    let title = model.sessions.iter().find(|item| item.id == id)
                        .map_or("Session", |item| item.title.as_str());
                    ui.label(format!("{title} ({id})"));
                }
                ui.label("The process and its children will be stopped. This cannot resume the running command.");
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { model.confirm_terminate = false; }
                    if ui.button("Terminate").clicked() {
                        model.confirm_terminate = false;
                        if let Some(id) = model.terminate_target.take() {
                            model.menu_command = Some(WorkbenchMenuCommand::TerminateSession(id));
                        }
                    }
                });
            });
    }

    egui::Panel::left("session_list")
        .resizable(true)
        .default_size(248.0)
        .show(ui, |ui| {
            ui.heading("连接");
            if ui.button("＋ 新建 SSH").clicked() {
                model.menu_command = Some(WorkbenchMenuCommand::NewSshProfile);
            }
            ui.add(
                egui::TextEdit::singleline(&mut model.profile_search).hint_text("搜索已保存连接"),
            );
            ui.separator();
            ui.label("已保存的 SSH 连接");
            let search = model.profile_search.trim().to_lowercase();
            let mut visible = 0;
            egui::ScrollArea::vertical()
                .id_salt("saved_ssh_profiles")
                .show(ui, |ui| {
                    for profile in &model.saved_ssh_profiles {
                        if !search.is_empty()
                            && !profile.name.to_lowercase().contains(&search)
                            && !profile.target.to_lowercase().contains(&search)
                        {
                            continue;
                        }
                        visible += 1;
                        let name = if profile.favorite {
                            format!("★ {}", profile.name)
                        } else {
                            profile.name.clone()
                        };
                        ui.horizontal(|ui| {
                            if ui.button("连接").on_hover_text(&profile.target).clicked() {
                                model.menu_command =
                                    Some(WorkbenchMenuCommand::OpenSavedProfile(profile.id));
                            }
                            ui.label(name);
                            if ui.small_button("编辑").clicked() {
                                model.menu_command =
                                    Some(WorkbenchMenuCommand::EditSavedProfile(profile.id));
                            }
                        });
                        ui.small(&profile.target);
                    }
                    if visible == 0 {
                        ui.weak(if search.is_empty() {
                            "还没有保存的 SSH 连接"
                        } else {
                            "没有匹配的连接"
                        });
                    }
                    ui.separator();
                    ui.label("当前会话");
                    for session in &model.sessions {
                        let selected = model.selected == Some(session.id);
                        let state = if session.connected { "●" } else { "○" };
                        if ui
                            .selectable_label(selected, format!("{state} {}", session.title))
                            .clicked()
                        {
                            model.selected = Some(session.id);
                        }
                    }
                });
        });

    egui::Panel::top("workspace_tabs").show(ui, |ui| {
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("+ New tab").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::NewWorkspaceTab);
                }
                if let Some(tab) = model
                    .tabs
                    .iter()
                    .find(|tab| Some(tab.id) == model.selected_tab)
                    && ui
                        .add_enabled(tab.session_id.is_some(), egui::Button::new("Clone session"))
                        .on_hover_text("Start a new process from this tab's saved Profile")
                        .clicked()
                {
                    model.menu_command = Some(WorkbenchMenuCommand::CloneWorkspaceSession(tab.id));
                }
                for (label, axis) in [
                    ("Split right", SplitAxis::Horizontal),
                    ("Split down", SplitAxis::Vertical),
                ] {
                    if ui.button(label).clicked() {
                        model.menu_command = Some(WorkbenchMenuCommand::SplitWorkspace(axis));
                    }
                }
                if ui.button("New window").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::NewWorkspaceWindow);
                }
                for tab in &model.tabs {
                    let selected = model.selected_tab == Some(tab.id);
                    let label = if tab.session_id.is_none() {
                        format!("{} (not started)", tab.title)
                    } else {
                        tab.title.clone()
                    };
                    let response = ui.selectable_label(selected, label);
                    if response.clicked() {
                        model.selected_tab = Some(tab.id);
                    }
                    response.context_menu(|ui| {
                        if ui
                            .add_enabled(
                                tab.session_id.is_some(),
                                egui::Button::new("Clone session"),
                            )
                            .on_hover_text("Start a new process from this tab's saved Profile")
                            .clicked()
                        {
                            model.menu_command =
                                Some(WorkbenchMenuCommand::CloneWorkspaceSession(tab.id));
                            ui.close();
                        }
                        if ui.button("Move left").clicked() {
                            model.menu_command =
                                Some(WorkbenchMenuCommand::MoveWorkspaceTab(tab.id, -1));
                            ui.close();
                        }
                        if ui.button("Move right").clicked() {
                            model.menu_command =
                                Some(WorkbenchMenuCommand::MoveWorkspaceTab(tab.id, 1));
                            ui.close();
                        }
                        if let Some(document) = &model.workspace_document {
                            let current = model
                                .workspace_window_id
                                .or_else(|| document.windows.first().map(|w| w.id));
                            for (index, window) in document.windows.iter().enumerate() {
                                if Some(window.id) != current
                                    && model.available_windows.contains(&window.id)
                                    && ui.button(format!("Move to window {}", index + 1)).clicked()
                                {
                                    model.menu_command = Some(
                                        WorkbenchMenuCommand::MoveTabToWindow(tab.id, window.id),
                                    );
                                    ui.close();
                                }
                            }
                            let groups = current
                                .and_then(|id| document.windows.iter().find(|w| w.id == id))
                                .map(|w| {
                                    let mut groups = Vec::new();
                                    let mut pending = vec![w.root];
                                    while let Some(id) = pending.pop() {
                                        if let Some(pane) =
                                            document.panes.iter().find(|p| p.id == id)
                                        {
                                            match pane.content {
                                                WorkspacePaneContent::Tabs { group_id } => {
                                                    groups.push(group_id)
                                                }
                                                WorkspacePaneContent::Split {
                                                    first,
                                                    second,
                                                    ..
                                                } => {
                                                    pending.push(second);
                                                    pending.push(first);
                                                }
                                            }
                                        }
                                    }
                                    groups
                                })
                                .unwrap_or_default();
                            for (index, group) in document
                                .tab_groups
                                .iter()
                                .filter(|g| groups.contains(&g.id))
                                .enumerate()
                            {
                                if !group.tabs.contains(&tab.id)
                                    && ui.button(format!("Move to group {}", index + 1)).clicked()
                                {
                                    model.menu_command = Some(
                                        WorkbenchMenuCommand::MoveTabToGroup(tab.id, group.id),
                                    );
                                    ui.close();
                                }
                            }
                        }
                    });
                    if selected
                        && ui
                            .small_button("x")
                            .on_hover_text("Close this view")
                            .clicked()
                    {
                        model.menu_command = Some(WorkbenchMenuCommand::CloseWorkspaceTab(tab.id));
                    }
                }
            });
        });
        if let Some(tab) = model
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == model.selected_tab)
            && tab.session_id.is_none()
        {
            ui.horizontal(|ui| {
                ui.label("Restored layout. The previous process has not been restarted.");
                if ui.button("Open this tab").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::OpenWorkspaceTab(tab.id));
                }
            });
        }
        if !model.workspace_status.is_empty() {
            ui.label(&model.workspace_status);
        }
    });

    model.pane_views.clear();
    let document = model.workspace_document.clone();
    let terminal_rect = egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
        .show(ui, |ui| {
            let rect = ui.max_rect();
            if let Some(document) = &document
                && let Some(window) = model
                    .workspace_window_id
                    .and_then(|id| document.windows.iter().find(|w| w.id == id))
                    .or_else(|| document.windows.first())
            {
                draw_workspace_pane(ui, document, window.root, rect, model);
                model
                    .pane_views
                    .iter()
                    .find(|p| p.tab_id == model.selected_tab)
                    .map_or(rect, |p| p.rect)
            } else {
                rect
            }
        })
        .inner;

    if model.about_open {
        let modal = egui::Modal::new(egui::Id::new("cshell-about-dialog")).show(ui.ctx(), |ui| {
            ui.heading("关于 CShell");
            ui.label("SSH 终端与会话管理");
            ui.label(format!("版本 {}", env!("CARGO_PKG_VERSION")));
            ui.add_space(8.0);
            if ui.button("关闭").clicked() {
                ui.close();
            }
        });
        if modal.should_close() {
            model.about_open = false;
        }
    }

    terminal_rect
}

fn draw_workspace_pane(
    ui: &mut egui::Ui,
    document: &WorkspaceDocument,
    id: PaneId,
    rect: egui::Rect,
    model: &mut WorkbenchViewModel,
) {
    let Some(pane) = document.panes.iter().find(|p| p.id == id) else {
        return;
    };
    match pane.content {
        WorkspacePaneContent::Tabs { group_id } => {
            let Some(group) = document.tab_groups.iter().find(|g| g.id == group_id) else {
                return;
            };
            let header = egui::Rect::from_min_max(
                rect.min,
                egui::pos2(rect.max.x, (rect.min.y + 30.0).min(rect.max.y)),
            );
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(group_id)
                    .max_rect(header)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            egui::ScrollArea::horizontal()
                .id_salt(group_id)
                .show(&mut child, |ui| {
                    ui.horizontal(|ui| {
                        for tab in &group.tabs {
                            let title = document
                                .bindings
                                .iter()
                                .find(|b| b.tab_id == *tab)
                                .map_or("Tab", |b| b.title.as_str());
                            if ui
                                .selectable_label(group.active_tab == Some(*tab), title)
                                .clicked()
                            {
                                model.selected_tab = Some(*tab);
                            }
                        }
                    });
                });
            let body = egui::Rect::from_min_max(egui::pos2(rect.min.x, header.max.y), rect.max);
            let focused = group
                .tabs
                .iter()
                .any(|tab| Some(*tab) == model.selected_tab);
            ui.painter().rect_stroke(
                body,
                0.0,
                egui::Stroke::new(
                    if focused { 2.0 } else { 1.0 },
                    if focused {
                        egui::Color32::LIGHT_BLUE
                    } else {
                        egui::Color32::DARK_GRAY
                    },
                ),
                egui::StrokeKind::Inside,
            );
            model.pane_views.push(WorkspacePaneView {
                group_id,
                tab_id: group.active_tab,
                rect: body.shrink2(egui::vec2(
                    2.0_f32.min(body.width() / 2.0),
                    2.0_f32.min(body.height() / 2.0),
                )),
            });
        }
        WorkspacePaneContent::Split {
            axis,
            ratio_permille,
            first,
            second,
        } => {
            let horizontal = axis == SplitAxis::Horizontal;
            let size = if horizontal {
                rect.width()
            } else {
                rect.height()
            };
            let origin = if horizontal { rect.min.x } else { rect.min.y };
            let gap = 6.0_f32.min(size.max(0.0));
            let split =
                origin + gap / 2.0 + (size - gap).max(0.0) * f32::from(ratio_permille) / 1000.0;
            let mut left = rect;
            let mut right = rect;
            let mut divider = rect;
            if horizontal {
                left.max.x = split - gap / 2.0;
                right.min.x = split + gap / 2.0;
                divider.min.x = split - gap / 2.0;
                divider.max.x = split + gap / 2.0;
            } else {
                left.max.y = split - gap / 2.0;
                right.min.y = split + gap / 2.0;
                divider.min.y = split - gap / 2.0;
                divider.max.y = split + gap / 2.0;
            }
            let response = ui.interact(
                divider,
                egui::Id::new(("workspace-divider", id)),
                egui::Sense::drag(),
            );
            response.clone().on_hover_cursor(if horizontal {
                egui::CursorIcon::ResizeHorizontal
            } else {
                egui::CursorIcon::ResizeVertical
            });
            if response.dragged()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let position = if horizontal { pointer.x } else { pointer.y };
                let ratio = (((position - origin - gap / 2.0) / (size - gap).max(1.0)) * 1000.0)
                    .clamp(100.0, 900.0) as u16;
                model.menu_command = Some(WorkbenchMenuCommand::ResizeSplit(id, ratio));
            }
            draw_workspace_pane(ui, document, first, left, model);
            draw_workspace_pane(ui, document, second, right, model);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkbenchMenuCommand, WorkbenchViewModel, draw_workbench};

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

    fn button_center(update: &egui::accesskit::TreeUpdate, label: &str) -> egui::Pos2 {
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
        let context = egui::Context::default();
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
        let split = button_center(&update, "Split down");
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
        let context = egui::Context::default();
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
        let create = button_center(&update, "New window");
        click(&context, &mut model, create);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::NewWorkspaceWindow)
        );
    }

    #[test]
    fn tab_context_menu_moves_to_a_live_other_window() {
        let context = egui::Context::default();
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
            .find(|(_, n)| n.label() == Some("Left pane (not started)"))
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
        let move_button = button_center(&menu, "Move to window 2");
        click(&context, &mut model, move_button);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::MoveTabToWindow(tab, target))
        );
    }

    #[test]
    fn clone_button_requires_running_tab_and_emits_explicit_command() {
        let context = egui::Context::default();
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
        assert!(
            update
                .nodes
                .iter()
                .any(|(_, node)| { node.label() == Some("Clone session") && node.is_disabled() })
        );
        model.tabs[0].session_id = Some(cshell_domain::SessionId::new());
        let update = frame(&context, &mut model, vec![]);
        let clone = button_center(&update, "Clone session");
        click(&context, &mut model, clone);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::CloneWorkspaceSession(tab))
        );
    }

    #[test]
    fn tiny_split_layout_keeps_pane_rectangles_nonnegative() {
        let context = egui::Context::default();
        let doc = split_document();
        let mut model = WorkbenchViewModel::default();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            super::draw_workspace_pane(
                ui,
                &doc,
                doc.windows[0].root,
                egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(3.0, 4.0)),
                &mut model,
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
        let context = egui::Context::default();
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
        let open = button_center(&update, "Open this tab");
        click(&context, &mut model, open);
        assert_eq!(
            model.menu_command.take(),
            Some(WorkbenchMenuCommand::OpenWorkspaceTab(tab))
        );
        let update = frame(&context, &mut model, vec![]);
        let close = button_center(&update, "x");
        click(&context, &mut model, close);
        assert_eq!(
            model.menu_command.take(),
            Some(WorkbenchMenuCommand::CloseWorkspaceTab(tab))
        );
        let update = frame(&context, &mut model, vec![]);
        let new = button_center(&update, "+ New tab");
        click(&context, &mut model, new);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::NewWorkspaceTab)
        );
    }

    #[test]
    fn about_dialog_appears_in_accessibility_tree() {
        let context = egui::Context::default();
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
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel::default();
        let first = frame(&context, &mut model, Vec::new());
        let menu_position = button_center(&first, "菜单");
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
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel::default();
        let first = frame(&context, &mut model, Vec::new());
        let position = button_center(&first, "SSH 连接");
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::SshConnections)
        );
    }

    #[test]
    fn saved_ssh_connections_can_be_opened_from_the_workbench() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let id = cshell_domain::ProfileId::new();
        let mut model = WorkbenchViewModel {
            saved_ssh_profiles: vec![super::SavedSshProfileViewModel {
                id,
                name: "Production".into(),
                target: "alice@example.test:22".into(),
                favorite: false,
            }],
            ..WorkbenchViewModel::default()
        };
        let first = frame(&context, &mut model, Vec::new());
        let position = button_center(&first, "连接");
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::OpenSavedProfile(id))
        );

        model.menu_command = None;
        let update = frame(&context, &mut model, Vec::new());
        let position = button_center(&update, "编辑");
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::EditSavedProfile(id))
        );

        model.menu_command = None;
        let update = frame(&context, &mut model, Vec::new());
        let position = button_center(&update, "＋ 新建 SSH");
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
        let context = egui::Context::default();
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
        let position = button_center(&update, "Terminate");
        click(&context, &mut model, position);
        assert_eq!(
            model.menu_command,
            Some(WorkbenchMenuCommand::TerminateSession(original))
        );
        assert!(!model.confirm_terminate);
    }
}
