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
    pub terminal_generation: Option<u64>,
}

/// Draws the native workbench chrome and returns the logical-point rectangle
/// reserved for the GPU terminal surface.
#[must_use]
pub fn draw_workbench(ui: &mut egui::Ui, model: &mut WorkbenchViewModel) -> egui::Rect {
    egui::Panel::top("top_bar").exact_size(36.0).show(ui, |ui| {
        ui.horizontal(|ui| {
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

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
        .show(ui, |ui| ui.max_rect())
        .inner
}
