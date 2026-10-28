use crate::{
    daemon_connection::DesktopConnectionConfig,
    profile_connection::DesktopProfileCatalog,
    zmodem_connection::Connection,
    zmodem_settings::{self, Settings},
};
use cshell_ipc::{ZmodemRequest, ZmodemTransfer};
mod defaults;
use std::{collections::BTreeSet, sync::mpsc};

#[derive(Default)]
pub(crate) struct Panel {
    pub settings_open: bool,
    settings_page: u8,
    display_font_pixels: Option<u16>,
    display_request: Option<u16>,
    workbench_layout: cshell_ui::workbench_layout::Layout,
    layout_request: Option<cshell_ui::workbench_layout::Layout>,
    keyboard_settings_request: bool,
    pub open: bool,
    auto_open: bool,
    active: bool,
    destinations: std::collections::BTreeMap<u64, String>,
    connection: Option<Connection>,
    generation: u64,
    seen: BTreeSet<u64>,
    handled: BTreeSet<u64>,
    hidden: BTreeSet<u64>,
    picker: Option<mpsc::Receiver<Picked>>,
    settings: Settings,
    settings_loaded: bool,
    error: Option<String>,
    saved: bool,
    security: crate::vault_panel::VaultPanel,
    security_request: Option<cshell_ipc::VaultRequest>,
}
struct Picked {
    target: Option<ZmodemTransfer>,
    paths: Option<Vec<String>>,
}
impl Panel {
    pub fn sync_security(&mut self,view:&crate::profile_connection::DesktopProfileView){self.security.response=view.vault.clone();self.security.error=view.vault_error.clone();self.security.set_operation_state(view.vault_mutations_pending>0,view.vault_notice.clone());}
    pub fn take_security_request(&mut self)->Option<cshell_ipc::VaultRequest>{self.security_request.take()}
    pub fn clear_secrets(&mut self){self.security.clear();}
    pub fn poll(&mut self, config: Option<&DesktopConnectionConfig>) -> bool {
        if self.connection.is_none()
            && let Some(config) = config
        {
            match Connection::start(config.clone()) {
                Ok(c) => self.connection = Some(c),
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        let generation = self
            .connection
            .as_ref()
            .map(|c| c.view().generation)
            .unwrap_or(0);
        let changed = self.generation != generation;
        self.generation = generation;
        changed || self.picker.is_some()
    }
    pub fn blocks_input(&self) -> bool {
        self.settings_open || self.picker.is_some() || self.active
    }
    fn update_completion(&mut self, transfers: &[ZmodemTransfer], error: Option<&str>) {
        if self.auto_open
            && self.picker.is_none()
            && self.error.is_none()
            && error.is_none()
            && !transfers.is_empty()
            && transfers
                .iter()
                .filter(|s| !self.hidden.contains(&s.transfer_id))
                .all(|s| matches!(s.state, 3 | 4))
        {
            self.open = false;
            self.auto_open = false;
        }
    }
    fn submit(&mut self, s: &ZmodemTransfer, operation: u32, paths: Vec<String>) -> bool {
        let request_path = paths.first().cloned();
        let request = ZmodemRequest {
            operation,
            session_id: s.session_id.clone(),
            transfer_id: s.transfer_id,
            paths,
        };
        if self.connection.as_ref().is_some_and(|c| c.request(request)) {
            self.error = None;
            if operation == 2 {
                if let Some(path) = request_path {
                    self.destinations.insert(s.transfer_id, path);
                }
            }
            true
        } else {
            self.error = Some("传输操作队列繁忙，请重试".into());
            false
        }
    }
    fn pick(&mut self, target: Option<ZmodemTransfer>, directory: bool) {
        if self.picker.is_some() {
            return;
        }
        let (send, recv) = mpsc::channel();
        let start = zmodem_settings::resolve(&zmodem_settings::load(), None);
        let result = std::thread::Builder::new()
            .name("cshell-file-picker".into())
            .spawn(move || {
                let dialog = rfd::FileDialog::new()
                    .set_directory(start)
                    .set_title(if directory {
                        "选择下载目录"
                    } else {
                        "ZMODEM：选择上传文件"
                    });
                let paths = if directory {
                    dialog.pick_folder().map(|p| vec![p])
                } else {
                    dialog.pick_files()
                };
                let paths = paths.map(|paths| {
                    paths
                        .into_iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect()
                });
                let _ = send.send(Picked { target, paths });
            });
        match result {
            Ok(_) => self.picker = Some(recv),
            Err(e) => self.error = Some(format!("无法打开文件选择框：{e}")),
        }
    }
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        catalog: Option<&DesktopProfileCatalog>,
        owned: &[Vec<u8>],
    ) {
        if let Some(result) = self.picker.as_ref().and_then(|r| r.try_recv().ok()) {
            self.picker = None;
            if let Some(target) = result.target {
                if let Some(paths) = result.paths {
                    self.submit(&target, target.direction, paths);
                } else {
                    self.submit(&target, 3, Vec::new());
                }
            } else if let Some(paths) = result.paths {
                self.settings.download_directory = paths.into_iter().next().unwrap_or_default();
                self.saved = false;
            }
        }
        let view = self
            .connection
            .as_ref()
            .map(|c| c.view())
            .unwrap_or_default();
        let transfers: Vec<_> = view
            .response
            .transfers
            .iter()
            .filter(|s| owned.contains(&s.session_id))
            .cloned()
            .collect();
        self.active = transfers.iter().any(|s| matches!(s.state, 1 | 2));
        for s in &transfers {
            if self.seen.insert(s.transfer_id) && !matches!(s.state, 3 | 4) {
                self.open = true;
                self.auto_open = true;
            }
            if s.state == 1 && !self.handled.contains(&s.transfer_id) {
                if s.direction == 2 {
                    let override_path = catalog
                        .and_then(|c| {
                            c.ssh_connections.iter().find(|c| {
                                s.profile_id.as_deref()
                                    == Some(c.profile_id.as_uuid().as_bytes().as_slice())
                            })
                        })
                        .and_then(|c| c.options.zmodem_download_directory.as_deref());
                    // Wait for the catalog instead of accidentally ignoring an override.
                    if s.profile_id.is_some() && catalog.is_none() {
                        continue;
                    }
                    let path = zmodem_settings::resolve(&zmodem_settings::load(), override_path);
                    let value = path.to_string_lossy().into_owned();
                    if path.is_absolute() && path.is_dir() {
                        if self.submit(s, 2, vec![value]) {
                            self.handled.insert(s.transfer_id);
                        }
                    } else {
                        self.error = Some(format!("下载目录不可用：{value}，请选择其他目录"));
                        self.handled.insert(s.transfer_id);
                    }
                } else if self.picker.is_none() {
                    self.handled.insert(s.transfer_id);
                    self.pick(Some(s.clone()), false);
                }
            }
        }
        self.update_completion(&transfers, view.error.as_deref());
        let mut open = self.open;
        egui::Window::new("文件传输")
            .id(egui::Id::new("zmodem-transfers"))
            .open(&mut open)
            .default_width(620.0)
            .max_height((ctx.content_rect().height() - 64.0).max(220.0))
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(error) = self.error.as_ref().or(view.error.as_ref()) {
                    egui::Frame::new()
                        .fill(cshell_ui::theme::PANEL)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.colored_label(
                                cshell_ui::theme::WARNING,
                                egui::RichText::new("传输需要处理").strong(),
                            );
                            ui.label(error);
                        });
                }
                egui::ScrollArea::vertical()
                    .max_height(430.0)
                    .show(ui, |ui| {
                        for s in transfers.iter().rev() {
                            if self.hidden.contains(&s.transfer_id) {
                                continue;
                            }
                            egui::Frame::group(ui.style())
                                .inner_margin(14.0)
                                .show(ui, |ui| {
                                    let upload = s.direction == 1;
                                    let accent = if s.state == 5 {
                                        cshell_ui::theme::WARNING
                                    } else {
                                        cshell_ui::theme::ACCENT
                                    };
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            egui::RichText::new(if upload {
                                                "↑ 上传到服务器"
                                            } else {
                                                "↓ 下载到本机"
                                            })
                                            .size(20.0)
                                            .strong()
                                            .color(accent),
                                        );
                                        ui.label(
                                            egui::RichText::new(if !upload && s.state == 1 {
                                                "准备下载"
                                            } else {
                                                state_label(s.state)
                                            })
                                            .strong()
                                            .color(accent),
                                        );
                                    });
                                    ui.label(format!(
                                        "{} → {}",
                                        if upload { "本机" } else { &s.session_title },
                                        if upload { &s.session_title } else { "本机" }
                                    ));
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(if s.name.is_empty() {
                                            if upload {
                                                "等待选择文件"
                                            } else {
                                                "等待服务器发送文件"
                                            }
                                        } else {
                                            &s.name
                                        })
                                        .size(18.0)
                                        .strong(),
                                    );
                                    if !upload {
                                        if let Some(directory) =
                                            self.destinations.get(&s.transfer_id)
                                        {
                                            ui.weak(format!("保存到：{directory}"));
                                        }
                                    } else {
                                        ui.weak("保存到：远端 rz 命令所在目录");
                                    }
                                    let fraction = progress(s);
                                    ui.add_space(4.0);
                                    ui.add(
                                        egui::ProgressBar::new(fraction)
                                            .desired_height(22.0)
                                            .text(format!("{:.0}%", fraction * 100.0)),
                                    );
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(format!(
                                            "{} / {}",
                                            format_bytes(s.bytes),
                                            format_bytes(s.size)
                                        ));
                                        ui.weak(format!("已完成 {} 个文件", s.files_completed));
                                    });
                                    if s.state == 5 {
                                        ui.colored_label(cshell_ui::theme::WARNING, &s.detail);
                                    } else {
                                        ui.weak(&s.detail);
                                    }
                                    ui.horizontal_wrapped(|ui| {
                                        if s.state == 1
                                            && ui
                                                .add_enabled(
                                                    self.picker.is_none(),
                                                    egui::Button::new(if upload {
                                                        "选择上传文件…"
                                                    } else {
                                                        "更改保存目录…"
                                                    }),
                                                )
                                                .clicked()
                                        {
                                            self.pick(Some(s.clone()), !upload);
                                        }
                                        if matches!(s.state, 1 | 2)
                                            && ui.button("取消传输").clicked()
                                        {
                                            self.submit(s, 3, Vec::new());
                                        }
                                        if s.state >= 3 && ui.button("移除记录").clicked() {
                                            self.hidden.insert(s.transfer_id);
                                        }
                                    });
                                });
                            ui.add_space(8.0);
                        }
                        if transfers.is_empty() {
                            ui.label("在 SSH 终端输入 rz 上传，或输入 sz 文件名 下载。");
                        }
                        ui.weak("传输成功后自动关闭；发生错误时保留窗口。");
                    });
            });
        self.open = open;
        self.draw_global_settings(ctx,catalog);
        if self.picker.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

