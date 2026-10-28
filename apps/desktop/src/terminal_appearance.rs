//! Presentation-only preview; saved connection settings remain authoritative.
use cshell_domain::ThemeRecord;

pub(super) fn set_font(context: &egui::Context, pixels: u16) {
    context.data_mut(|d| d.insert_temp(egui::Id::new("terminal-preview-font"), pixels));
}
pub(super) fn font(context: &egui::Context) -> u16 {
    context
        .data(|d| d.get_temp(egui::Id::new("terminal-preview-font")))
        .unwrap_or(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS)
}
pub(super) fn preview(ui: &mut egui::Ui, name: &str, themes: &[ThemeRecord], pixels: u16) {
    let (fg, bg) = match cshell_render::TerminalTheme::from_catalog_name(name, themes) {
        cshell_render::TerminalTheme::Custom(p) => (p.foreground, p.background),
        cshell_render::TerminalTheme::Light => ([28, 34, 44], [240, 242, 247]),
        cshell_render::TerminalTheme::Dark => ([218, 225, 235], [15, 20, 29]),
    };
    egui::Frame::NONE
        .fill(egui::Color32::from_rgb(bg[0], bg[1], bg[2]))
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for line in ["user@linux:~$ ls", "bin  logs  src"] {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(line)
                            .monospace()
                            .size(f32::from(pixels))
                            .color(egui::Color32::from_rgb(fg[0], fg[1], fg[2])),
                    )
                    .truncate(),
                );
            }
        });
    ui.weak("终端显示示例");
}
