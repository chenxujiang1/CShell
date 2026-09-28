use egui::{Context, FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

const CJK_FONT_NAME: &str = "Noto Sans SC";
// Google Fonts Noto Sans SC (OFL 1.1); see ../assets/fonts/OFL.txt.
const CJK_FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/NotoSansSC-VF.ttf");

pub fn install(context: &Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        CJK_FONT_NAME.to_owned(),
        Arc::new(FontData::from_static(CJK_FONT_BYTES)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(CJK_FONT_NAME.to_owned());
    }
    context.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::install;
    use egui::{Context, FontId};

    #[test]
    fn desktop_ui_fonts_cover_chinese_controls_and_input() {
        let context = Context::default();
        install(&context);
        let mut output = context.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        context.fonts_mut(|fonts| {
            assert!(fonts.has_glyphs(
                &FontId::proportional(14.0),
                "菜单 SSH连接 新建配置 主机密钥 中文用户名"
            ));
            assert!(fonts.has_glyphs(&FontId::monospace(14.0), "中文路径"));
        });
    }
}
