use crate::{help_page, theme};
use egui::{Context, RichText, Ui};

fn key_badge(ui: &mut Ui, keys: &str) {
    egui::Frame::new()
        .fill(theme::RAISED)
        .stroke(egui::Stroke::new(1.0, theme::LINE))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(keys).size(14.0).strong().color(theme::TEXT));
        });
}

fn rows(ui: &mut Ui, entries: &[(&str, &str)]) {
    for (index, (keys, meaning)) in entries.iter().enumerate() {
        if index > 0 {
            ui.add_space(4.0);
        }
        if ui.available_width() < 540.0 {
            key_badge(ui, keys);
            ui.label(*meaning);
        } else {
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(216.0, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_width(216.0);
                        key_badge(ui, keys);
                    },
                );
                ui.add(egui::Label::new(*meaning).wrap());
            });
        }
    }
}

pub(crate) fn draw(ctx: &Context, open: &mut bool) {
    if !*open {
        return;
    }
    let mac = cfg!(target_os = "macos");
    let subtitle = if mac {
        "macOS 默认操作 · 编辑框使用各自的快捷键"
    } else {
        "Windows / Linux 默认操作 · 编辑框使用各自的快捷键"
    };
    if help_page::show(
        ctx,
        "cshell-shortcut-help",
        "快捷键说明",
        subtitle,
        720.0,
        |ui| {
            help_page::section(ui, "复制、粘贴与全选", |ui| {
                rows(
                    ui,
                    &[
                        (
                            if mac { "⌘C" } else { "Ctrl + Shift + C" },
                            "复制选中的终端文字。也可在设置中启用鼠标松开时自动复制。",
                        ),
                        (
                            if mac { "⌘V" } else { "Ctrl + Shift + V" },
                            "粘贴剪贴板内容；多行或控制字符先预览，不额外追加回车。",
                        ),
                        (
                            if mac { "⌘A" } else { "Ctrl + Shift + A" },
                            "全选当前终端仍保留的历史与屏幕。全选本身不自动复制。",
                        ),
                        ("右键 → 选择当前屏幕", "只选择眼前的屏幕内容。"),
                        (
                            "右键 → 导出当前终端…",
                            "将保留内容保存为文本。复制超过 16 MiB 时会提示导出，不会静默截断。",
                        ),
                    ],
                );
                ui.add_space(6.0);
                ui.label(RichText::new("也可使用 Ctrl + Insert 复制、Shift + Insert 粘贴；自定义绑定以键盘设置为准。").size(14.0).color(theme::MUTED));
            });
            help_page::section(ui, "鼠标选择", |ui| {
                rows(
                    ui,
                    &[
                        ("单击 / 拖动", "聚焦并清除旧选区 / 按字符选择。"),
                        (
                            "双击 / 双击后拖动",
                            "选一个词 / 按词扩展。路径中的 / 和反斜杠是默认分隔符。",
                        ),
                        (
                            "三击 / 三击后拖动",
                            "选择整条逻辑行 / 按行扩展；自动折行仍算同一行。",
                        ),
                        ("Shift + 单击", "扩展已有选区；普通单击重新开始选择。"),
                        ("Alt + 拖动", "矩形选择。"),
                        (
                            "Shift + 拖动",
                            "Vim、nano、tmux 接管鼠标时，强制在本地选择文字。",
                        ),
                        (
                            "右键",
                            "打开复制、粘贴和全选菜单，也可复制完整路径、URL 或地址。",
                        ),
                    ],
                );
            });
            help_page::section(ui, "终端控制与历史回看", |ui| {
                rows(
                    ui,
                    &[
                        (
                            "Ctrl + C",
                            "发送终端控制键，例如停止 ping 或 docker logs -f。复制请使用上方复制快捷键。",
                        ),
                        ("Ctrl + Break", "显式中断当前终端；也可右键选择“发送中断”。"),
                        ("Ctrl + A", "保留 Shell 的行首等原有含义。"),
                        (
                            "输入文字 / Enter",
                            "SSH 已断开时重新连接当前标签；本次输入不发送，复制和翻页不触发。",
                        ),
                        ("Shift + PageUp / PageDown", "向上 / 向下回看终端历史。"),
                        ("End / 返回最新", "回看时回到实时画面，不向服务器发送回车。"),
                        (
                            "Ctrl + 滚轮 / Ctrl + 0",
                            "临时缩放当前窗口 / 恢复默认字号。",
                        ),
                        (
                            if mac { "⌘⇧F" } else { "Ctrl + Shift + F" },
                            "查找终端文本。",
                        ),
                        (
                            if mac { "⌘⇧K" } else { "Ctrl + Shift + K" },
                            "打开快捷操作；绑定可在键盘设置中修改。",
                        ),
                    ],
                );
            });
            help_page::section(ui, "阅读持续日志", |ui| {
                ui.label("选择或回看只暂停本地阅读，服务器上的 tail -f 继续运行。后续输出不会混入已固定的全选内容。");
                ui.label("历史只包含仍保留的内容；重要日志请及时导出保存。");
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
    #[test]
    fn help_keeps_close_button_visible_in_small_and_scaled_windows() {
        for (size, scale) in [
            (egui::vec2(640.0, 480.0), 1.0),
            (egui::vec2(800.0, 600.0), 1.5),
            (egui::vec2(1200.0, 800.0), 2.0),
        ] {
            for about in [false, true] {
                let ctx = Context::default();
                ctx.enable_accesskit();
                ctx.set_pixels_per_point(scale);
                let mut open = true;
                let mut last = None;
                for _ in 0..3 {
                    let mut out = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ui| {
                            if about {
                                help_page::about(ui.ctx(), &mut open);
                            } else {
                                draw(ui.ctx(), &mut open);
                            }
                        },
                    );
                    last = out.platform_output.accesskit_update.take();
                    out.textures_delta.clear();
                }
                let tree = last.unwrap();
                let close = tree
                    .nodes
                    .iter()
                    .find(|(_, n)| n.label() == Some("关闭"))
                    .expect("close stays outside scrolling text")
                    .1
                    .bounds()
                    .unwrap();
                assert!(close.y0 >= 0.0 && close.y1 <= f64::from(size.y));
                assert!(close.x0 >= 0.0 && close.x1 <= f64::from(size.x));
                assert!(open);
            }
        }
    }
}