fn state_label(state: u32) -> &'static str {
    match state {
        1 => "等待选择",
        2 => "正在传输",
        3 => "已完成",
        4 => "已取消",
        5 => "传输失败",
        _ => "等待中",
    }
}
fn progress(s: &ZmodemTransfer) -> f32 {
    if s.state == 3 {
        1.0
    } else if s.size > 0 {
        (s.bytes as f64 / s.size as f64).clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}
fn format_bytes(bytes: u64) -> String {
    for (unit, scale) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn success_closes_but_active_failed_and_actionable_errors_stay_visible() {
        let mut p = Panel {
            open: true,
            auto_open: true,
            ..Default::default()
        };
        let mut s = ZmodemTransfer {
            state: 2,
            ..Default::default()
        };
        p.update_completion(&[s.clone()], None);
        assert!(p.open);
        s.state = 5;
        p.update_completion(&[s.clone()], None);
        assert!(p.open);
        s.state = 3;
        p.update_completion(&[s.clone()], Some("problem"));
        assert!(p.open);
        p.update_completion(&[s], None);
        assert!(!p.open);
        let mut p = Panel {
            open: true,
            auto_open: true,
            ..Default::default()
        };
        p.update_completion(
            &[
                ZmodemTransfer {
                    state: 3,
                    ..Default::default()
                },
                ZmodemTransfer {
                    state: 2,
                    ..Default::default()
                },
            ],
            None,
        );
        assert!(p.open);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    #[test]
    fn transfer_direction_file_bytes_and_target_are_readable_in_small_window() {
        for direction in [1, 2] {
            let context = crate::ui_layout_gate::context();
            let id = vec![1; 16];
            let transfer = ZmodemTransfer {
                session_id: id.clone(),
                transfer_id: 1,
                direction,
                state: 2,
                name: "测试文件.tar.gz".into(),
                bytes: 2 * 1024 * 1024,
                size: 8 * 1024 * 1024,
                session_title: "linux-server".into(),
                ..Default::default()
            };
            let mut panel = Panel {
                open: true,
                connection: Some(Connection::preview(vec![transfer])),
                ..Default::default()
            };
            panel.destinations.insert(1, "D:/Downloads".into());
            let size = egui::vec2(720.0, 480.0);
            let mut draw = |ctx: &egui::Context| panel.draw(ctx, None, &[id.clone()]);
            for _ in 0..3 {
                crate::ui_layout_gate::frame(&context, size, &mut draw);
            }
            let tree = crate::ui_layout_gate::frame(&context, size, &mut draw);
            let labels = tree
                .nodes
                .iter()
                .filter_map(|(_, n)| n.label().or(n.value()))
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                labels.contains(if direction == 1 {
                    "上传到服务器"
                } else {
                    "下载到本机"
                }),
                "{labels}"
            );
            assert!(labels.contains("测试文件.tar.gz"));
            assert!(labels.contains("2.0 MiB / 8.0 MiB"));
            if direction == 2 {
                assert!(labels.contains("D:/Downloads"));
            }
            let rect = context
                .memory(|m| m.area_rect(egui::Id::new("zmodem-transfers")))
                .unwrap();
            assert!(
                egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                    .expand(1.0)
                    .contains_rect(rect),
                "{rect:?}"
            );
            assert!(!labels.contains("????"));
        }
    }
}
