use super::*;
impl Panel {
    pub(crate) fn take_keyboard_settings_request(&mut self)->bool {std::mem::take(&mut self.keyboard_settings_request)}
    pub(crate) fn sync_layout(&mut self,layout:cshell_ui::workbench_layout::Layout){self.workbench_layout=layout;}
    pub(crate) fn take_layout_request(&mut self)->Option<cshell_ui::workbench_layout::Layout>{self.layout_request.take()}
    pub(crate) fn open_defaults(&mut self, security: bool) {
        self.settings_open = true;
        self.settings_page = if security { 2 } else { 0 };
    }
    pub(crate) fn sync_display(&mut self, pixels: u16) {
        self.display_font_pixels = Some(pixels);
    }
    pub(crate) fn take_display_request(&mut self) -> Option<u16> {
        self.display_request.take()
    }
    pub(super) fn draw_global_settings(
        &mut self,
        ctx: &egui::Context,
        catalog: Option<&DesktopProfileCatalog>,
    ) {
        if self.settings_open && !self.settings_loaded {
            self.settings = zmodem_settings::load();
            self.settings_loaded = true;
            self.error = None;
            self.saved = false;
        }
        let mut open = self.settings_open;
        cshell_ui::form::dialog(ctx,"全局默认设置").id(egui::Id::new("global-default-settings"))
            .open(&mut open).default_width(620.0).show(ctx,|ui| {
                let previous=self.settings_page;
                ui.horizontal_wrapped(|ui| {
                    cshell_ui::form::tab(ui,&mut self.settings_page,0,"终端显示");
                    cshell_ui::form::tab(ui,&mut self.settings_page,3,"鼠标与剪贴板");
                    cshell_ui::form::tab(ui,&mut self.settings_page,1,"文件传输");
                    cshell_ui::form::tab(ui,&mut self.settings_page,2,"凭据库与安全");
                    if ui.button("键盘快捷键…").clicked(){self.keyboard_settings_request=true;}
                });
                if previous!=self.settings_page {self.security.clear();}
                ui.separator();
                match self.settings_page {
                    0=>{
                        ui.strong("全局终端字号");
                        let pixels=self.display_font_pixels.get_or_insert(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS);
                        let before=*pixels;
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::DragValue::new(pixels).range(cshell_domain::MIN_TERMINAL_FONT_PIXELS..=cshell_domain::MAX_TERMINAL_FONT_PIXELS).suffix(" px"));
                            for size in [22,28,32] {if ui.small_button(format!("{size} px")).clicked(){*pixels=size;}}
                            if ui.button("恢复默认字号 28").clicked(){*pixels=28;}
                        });
                        if before!=*pixels {self.display_request=Some(*pixels);}
                        ui.weak("即时应用到所有终端窗口并自动保存，也可在主窗口状态栏调整。字号属于应用级设置。");
                        let theme=catalog.map_or("default",|c|c.defaults.theme.as_str());
                        crate::terminal_appearance::preview(ui,theme,catalog.map_or(&[],|c|c.themes.as_slice()),*pixels);
                        if let Some(c)=catalog {
                            ui.separator();
                            ui.label(format!("配置全局默认：主题 {} · 终端类型 {} · 日志 {}",c.defaults.theme,c.defaults.terminal_type,if c.defaults.logging {"开启"}else{"关闭"}));
                            ui.weak("会话的主题、终端类型和日志按全局 → 分组 → 本会话继承；可在连接配置中覆盖或恢复继承。文件传输默认值在相邻标签中设置。");
                        }
                        ui.separator();
                        let before=self.workbench_layout.clone();
                        self.workbench_layout.draw(ui);
                        if before!=self.workbench_layout {self.layout_request=Some(self.workbench_layout.clone());}
                    }
                    1=>{
            ui.heading("文件传输");
            ui.label("拖入 SSH 终端时的上传方式");
            let old=self.settings.terminal_upload_protocol;
            egui::ComboBox::from_id_salt("terminal-upload-protocol").selected_text(match old {cshell_application::TerminalUploadProtocol::Sftp=>"SFTP（推荐）",cshell_application::TerminalUploadProtocol::Zmodem=>"ZMODEM"}).show_ui(ui,|ui|{
                ui.selectable_value(&mut self.settings.terminal_upload_protocol,cshell_application::TerminalUploadProtocol::Sftp,"SFTP（推荐）");
                ui.selectable_value(&mut self.settings.terminal_upload_protocol,cshell_application::TerminalUploadProtocol::Zmodem,"ZMODEM");
            });
            if old!=self.settings.terminal_upload_protocol {self.saved=false;}
            ui.weak("上传到此 SSH 的当前目录，复用已认证连接。文件夹始终使用 SFTP 保留结构；ZMODEM 需服务器安装 rz，不向当前终端输入命令。");
            if ui.checkbox(&mut self.settings.confirm_terminal_upload,"拖放后先确认目标目录").changed(){self.saved=false;}
            if ui.checkbox(&mut self.settings.close_successful_upload,"全部上传成功后自动收起进度").changed(){self.saved=false;}
            ui.weak("默认拖入即传，同名文件不覆盖。错误和冲突保留，供检查处理。");
            ui.separator();
            ui.label("SFTP 默认校验策略");
            if zmodem_settings::verification_selector(ui,"global-sftp-verification",&mut self.settings.sftp_verification){self.saved=false;}
            ui.weak("标准：确认写入、核对大小、安全发布，无整文件读回；完整：比较 SHA-256，优先服务器计算，不支持时读回一次。");
            ui.weak("会话可覆盖此设置；新任务使用当时的选择。断点恢复和替换已有文件仍执行严格核对。");
            ui.separator();
            ui.label("ZMODEM / sz 默认下载目录");
            ui.horizontal(|ui|{
                if ui.add(egui::TextEdit::singleline(&mut self.settings.download_directory).desired_width((ui.available_width()-85.0).max(120.0)).hint_text("留空使用用户 Downloads 目录")).changed(){self.saved=false;}
                if ui.add_enabled(self.picker.is_none(),egui::Button::new("浏览…")).clicked(){self.pick(None,true);}
            });
            ui.weak(format!("当前默认：{}",zmodem_settings::resolve(&self.settings,None).display()));
            ui.label("SSH 会话的下载目录留空时继承此设置。修改全局默认后，后续传输立即使用新目录；单独设置目录的会话保持自己的选择。");
            ui.weak("下载完成后才发布文件；同名文件自动改名，不覆盖已有文件。");
            if let Some(error)=&self.error{ui.colored_label(egui::Color32::from_rgb(235,165,80),error);}
            if self.saved{ui.colored_label(egui::Color32::from_rgb(115,195,140),"文件传输默认设置已保存，后续传输立即生效");}
            if ui.button("保存文件传输设置").clicked(){match zmodem_settings::save(&self.settings){Ok(())=>{self.error=None;self.saved=true;},Err(error)=>{self.error=Some(error);self.saved=false;}}}

                    }
                    3=>{crate::terminal_clipboard_settings::draw(ui);}
                    _=>{if let Some(request)=self.security.draw_protection(ui){self.security_request=Some(request);}}
                }
            });
        if self.keyboard_settings_request {open=false;}
        self.settings_open = open;
        if !open {
            self.security.clear();
            self.settings_loaded = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_layout_gate::{bounds, context};
    #[test]
    fn font_preview_fits_small_windows_and_queues_live_display_change_only() {
        for size in [egui::vec2(720.0, 480.0), egui::vec2(900.0, 600.0)] {
            let ctx = context();
            let mut panel = Panel::default();
            panel.open_defaults(false);
            panel.settings_loaded = true; // Isolated draft; never load or write user settings.
            panel.sync_display(28);
            let mut draw = |events: Vec<egui::Event>| {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        events,
                        ..Default::default()
                    },
                    |ui| panel.draw_global_settings(ui.ctx(), None),
                );
                output.textures_delta.clear();
                output.platform_output.accesskit_update.unwrap()
            };
            for _ in 0..3 {
                draw(vec![]);
            }
            let tree = draw(vec![]);
            let pos = bounds(
                &tree
                    .nodes
                    .iter()
                    .find(|(_, n)| n.label() == Some("32 px"))
                    .unwrap()
                    .1,
            )
            .center();
            assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains(pos));
            draw(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            draw(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            assert_eq!(panel.take_display_request(), Some(32));
            assert_eq!(panel.take_display_request(), None);
            assert!(panel.take_security_request().is_none());
            assert!(!panel.saved);
            panel.open_defaults(true);
            assert_eq!(panel.settings_page, 2);
        }
    }
    #[test]
    fn preferences_remain_visible_in_small_global_settings_window() {
        for size in [egui::vec2(720.0, 480.0), egui::vec2(900.0, 600.0)] {
            let ctx = crate::ui_layout_gate::context();
            let mut panel = crate::zmodem_panel::Panel::default();
            panel.open_defaults(false);
            panel.settings_loaded = true;
            let mut tree = None;
            for frame in 0..5 {
                let events = if frame == 3 || frame == 4 {
                    let tree: &egui::accesskit::TreeUpdate = tree.as_ref().unwrap();
                    let node = &tree.nodes.iter().find(|(_,n)| n.label() == Some("鼠标与剪贴板")).unwrap().1;
                    let pos = crate::ui_layout_gate::bounds(node).center();
                    vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: frame == 3, modifiers: egui::Modifiers::NONE }]
                } else { vec![] };
                let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO,size)), events, ..Default::default() }, |ui| panel.draw_global_settings(ui.ctx(),None));
                out.textures_delta.clear();
                tree = out.platform_output.accesskit_update;
            }
            let tree = tree.unwrap();
            for label in ["选中后自动复制（松开左键时）", "中键粘贴", "右键直接粘贴"] {
                let node = &tree.nodes.iter().find(|(_,n)| n.label() == Some(label)).unwrap_or_else(|| panic!("missing {label}")).1;
                assert!(egui::Rect::from_min_size(egui::Pos2::ZERO,size).contains_rect(crate::ui_layout_gate::bounds(node)));
            }
        }
    }
}
