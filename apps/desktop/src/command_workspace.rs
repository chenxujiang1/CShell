//! Layout-only preferences. Command text and temporary bookmarks stay in memory.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum Position {
    Bottom,
    Right,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    position: Position,
    height: f32,
    width: f32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            position: Position::Bottom,
            height: 240.0,
            width: 380.0,
        }
    }
}
#[derive(Default)]
pub(super) struct Dock {
    pub visible: bool,
    loaded: bool,
    settings: Settings,
    last: Option<Settings>,
}
impl Dock {
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }
    pub fn draw(&mut self, ui: &mut egui::Ui, mut content: impl FnMut(&mut egui::Ui)) {
        if !self.loaded {
            self.loaded = true;
            #[cfg(not(test))]
            if let Some(path) = path()
                && std::fs::metadata(&path).is_ok_and(|m| m.len() < 4096)
                && let Ok(bytes) = std::fs::read(path)
                && let Ok(value) = serde_json::from_slice::<Settings>(&bytes)
            {
                self.settings = value;
            }
            if !self.settings.height.is_finite() {
                self.settings.height = 240.0;
            }
            if !self.settings.width.is_finite() {
                self.settings.width = 380.0;
            }
            self.last = Some(self.settings.clone());
        }
        if !self.visible {
            return;
        }
        let right = self.settings.position == Position::Right;
        let size = if right {
            self.settings.width
        } else {
            self.settings.height
        };
        let available = if right {
            ui.available_width()
        } else {
            ui.available_height()
        };
        let min = if right { 260.0 } else { 150.0 };
        let max = (available * 0.65).max(min).min(available);
        let panel = if right {
            egui::Panel::right("command-workspace-right")
        } else {
            egui::Panel::bottom("command-workspace-bottom")
        };
        let result = panel
            .default_size(size.clamp(min.min(max), max))
            .min_size(min.min(max))
            .max_size(max)
            .resizable(true)
            .frame(cshell_ui::form::utility_bar())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("命令工作区");
                    if ui.selectable_label(!right, "底部").clicked() {
                        self.settings.position = Position::Bottom;
                    }
                    if ui.selectable_label(right, "右侧").clicked() {
                        self.settings.position = Position::Right;
                    }
                    if ui.button("收起").clicked() {
                        self.visible = false;
                        ui.ctx().memory_mut(|m| {
                            if let Some(id) = m.focused() {
                                m.surrender_focus(id);
                            }
                        });
                    }
                });
                ui.push_id("docked-command-history", |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("command-workspace-scroll")
                        .max_height(ui.available_height())
                        .show(ui, |ui| content(ui));
                });
            });
        if right {
            self.settings.width = result.response.rect.width();
        } else {
            self.settings.height = result.response.rect.height();
        }
        if !ui.ctx().input(|i| i.pointer.any_down()) && self.last.as_ref() != Some(&self.settings) {
            #[cfg(not(test))]
            if let Some(path) = path() {
                if let Err(error) = save(&path, &self.settings) {
                    ui.ctx().debug_painter().text(
                        egui::pos2(12.0, 12.0),
                        egui::Align2::LEFT_TOP,
                        format!("命令布局未保存：{error}"),
                        egui::FontId::proportional(12.0),
                        cshell_ui::theme::WARNING,
                    );
                }
            }
            self.last = Some(self.settings.clone());
        }
    }
}
fn path() -> Option<std::path::PathBuf> {
    crate::display_settings::settings_path().map(|p| p.with_file_name("command-workspace.json"))
}
fn save(path: &std::path::Path, value: &Settings) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_roundtrip_stores_only_geometry_and_dock_keeps_terminal_space() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("layout.json");
        let settings = Settings {
            position: Position::Right,
            width: 420.0,
            height: 210.0,
        };
        save(&path, &settings).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            serde_json::from_slice::<Settings>(&bytes).unwrap(),
            settings
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            3
        );
        for position in [Position::Bottom, Position::Right] {
            let ctx = crate::ui_layout_gate::context();
            let mut dock = Dock {
                visible: true,
                settings: Settings {
                    position,
                    ..Default::default()
                },
                ..Default::default()
            };
            for index in 0..3 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(720.0, 480.0),
                        )),
                        time: Some(index as f64),
                        ..Default::default()
                    },
                    |ui| {
                        dock.draw(ui, |ui| {
                            ui.label("history");
                        });
                        assert!(ui.available_width() >= 240.0 && ui.available_height() >= 140.0);
                    },
                );
                output.textures_delta.clear();
            }
            dock.toggle();
            assert!(!dock.visible);
        }
    }
}
