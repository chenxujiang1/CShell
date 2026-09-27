//! Egui workbench controls. Terminal cells are never represented as egui widgets.

use cshell_domain::{
    PaneId, SessionId, SplitAxis, TabGroupId, TabId, WorkspaceDocument, WorkspacePaneContent,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTabViewModel {
    pub id: SessionId,
    pub title: String,
    pub connected: bool,
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
    pub tabs: Vec<WorkspaceTabViewModel>,
    pub selected_tab: Option<TabId>,
    pub workspace_document: Option<WorkspaceDocument>,
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
    ReconnectNewShell,
    CloseView,
    TerminateSession(SessionId),
    NewWorkspaceTab,
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
                if ui.button("管理 Profiles").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::Profiles);
                    ui.close();
                }
                if ui.button("关于 CShell").clicked() {
                    model.about_open = true;
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
            if ui
                .add_enabled(
                    model.can_reconnect_ssh,
                    egui::Button::new("Reconnect · New Shell"),
                )
                .clicked()
            {
                model.menu_command = Some(WorkbenchMenuCommand::ReconnectNewShell);
            }
            let status = if model.daemon_connected {
                "terminal connected"
            } else {
                "terminal disconnected"
            };
            ui.label(status);
            if ui
                .add_enabled(model.daemon_connected, egui::Button::new("Close view"))
                .clicked()
            {
                model.menu_command = Some(WorkbenchMenuCommand::CloseView);
            }
            if ui
                .add_enabled(
                    model.daemon_connected,
                    egui::Button::new("Terminate process"),
                )
                .clicked()
            {
                model.terminate_target = model.selected;
                model.confirm_terminate = model.terminate_target.is_some();
            }
            if !model.daemon_status_detail.is_empty() {
                ui.label(&model.daemon_status_detail);
            }
            if let Some(warning) = &model.input_warning {
                ui.colored_label(egui::Color32::LIGHT_RED, warning);
            }
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
        .default_size(220.0)
        .show(ui, |ui| {
            ui.heading("Sessions");
            for session in &model.sessions {
                let selected = model.selected == Some(session.id);
                let state = if session.connected { "[+]" } else { "[-]" };
                if ui
                    .selectable_label(selected, format!("{state} {}", session.title))
                    .clicked()
                {
                    model.selected = Some(session.id);
                }
            }
        });

    egui::Panel::top("workspace_tabs").show(ui, |ui| {
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("+ New tab").clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::NewWorkspaceTab);
                }
                for (label, axis) in [
                    ("Split right", SplitAxis::Horizontal),
                    ("Split down", SplitAxis::Vertical),
                ] {
                    if ui.button(label).clicked() {
                        model.menu_command = Some(WorkbenchMenuCommand::SplitWorkspace(axis));
                    }
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
                            for (index, group) in document.tab_groups.iter().enumerate() {
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
            if let Some(document) = &document {
                draw_workspace_pane(ui, document, document.windows[0].root, rect, model);
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
            ui.label("跨平台 SSH/SFTP 终端 · Phase 0 工程原型");
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
