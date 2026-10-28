//! Process-wide preferences shared by every native terminal window.
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub copy_on_select: bool,
    pub right_click_paste: bool,
    pub middle_click_paste: bool,
    pub word_delimiters: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            copy_on_select: true,
            right_click_paste: false,
            middle_click_paste: true,
            word_delimiters: "\"'`[]{}()<>|;,=".into(),
        }
    }
}
static SETTINGS: OnceLock<Mutex<Settings>> = OnceLock::new();
fn store() -> &'static Mutex<Settings> {
    SETTINGS.get_or_init(|| Mutex::new(Settings::default()))
}
pub(crate) fn get() -> Settings {
    store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
fn path() -> Option<std::path::PathBuf> {
    super::display_settings::settings_path()?
        .parent()
        .map(|p| p.join("terminal-clipboard.json"))
}
pub(crate) fn initialize() {
    let settings = path()
        .and_then(|p| {
            if std::fs::metadata(&p).ok()?.len() > 4096 {
                return None;
            }
            serde_json::from_slice(&std::fs::read(p).ok()?).ok()
        })
        .unwrap_or_default();
    *store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
}
pub(crate) fn draw(ui: &mut egui::Ui) {
    let mut settings = get();
    let before = settings.clone();
    ui.strong("终端 · 鼠标与剪贴板");
    ui.checkbox(&mut settings.copy_on_select, "选中后自动复制（松开左键时）");
    ui.checkbox(&mut settings.middle_click_paste, "中键粘贴");
    ui.checkbox(&mut settings.right_click_paste, "右键直接粘贴");
    ui.weak("右键默认打开菜单；启用直接粘贴后，Shift＋右键仍可打开菜单。");
    ui.collapsing("高级选择设置", |ui| {
        ui.label("双击单词时的分隔字符（空白始终分隔）");
        ui.add(
            egui::TextEdit::singleline(&mut settings.word_delimiters)
                .char_limit(128)
                .desired_width(ui.available_width()),
        );
        if ui.small_button("恢复默认分隔符").clicked() {
            settings.word_delimiters = Settings::default().word_delimiters;
        }
        ui.weak("默认保留路径中的 /、地址中的 : 和 .，三击选择完整逻辑行。");
    });
    ui.separator();
    for text in [
        "选择：左键拖动 · 双击单词 · 三击整行",
        "扩展选择：Shift＋左键 · 矩形选择：Alt＋拖动",
        "复制：Ctrl＋Shift＋C / Ctrl＋Insert",
        "粘贴：Ctrl＋Shift＋V / Shift＋Insert",
    ] {
        ui.label(text);
    }
    ui.weak("终端保留 Ctrl＋C/V 的控制键含义；编辑框使用普通 Ctrl＋C/V。多行及控制字符粘贴先预览，不额外追加回车。");
    ui.weak("Vim / tmux 接管鼠标时，Shift＋拖动选择、Shift＋右键菜单。Ctrl＋滚轮临时缩放当前窗口，Ctrl＋0 恢复默认。");
    ui.weak("即时应用到全部窗口并自动保存；所有会话使用这些全局偏好。");
    if settings != before {
        *store()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings.clone();
        let result = path()
            .ok_or_else(|| std::io::Error::other("无法确定设置目录"))
            .and_then(|p| {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(p, serde_json::to_vec_pretty(&settings)?)
            });
        if let Err(error) = result {
            ui.colored_label(
                cshell_ui::theme::WARNING,
                format!("已生效，但未保存：{error}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_preferences_keep_linux_defaults_and_roundtrip() {
        let defaults: Settings = serde_json::from_str("{}").unwrap();
        assert!(defaults.copy_on_select && defaults.middle_click_paste);
        assert!(!defaults.right_click_paste);
        let custom = Settings {
            right_click_paste: true,
            copy_on_select: false,
            middle_click_paste: false,
            ..Default::default()
        };
        assert_eq!(
            custom,
            serde_json::from_str(&serde_json::to_string(&custom).unwrap()).unwrap()
        );
    }
}
