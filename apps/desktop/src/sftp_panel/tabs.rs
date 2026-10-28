use super::*;
#[derive(Clone, Debug)]
pub(crate) struct RemoteTab {
    pub id: u64,
    pub title: String,
    pub host: String,
    pub phase: crate::sftp_connection::ConnectionPhase,
    pub active: bool,
    pub running: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TabAction {
    Select(u64),
    Close(u64),
    Open(cshell_domain::ProfileId),
}
impl SftpPanel {
    pub(crate) fn take_tab_action(&mut self) -> Option<TabAction> {
        self.tab_action.take()
    }
    pub(super) fn draw_remote_tabs(&mut self, ui: &mut egui::Ui) -> f32 {
        if self.remote_tabs.is_empty() {
            return 0.0;
        }
        let top = ui.cursor().top();
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui| {
                let plus=ui.button("＋").on_hover_text("新建 SFTP 标签：选择保存的服务器");
                plus.widget_info(||egui::WidgetInfo::labeled(egui::WidgetType::Button,true,"新建 SFTP 标签"));
                egui::Popup::menu(&plus).align(egui::RectAlign::BOTTOM_END).width(240.0).show(|ui| {
                    ui.set_width(240.0);
                    if self.available_hosts.is_empty(){ui.label("请先在会话管理器保存服务器配置。");}
                    egui::ScrollArea::vertical().max_height(300.0).show(ui,|ui| {
                        for (profile,title,host) in &self.available_hosts {
                            if ui.add_sized([220.0,26.0],egui::Button::new(title).truncate()).on_hover_text(host).clicked(){self.tab_action=Some(TabAction::Open(*profile));ui.close();}
                        }
                    });
                });
                let width=ui.available_width();
                ui.allocate_ui_with_layout(egui::vec2(width,26.0),egui::Layout::left_to_right(egui::Align::Center),|ui| {
                    egui::ScrollArea::horizontal().id_salt("sftp-host-tabs").auto_shrink([false,true]).max_width(width).show(ui,|ui| {
                        ui.horizontal(|ui| {ui.spacing_mut().item_spacing.x=2.0;
                            for tab in &self.remote_tabs {ui.push_id(tab.id,|ui| {
                                let marker=if tab.phase==crate::sftp_connection::ConnectionPhase::Connected {"●"}else{"○"};
                                let title=if tab.running>0 {format!("{marker} {} · {}",tab.title,tab.running)}else{format!("{marker} {}",tab.title)};
                                let response=ui.add_sized([140.0,26.0],egui::Button::new(egui::RichText::new(title).color(if tab.active {cshell_ui::theme::ACCENT}else{cshell_ui::theme::TEXT})).selected(tab.active).truncate()).on_hover_text(format!("{}\n{} · {} 个传输中任务",tab.host,tab.phase.label(),tab.running));
                                response.widget_info(||egui::WidgetInfo::selected(egui::WidgetType::Button,true,tab.active,format!("SFTP 标签 · {}",tab.title)));
                                if response.clicked(){self.tab_action=Some(TabAction::Select(tab.id));}
                                let close=ui.add_sized([22.0,26.0],egui::Button::new("×").frame(false)).on_hover_text("关闭此主机标签并断开它的 SFTP；中断任务保留在历史中");
                                close.widget_info(||egui::WidgetInfo::labeled(egui::WidgetType::Button,true,format!("关闭 SFTP 标签 · {}",tab.title)));
                                if close.clicked(){self.tab_action=Some(TabAction::Close(tab.id));}
                                ui.add_space(5.0);
                            });}
                        });
                    });
                });
            });
        });
        ui.separator();
        ui.cursor().top() - top
    }
}
