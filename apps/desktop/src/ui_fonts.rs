use egui::{Context, FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

const CJK_FONT_NAME: &str = "Noto Sans SC";
// Google Fonts Noto Sans SC (OFL 1.1); see ../assets/fonts/OFL.txt.
// egui uses the variable font's default axis (100); pin 400 for readable UI text.
const CJK_FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/NotoSansSC-Regular.ttf");

pub fn install(context: &Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        CJK_FONT_NAME.to_owned(),
        Arc::new(FontData::from_static(CJK_FONT_BYTES)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        // Keep the primary Latin font, but prefer text symbols over emoji faces.
        let chain=fonts.families.entry(family).or_default();
        chain.insert(1.min(chain.len()),CJK_FONT_NAME.to_owned());
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
            for ch in "生产预发布测试开发…←→↑↓↔".chars(){assert!(fonts.has_glyphs(&FontId::proportional(13.0),&ch.to_string()),"missing glyph {ch} U+{:04X}",ch as u32);}
        });
    }
}
