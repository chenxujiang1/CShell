//! Egui workbench controls. Terminal cells are never represented as egui widgets.

use cshell_domain::SessionId;

#[derive(Clone, Debug)]
pub struct SessionTabViewModel {
    pub id: SessionId,
    pub title: String,
    pub connected: bool,
}

#[derive(Debug, Default)]
pub struct WorkbenchViewModel {
    pub sessions: Vec<SessionTabViewModel>,
    pub selected: Option<SessionId>,
    pub daemon_connected: bool,
    pub daemon_status_detail: String,
    pub input_warning: Option<String>,
    pub terminal_generation: Option<u64>,
    pub about_open: bool,
    pub menu_command: Option<WorkbenchMenuCommand>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkbenchMenuCommand {
    SearchTerminal,
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
            let status = if model.daemon_connected {
                "daemon connected"
            } else {
                "daemon offline"
            };
            ui.label(status);
            if !model.daemon_status_detail.is_empty() {
                ui.label(&model.daemon_status_detail);
            }
            if let Some(warning) = &model.input_warning {
                ui.colored_label(egui::Color32::LIGHT_RED, warning);
            }
        });
    });

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

    let terminal_rect = egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
        .show(ui, |ui| ui.max_rect())
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
}
