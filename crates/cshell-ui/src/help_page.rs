//! Reading-sized styles are scoped to help, leaving the compact workbench alone.
use crate::theme;
use egui::{Context, FontId, RichText, TextStyle, Ui};

pub(crate) fn show(
    ctx: &Context,
    id: &'static str,
    title: &str,
    subtitle: &str,
    width: f32,
    content: impl FnOnce(&mut Ui),
) -> bool {
    let frame = egui::Frame::popup(&ctx.style_of(egui::Theme::Dark))
        .fill(theme::PANEL)
        .stroke(egui::Stroke::new(1.0, theme::LINE))
        .inner_margin(20.0)
        .corner_radius(8.0);
    egui::Modal::new(egui::Id::new(id))
        .frame(frame)
        .show(ctx, |ui| {
            let style = ui.style_mut();
            style
                .text_styles
                .insert(TextStyle::Body, FontId::proportional(15.0));
            style
                .text_styles
                .insert(TextStyle::Button, FontId::proportional(14.0));
            style
                .text_styles
                .insert(TextStyle::Small, FontId::proportional(14.0));
            style
                .text_styles
                .insert(TextStyle::Monospace, FontId::monospace(14.0));
            style.visuals.override_text_color = Some(theme::TEXT);
            style.visuals.weak_text_color = Some(theme::MUTED);
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.button_padding = egui::vec2(16.0, 7.0);
            style.wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.set_width((ctx.content_rect().width() - 88.0).clamp(240.0, width));
            ui.label(RichText::new(title).size(22.0).strong().color(theme::TEXT));
            ui.label(RichText::new(subtitle).size(14.0).color(theme::MUTED));
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt(id)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .auto_shrink([false, true])
                .max_height((ctx.content_rect().height() - 220.0).max(96.0))
                .show(ui, content);
            ui.add_space(4.0);
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_sized([88.0, 34.0], egui::Button::new("关闭"))
                    .clicked()
                {
                    ui.close();
                }
            });
        })
        .should_close()
}

pub(crate) fn section(ui: &mut Ui, title: &str, content: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(theme::BACKGROUND)
        .stroke(egui::Stroke::new(1.0, theme::LINE))
        .corner_radius(5.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(
                RichText::new(title)
                    .size(16.0)
                    .strong()
                    .color(theme::ACCENT),
            );
            ui.add_space(2.0);
            content(ui);
        });
}

pub(crate) fn about(ctx: &Context, open: &mut bool) {
    if !*open {
        return;
    }
    if show(
        ctx,
        "cshell-about-dialog",
        "关于 CShell",
        "SSH 终端与会话管理",
        540.0,
        |ui| {
            section(ui, "CShell", |ui| {
                ui.label(
                    RichText::new(format!("版本 {}", env!("CARGO_PKG_VERSION")))
                        .size(19.0)
                        .strong(),
                );
                ui.label("开发试用版");
                ui.label("管理 SSH 与本地终端，在多个会话间切换，并通过 SFTP 传输文件。");
            });
            section(ui, "使用与反馈", |ui| {
                ui.label("鼠标选择、复制粘贴、终端控制与历史回看的说明，位于“帮助 → 快捷键说明”。");
                ui.label("反馈问题时，请附上版本、操作系统和复现步骤；请勿提供密码或私钥。");
                if ui.button("复制版本信息").clicked() {
                    ui.ctx().copy_text(format!(
                        "CShell {}\n{} / {}",
                        env!("CARGO_PKG_VERSION"),
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ));
                }
            });
        },
    ) {
        *open = false;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    fn find_body(shape: &egui::Shape) -> Option<&egui::epaint::TextShape> {
        match shape {
            egui::Shape::Text(text) if text.galley.job.text == "Readable dialog body" => Some(text),
            egui::Shape::Vec(shapes) => shapes.iter().find_map(find_body),
            _ => None,
        }
    }
    #[test]
    fn reading_font_is_rendered_in_help_without_changing_the_workbench_style() {
        let ctx = Context::default();
        crate::install_workbench_style(&ctx);
        let original = ctx.style_of(egui::Theme::Dark).text_styles.clone();
        let mut found = false;
        for _ in 0..3 {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(640.0, 480.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    show(
                        ui.ctx(),
                        "readable-help-test",
                        "Help",
                        "Default shortcuts",
                        540.0,
                        |ui| {
                            ui.label("Readable dialog body");
                        },
                    );
                },
            );
            for shape in &out.shapes {
                if let Some(body) = find_body(&shape.shape) {
                    assert!(
                        body.galley
                            .job
                            .sections
                            .iter()
                            .all(|section| section.format.font_id.size >= 15.0)
                    );
                    found = true;
                }
            }
            out.textures_delta.clear();
        }
        assert!(found, "verify actual laid out help text");
        assert_eq!(ctx.style_of(egui::Theme::Dark).text_styles, original);
    }
}
