//! A centered, bounded review of the immutable payload and recipient snapshot.
use super::*;
use cshell_ui::theme;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Decision {
    Stay,
    Edit,
    Send,
}

fn left_label(ui: &mut egui::Ui, width: f32, height: f32, text: egui::RichText) -> egui::Response {
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(width, height));
            ui.add(egui::Label::new(text).truncate())
        },
    )
    .inner
}

fn suspicious(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\r')
        || matches!(c,
        '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

fn extra_ack(text: &str, risk: SyncInputRisk) -> bool {
    risk.destructive_command
        || text.chars().any(suspicious)
        || text
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0] == b'\r' && pair[1] != b'\n')
        || text.ends_with('\r')
}

/// Natural LF/CRLF line breaks; non-layout controls remain visibly annotated.
fn readable_body(text: &str) -> Zeroizing<String> {
    use std::fmt::Write;
    let mut result = Zeroizing::new(String::new());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => result.push_str("[CR]"),
            '\t' => result.push_str("[TAB]"),
            c if suspicious(c) => {
                let _ = write!(result, "[U+{:04X}]", u32::from(c));
            }
            c => result.push(c),
        }
    }
    result
}

/// Bound font layout for a pathological single line; payload remains untouched.
fn bounded_line(line: &str) -> std::borrow::Cow<'_, str> {
    if let Some((boundary, _)) = line.char_indices().nth(4096) {
        std::borrow::Cow::Owned(format!("{} … [本行仅预览前 4096 字符，发送原文未截断]", &line[..boundary]))
    } else { std::borrow::Cow::Borrowed(line) }
}

