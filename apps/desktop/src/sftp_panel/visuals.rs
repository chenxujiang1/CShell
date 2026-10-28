use super::*;
pub(super) use cshell_ui::workbench_icons::{Icon, paint};
use egui::{Color32, FontId, Rect, RichText, Stroke, vec2};
pub(super) const UPLOAD: Color32 = cshell_ui::theme::WARNING;
pub(super) const DOWNLOAD: Color32 = Color32::from_rgb(133, 184, 239);
pub(super) fn direction_color(local_target: bool) -> Color32 {
    if local_target { DOWNLOAD } else { UPLOAD }
}
pub(super) fn tool(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    accent: Option<Color32>,
    compact: bool,
) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let color = if !ui.is_enabled() {
            cshell_ui::theme::DISABLED
        } else {
            accent.unwrap_or(cshell_ui::theme::TEXT)
        };
        let font = FontId::proportional(13.0);
        let text = ui
            .painter()
            .layout_no_wrap(label.into(), font.clone(), color);
        let (rect, response) = ui.allocate_exact_size(
            vec2(if compact { 26.0 } else { text.size().x + 34.0 }, 24.0),
            egui::Sense::click(),
        );
        let fill = if response.hovered() {
            cshell_ui::theme::RAISED
        } else {
            accent
                .map(|c| c.gamma_multiply(0.12))
                .unwrap_or(cshell_ui::theme::PANEL)
        };
        ui.painter().rect_filled(rect, 2.0, fill);
        ui.painter().rect_stroke(
            rect,
            2.0,
            Stroke::new(
                1.0,
                accent
                    .unwrap_or(cshell_ui::theme::LINE)
                    .gamma_multiply(if enabled { 0.75 } else { 0.3 }),
            ),
            egui::StrokeKind::Inside,
        );
        paint(
            ui.painter(),
            egui::pos2(rect.left() + 13.0, rect.center().y),
            icon,
            color,
        );
        if !compact {
            ui.painter().galley(
                egui::pos2(rect.left() + 26.0, rect.center().y - text.size().y / 2.0),
                text,
                color,
            );
        }
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
        });
        response
    })
    .inner
}
/// Right-aligned roots keep menu hit targets inside narrow native windows from the first frame.
pub(super) fn menu(
    ui: &mut egui::Ui,
    label: &str,
    width: f32,
    contents: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let response = ui.button(label);
    egui::Popup::menu(&response)
        .align(egui::RectAlign::BOTTOM_END)
        .width(width)
        .show(contents);
    response
}
pub(super) fn peer_label(view: &SftpView) -> Option<String> {
    let peer = view.peer.as_ref()?;
    let host = if peer.host.host.contains(':') {
        format!("[{}]", peer.host.host)
    } else {
        peer.host.host.clone()
    };
    Some(format!("{}@{}:{}", peer.username, host, peer.host.port))
}
impl SftpPanel {
    pub(super) fn host_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: Option<&SftpView>,
        command: &mut Option<SftpClientCommand>,
    ) {
        ui.horizontal(|ui| {
            let owned = view.is_some_and(|v| v.connection_profile.is_some());
            let host_color = if view.is_some_and(|v| {
                v.connection_profile.is_some()
                    && v.connection_phase != crate::sftp_connection::ConnectionPhase::Connected
            }) {
                cshell_ui::theme::MUTED
            } else {
                cshell_ui::theme::ACCENT
            };
            let shared=view.and_then(|v|v.connection_info.as_ref()).is_some_and(|i|i.shared_from.is_some());
            let menu_width =
                (if self.page == 0 { 88.0 } else { 176.0 }) + if owned { 230.0 } else { 0.0 } + if shared {80.0} else {0.0};
            let host_width = (ui.available_width() - menu_width - 8.0).max(80.0);
            ui.allocate_ui_with_layout(
                vec2(host_width, 28.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(22.0, 24.0), egui::Sense::hover());
                    paint(ui.painter(), rect.center(), Icon::Server, host_color);
                    let label = view.and_then(peer_label).unwrap_or_else(|| {
                        if view.is_some_and(|v| {
                            v.connection_profile.is_some() && !v.connection_phase.busy()
                        }) {
                            "尚未验证主机身份".into()
                        } else {
                            "正在读取当前主机…".into()
                        }
                    });
                    ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::Label::new(
                            RichText::new(format!("当前主机 · {label}"))
                                .size(15.0)
                                .strong()
                                .color(host_color),
                        )
                        .halign(egui::Align::Min)
                        .truncate(),
                    )
                    .on_hover_text(label);
                },
            );
            if let Some((view, profile)) = view.and_then(|v| v.connection_profile.map(|p| (v, p))) {
                let phase = view.connection_phase;
                ui.colored_label(
                    if phase == crate::sftp_connection::ConnectionPhase::Connected {
                        cshell_ui::theme::ACCENT
                    } else {
                        cshell_ui::theme::WARNING
                    },
                    phase.label(),
                );
                if view.connection_info.as_ref().is_some_and(|i|i.shared_from.is_some()) {
                    ui.weak("复用 SSH").on_hover_text("使用已认证的 SSH 连接，无需再次输入动态码；关闭任一窗口不会关闭其他渠道");
                }
                if tool(
                    ui,
                    Icon::Disconnect,
                    "断开",
                    matches!(
                        phase,
                        crate::sftp_connection::ConnectionPhase::Connected
                            | crate::sftp_connection::ConnectionPhase::Uncertain
                    ),
                    None,
                    false,
                )
                .on_hover_text("仅断开 SFTP；正在传输的任务将中断，SSH 终端不受影响")
                .clicked()
                {
                    *command = Some(SftpClientCommand::Disconnect(session));
                }
                if tool(
                    ui,
                    Icon::Reconnect,
                    "重新连接",
                    !phase.busy(),
                    Some(cshell_ui::theme::ACCENT),
                    false,
                )
                .on_hover_text("有可用 SSH 时复用连接，否则重新认证；中断任务需检查后恢复")
                .clicked()
                {
                    *command = Some(crate::sftp_connection::lifecycle::connect(
                        profile,
                        view.connection_previous.or(Some(session)),
                    ));
                }
            }
            if self.page != 0
                && tool(ui, Icon::Back, "文件与传输", true, None, false)
                    .on_hover_text("返回文件传输")
                    .clicked()
            {
                self.page = 0;
            }
            ui.add_enabled_ui(
                !owned
                    || view.is_some_and(|v| {
                        v.connection_phase == crate::sftp_connection::ConnectionPhase::Connected
                    }),
                |ui| {
                    if ui.available_width()>380.0 && ui.button("目录比较 / 同步").clicked(){self.open_directory_sync();self.page=0;}
                    menu(ui, "更多功能", 180.0, |ui| {
                        cshell_ui::form::menu(ui);
                        ui.set_width(180.0);
                        for (page, label) in [
                            (0, "文件传输"),
                            (1, "队列详情"),
                            (2, "远程编辑"),
                            (3, "历史 / 迁移"),
                        ] {
                            if ui
                                .add_sized(
                                    [160.0, 26.0],
                                    egui::Button::new(label).selected(self.page == page),
                                )
                                .clicked()
                            {
                                self.page = page;
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("目录比较 / 同步").clicked(){self.open_directory_sync();self.page=0;ui.close();}
                        if ui.button("刷新任务").clicked() {
                            *command = Some(SftpClientCommand::QueueGet(session));
                            ui.close();
                        }
                    });
                },
            );
        });
    }
    pub(super) fn default_directory_menu(&mut self, ui: &mut egui::Ui, view: &SftpView) {
        egui::containers::menu::SubMenuButton::from_button(
            egui::Button::new("默认目录").right_text(()),
        )
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            ui.label("SFTP 打开时使用的本地文件夹");
            ui.add(
                egui::TextEdit::singleline(&mut self.default_local_directory)
                    .hint_text("留空使用当前用户目录")
                    .desired_width(260.0),
            );
            if ui.button("使用当前目录").clicked()
                && let Some(path) = view.local_directory.as_deref()
            {
                self.default_local_directory = path.into();
            }
            if ui.button("恢复用户目录").clicked() {
                self.default_local_directory.clear();
            }
            if ui.button("保存默认目录").clicked() {
                self.default_directory_status = Some(
                    match crate::sftp_settings::save_local_directory(&self.default_local_directory)
                    {
                        Ok(()) => "已保存，下次打开 SFTP 时生效".into(),
                        Err(e) => e,
                    },
                );
            }
            if let Some(status) = &self.default_directory_status {
                ui.add(egui::Label::new(status).wrap());
            }
        });
    }
}
#[derive(Clone, Debug)]
pub(super) struct DropFeedback {
    pub allowed: bool,
    pub local: bool,
    pub caption: String,
    pub path: String,
}
impl DropFeedback {
    pub fn new(
        local: bool,
        loaded: Option<&str>,
        source_local: bool,
        same_session: bool,
        count: usize,
    ) -> Self {
        let (allowed, caption) = if !same_session {
            (false, "连接已切换，请重新拖动".into())
        } else if local == source_local {
            (
                false,
                if local {
                    "请拖到远端列表上传"
                } else {
                    "请拖到本地列表下载"
                }
                .into(),
            )
        } else if loaded.is_none() {
            (false, "请先载入目标目录".into())
        } else if count == 0 || count > 128 {
            (false, "一次请选择 1–128 个文件或文件夹".into())
        } else {
            (
                true,
                format!(
                    "松开{} · {} 个文件",
                    if local { "下载" } else { "上传" },
                    count
                ),
            )
        };
        Self {
            allowed,
            local,
            caption,
            path: loaded.unwrap_or("目录尚未载入").into(),
        }
    }
    pub fn paint(&self, ui: &egui::Ui, rect: Rect) {
        let color = if self.allowed {
            direction_color(self.local)
        } else {
            cshell_ui::theme::DANGER
        };
        ui.painter()
            .rect_filled(rect.shrink(1.0), 2.0, color.gamma_multiply(0.10));
        ui.painter()
            .rect_stroke(rect, 2.0, Stroke::new(2.5, color), egui::StrokeKind::Inside);
        let banner =
            Rect::from_center_size(rect.center(), vec2((rect.width() - 24.0).min(520.0), 72.0));
        ui.painter()
            .rect_filled(banner, 4.0, cshell_ui::theme::INPUT);
        ui.painter().rect_stroke(
            banner,
            4.0,
            Stroke::new(1.0, color),
            egui::StrokeKind::Inside,
        );
        let painter = ui.painter().with_clip_rect(banner.shrink(10.0));
        painter.text(
            banner.center_top() + vec2(0.0, 22.0),
            egui::Align2::CENTER_CENTER,
            &self.caption,
            FontId::proportional(16.0),
            color,
        );
        painter.text(
            banner.center_bottom() - vec2(0.0, 18.0),
            egui::Align2::CENTER_CENTER,
            &self.path,
            FontId::proportional(12.0),
            cshell_ui::theme::TEXT,
        );
    }
}