pub(super) fn show(
    ctx: &egui::Context,
    window: WorkspaceWindowId,
    editor: &mut ComposePanel,
    action: &InputAction,
    targets: &[SyncInputTarget],
    risk: SyncInputRisk,
    bytes: usize,
) -> Decision {
    let (text, append_enter, paste) = match action {
        InputAction::Compose { text, append_enter } => (text, *append_enter, false),
        InputAction::Paste { text, .. } => (text, false, true),
        _ => return Decision::Edit,
    };
    let preview = readable_body(text);
    let lines: Vec<_> = preview.split('\n').collect();
    let preview_truncated = lines.iter().any(|line| line.chars().take(4097).count() > 4096);
    let strong_warning = extra_ack(text, risk) || preview_truncated;
    let remote = targets
        .iter()
        .all(|target| target.kind == SyncInputTargetKind::Ssh);
    let noun = if remote { "台主机" } else { "个会话" };
    let title = if paste { format!("向 {} {noun}粘贴文本", targets.len()) } else { format!(
        "向 {} {noun}发送{}",
        targets.len(),
        if risk.multiline {
            "多行命令"
        } else {
            "命令"
        }
    ) };
    let send_label = format!("{}到 {} {noun}", if paste { "粘贴" } else { "发送" }, targets.len());
    let viewport = ctx.content_rect();
    // Logical points follow native DPI. Preserve margins and bound both axes.
    let width = (viewport.width() * 0.78)
        .clamp(600.0, 920.0)
        .min((viewport.width() - 32.0).max(1.0));
    let ideal_height = 286.0
        + targets.len().min(3) as f32 * 24.0
        + lines.len().clamp(4, 10) as f32 * 21.0
        + if strong_warning { 42.0 } else { 0.0 };
    let height = ideal_height
        .max(400.0)
        .min((viewport.height() * 0.85).min(720.0))
        .min((viewport.height() - 32.0).max(1.0));
    let inner_size = egui::vec2((width - 32.0).max(1.0), (height - 32.0).max(1.0));
    let expired = editor
        .review_started
        .is_some_and(|started| started.elapsed() >= SYNC_INPUT_REVIEW_LIFETIME);
    ctx.request_repaint_after(std::time::Duration::from_secs(1));
    let modal_id = egui::Id::new(("compose-send-review", window));
    let mut decision = Decision::Stay;
    let response = egui::Modal::new(modal_id)
        .area(egui::Modal::default_area(modal_id).constrain_to(viewport.shrink(16.0)))
        .backdrop_color(egui::Color32::from_black_alpha(130))
        .frame(egui::Frame::popup(&ctx.style_of(egui::Theme::Dark)).fill(theme::PANEL)
            .inner_margin(16).corner_radius(8).stroke(egui::Stroke::new(1.0, theme::LINE)))
        .show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(theme::TEXT);
            let (rect, _) = ui.allocate_exact_size(inner_size, egui::Sense::hover());
            let header_height = if strong_warning { 80.0 } else { 64.0 };
            let footer_height = if strong_warning { 92.0 } else { 76.0 };
            let header_rect = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), header_height));
            let footer_rect = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - footer_height), rect.max);
            let body_rect = egui::Rect::from_min_max(egui::pos2(rect.min.x, header_rect.max.y + 6.0), egui::pos2(rect.max.x, footer_rect.min.y - 8.0));
            let mut header = ui.new_child(egui::UiBuilder::new().id_salt("review-heading").max_rect(header_rect));
            header.set_clip_rect(header_rect);
            header.spacing_mut().item_spacing.y = 3.0;
            header.horizontal(|ui| {
                left_label(ui, ui.available_width() - 36.0, 28.0, egui::RichText::new(&title).size(21.0).strong().color(theme::TEXT)).on_hover_text(&title);
                if ui.add_sized([28.0, 28.0], egui::Button::new("×").frame(false)).on_hover_text(if paste { "取消本次粘贴" } else { "关闭预览并返回编辑，保留命令与目标" }).clicked() { decision = Decision::Edit; }
            });
            header.colored_label(theme::ACCENT, if paste { "尚未粘贴 · 请核对目标和剪贴板内容" } else { "尚未发送 · 请核对接收主机和命令内容" });
            if strong_warning {
                header.add(egui::Label::new(egui::RichText::new(if risk.destructive_command { "检测到可能有破坏性的命令，请仔细核对。" } else if preview_truncated { "超长行仅预览前 4096 字符，请核对完整内容后确认。" } else { "包含控制字符或不可见字符，请核对方括号标记。" }).color(theme::WARNING)).truncate());
            }
            header.separator();

            let mut body = ui.new_child(egui::UiBuilder::new().id_salt("review-body").max_rect(body_rect));
            body.set_clip_rect(body_rect);
            body.spacing_mut().item_spacing.y = 3.0;
            body.spacing_mut().scroll.floating = false;
            body.label(egui::RichText::new(format!("接收{} · {}", if remote { "主机" } else { "会话" }, targets.len())).strong().color(theme::TEXT));
            let peer_height = (targets.len().min(3) as f32 * 24.0).min((body_rect.height() * 0.3).max(24.0));
            egui::ScrollArea::vertical().id_salt(("review-peers", editor.review_serial))
                .max_height(peer_height).auto_shrink([false, true]).show(&mut body, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for target in targets {
                        ui.horizontal(|ui| {
                            left_label(ui, (ui.available_width() * 0.32).clamp(100.0, 240.0), 23.0, egui::RichText::new(&target.title).strong().color(theme::TEXT)).on_hover_text(&target.title);
                            let endpoint = editor.review_endpoints.get(&target.tab_id).map(|s| format!("配置：{s}"))
                                .unwrap_or_else(|| if remote { "SSH 会话".into() } else { "本地终端".into() });
                            ui.add(egui::Label::new(egui::RichText::new(&endpoint).monospace().color(theme::MUTED)).truncate()).on_hover_text(endpoint);
                        });
                    }
                });
            body.add_space(6.0);
            body.horizontal(|ui| { ui.label(egui::RichText::new(if paste { "剪贴板内容" } else { "命令内容" }).strong().color(theme::TEXT)); ui.weak(format!("{} 行", lines.len())); });
            let code_height = body.available_height().max(24.0);
            egui::Frame::NONE.fill(theme::INPUT).stroke(egui::Stroke::new(1.0, theme::LINE)).inner_margin(6)
                .show(&mut body, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.spacing_mut().interact_size.y = 21.0;
                    let font = egui::FontId::monospace(15.0);
                    let longest = lines.iter().max_by_key(|line| line.chars().count()).copied().unwrap_or("");
                    let content_width = ui.fonts_mut(|fonts| fonts.layout_no_wrap(bounded_line(longest).into_owned(), font.clone(), theme::TEXT).size().x) + 62.0;
                    egui::ScrollArea::both().id_salt(("review-command", editor.review_serial))
                        .max_height((code_height - 14.0).max(20.0)).auto_shrink([false, false])
                        .show_rows(ui, 21.0, lines.len(), |ui, range| {
                            ui.set_min_width(content_width);
                            for index in range {
                                ui.horizontal(|ui| {
                                    ui.add_sized([38.0, 21.0], egui::Label::new(egui::RichText::new((index + 1).to_string()).font(font.clone()).color(theme::MUTED)).selectable(false));
                                    ui.add(egui::Label::new(egui::RichText::new(bounded_line(lines[index])).font(font.clone())).wrap_mode(egui::TextWrapMode::Extend));
                                });
                            }
                        });
                });

            let mut footer = ui.new_child(egui::UiBuilder::new().id_salt("review-actions").max_rect(footer_rect));
            footer.set_clip_rect(footer_rect);
            footer.spacing_mut().item_spacing.y = 3.0;
            footer.separator();
            footer.add(egui::Label::new(egui::RichText::new(if expired {
                if paste { "预览已过期，请取消并重新粘贴，以便重新核对目标。" } else { "预览已过期，请返回编辑后重新确认；命令与目标会保留。" }
            } else if append_enter { "发送后追加 Enter；多行内容中的换行也可能触发执行。" }
                else { "不追加末尾 Enter；内容中已有的换行仍可能触发执行。" }).small().color(if expired { theme::WARNING } else { theme::MUTED })).truncate());
            if strong_warning { footer.checkbox(&mut editor.risk_ack, if paste { "我已核对风险，并确认粘贴" } else { "我已核对风险，并确认发送" }); }
            footer.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!expired && (!strong_warning || editor.risk_ack), egui::Button::new(egui::RichText::new(&send_label).strong().color(theme::ACCENT)).fill(theme::ACCENT_FILL).min_size(egui::vec2(154.0, 34.0))).clicked() { decision = Decision::Send; }
                let edit = ui.add_sized([104.0, 34.0], egui::Button::new(if paste { "取消粘贴" } else { "返回编辑" }));
                if editor.review_focus_requested { edit.request_focus(); editor.review_focus_requested = false; }
                if edit.clicked() { decision = Decision::Edit; }
                ui.menu_button("详细信息", |ui| {
                    ui.set_max_width(360.0);
                    ui.label(format!("{}：{bytes} 字节", if paste { "粘贴数据" } else { "发送数据（含所选末尾 Enter）" }));
                    ui.label("提交时会再次校验会话身份、连接状态和有效期。配置地址在本次预览创建时记录，用于核对目标。");
                    ui.label("正文按每个终端的粘贴模式发送，不保证逐台完成或遇错停止。入队不表示远端执行成功。");
                    ui.label("预览有效期 30 秒。控制字符的方括号标记仅用于显示，发送内容不改写。");
                });
            });
        });
    // Clicking outside keeps the review open. Escape explicitly returns to editing.
    if response.is_top_modal
        && !response.any_popup_open
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        decision = Decision::Edit;
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pathological_line_layout_is_bounded_without_splitting_utf8_or_altering_payload() {
        let text = "中文🙂".repeat(100_000);
        let preview = bounded_line(&text);
        assert!(preview.chars().count() < 4200);
        assert!(preview.contains("发送原文未截断"));
        assert_eq!(text.chars().count(), 300_000);
        assert!(matches!(bounded_line("hostname"), std::borrow::Cow::Borrowed(_)));
    }
    #[test]
    fn display_preserves_natural_lines_but_keeps_hidden_controls_visible() {
        assert_eq!(
            readable_body("echo one\r\necho two\n").as_str(),
            "echo one\necho two\n"
        );
        assert_eq!(
            readable_body("a\t\r\x1b\u{202e}\u{2028}").as_str(),
            "a[TAB][CR][U+001B][U+202E][U+2028]"
        );
        let risk = SyncInputRisk {
            multiline: true,
            control_characters: true,
            destructive_command: false,
        };
        assert!(!extra_ack("echo one\necho two", risk));
        assert!(!extra_ack("echo one\r\necho two", risk));
        for text in ["echo\tfoo", "echo one\rfoo", "echo\u{200b}foo"] {
            assert!(extra_ack(text, risk));
        }
        assert!(extra_ack(
            "rm -rf /tmp/test",
            SyncInputRisk {
                destructive_command: true,
                ..Default::default()
            }
        ));
    }

    #[test]
    fn review_is_centered_and_keeps_actions_visible_with_many_hosts_long_text_and_dpi() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for size in [
                egui::vec2(720.0, 480.0),
                egui::vec2(900.0, 600.0),
                egui::vec2(1280.0, 800.0),
            ] {
                for (complex, paste) in [(false, false), (true, false), (false, true), (true, true)] {
                    let ctx = crate::ui_layout_gate::context();
                    ctx.set_pixels_per_point(scale);
                    let window = WorkspaceWindowId::new();
                    let targets: Vec<_> = (0..if complex { 64 } else { 2 })
                        .map(|i| SyncInputTarget {
                            tab_id: TabId::new(),
                            session_id: cshell_domain::SessionId::new(),
                            kind: SyncInputTargetKind::Ssh,
                            title: format!("生产主机-{i}-{}", "long".repeat(12)),
                        })
                        .collect();
                    let text = if complex {
                        format!(
                            "rm -rf /tmp/demo\n{}\n{}",
                            "长命令".repeat(500),
                            "echo ok\n".repeat(4000)
                        )
                    } else {
                        "hostname\ndf -h".into()
                    };
                    let action = if paste { InputAction::Paste { text, bracketed: true } } else { InputAction::Compose {
                        text,
                        append_enter: false,
                    } };
                    let risk = SyncInputRisk {
                        multiline: true,
                        control_characters: true,
                        destructive_command: complex,
                    };
                    let mut editor = ComposePanel {
                        review_started: Some(std::time::Instant::now()),
                        ..Default::default()
                    };
                    for target in &targets {
                        editor.review_endpoints.insert(
                            target.tab_id,
                            format!("deploy@{}.example:2222", "host".repeat(30)),
                        );
                    }
                    let mut tree = None;
                    for _ in 0..4 {
                        let mut out = ctx.run_ui(
                            egui::RawInput {
                                screen_rect: Some(egui::Rect::from_min_size(
                                    egui::Pos2::ZERO,
                                    size,
                                )),
                                ..Default::default()
                            },
                            |ui| {
                                assert!(
                                    show(
                                        ui.ctx(),
                                        window,
                                        &mut editor,
                                        &action,
                                        &targets,
                                        risk,
                                        20
                                    ) == Decision::Stay
                                );
                            },
                        );
                        out.textures_delta.clear();
                        tree = out.platform_output.accesskit_update;
                    }
                    let rect = ctx
                        .memory(|m| m.area_rect(egui::Id::new(("compose-send-review", window))))
                        .unwrap();
                    let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                    assert!(
                        viewport.shrink(15.0).contains_rect(rect),
                        "{scale}/{size:?}/{complex}: {rect:?}"
                    );
                    assert!(
                        (rect.center() - viewport.center()).length() < 2.0,
                        "{scale}/{size:?}: {rect:?}"
                    );
                    assert!(
                        rect.width() <= 922.0 && rect.height() <= size.y * 0.85 + 2.0,
                        "{rect:?}"
                    );
                    let tree = tree.unwrap();
                    let close = &tree
                        .nodes
                        .iter()
                        .find(|(_, n)| n.label() == Some("×"))
                        .unwrap()
                        .1;
                    let close_rect = crate::ui_layout_gate::bounds(close);
                    assert!(
                        (rect.right() - close_rect.right()).abs() <= 24.0,
                        "close must stay at the upper-right corner: {close_rect:?}/{rect:?}"
                    );
                    for label in [
                        if paste { "取消粘贴" } else { "返回编辑" }.to_owned(),
                        format!("{}到 {} 台主机", if paste { "粘贴" } else { "发送" }, targets.len()),
                        "详细信息".into(),
                    ] {
                        let node = &tree
                            .nodes
                            .iter()
                            .find(|(_, n)| n.label() == Some(label.as_str()))
                            .unwrap_or_else(|| panic!("missing {label}"))
                            .1;
                        assert!(
                            rect.expand(1.0)
                                .contains_rect(crate::ui_layout_gate::bounds(node)),
                            "{label} outside {rect:?}"
                        );
                    }
                }
            }
        }
    }
}
