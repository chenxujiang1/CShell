use crate::sftp_connection::{SftpClientCommand, SftpView};
use cshell_domain::SessionId;
use cshell_ipc::SftpTransferState;

#[derive(Debug)]
pub struct SftpPanel {
    pub open: bool,
    remote_directory: String,
    remote_file: String,
    local_file: String,
    bound_session_id: Option<SessionId>,
}

impl Default for SftpPanel {
    fn default() -> Self {
        Self {
            open: false,
            remote_directory: "/".into(),
            remote_file: String::new(),
            local_file: String::new(),
            bound_session_id: None,
        }
    }
}

impl SftpPanel {
    pub fn draw(
        &mut self,
        context: &egui::Context,
        session_id: Option<SessionId>,
        view: Option<&SftpView>,
    ) -> Option<SftpClientCommand> {
        if !self.open {
            return None;
        }
        if self.bound_session_id != session_id {
            self.bound_session_id = session_id;
            self.remote_directory = "/".into();
            self.remote_file.clear();
            self.local_file.clear();
        }
        let mut open = self.open;
        let mut command = None;
        egui::Window::new("SFTP 文件")
            .open(&mut open)
            .default_size([700.0, 550.0])
            .resizable(true)
            .show(context, |ui| {
                let Some(session_id) = session_id else {
                    ui.label("请先选择一个已连接的 SSH 会话。");
                    return;
                };
                ui.horizontal(|ui| {
                    ui.label("远程目录");
                    ui.text_edit_singleline(&mut self.remote_directory);
                    if ui.button("列出").clicked() {
                        command = Some(SftpClientCommand::List(
                            session_id,
                            self.remote_directory.clone(),
                        ));
                    }
                    if ui.button("刷新传输").clicked() {
                        command = Some(SftpClientCommand::Recover(session_id));
                    }
                });
                ui.label("选择文件可填入远程路径；双击目录可进入。");
                if let Some(view) = view.filter(|view| view.session_id == Some(session_id)) {
                    if let Some(error) = &view.error {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    } else {
                        ui.label(&view.status);
                    }
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for entry in &view.entries {
                                let directory = entry.kind == 2;
                                let label = if directory {
                                    format!("目录 {}", entry.name)
                                } else {
                                    format!("{}  {} B", entry.name, entry.size.unwrap_or(0))
                                };
                                let clicked =
                                    ui.selectable_label(self.remote_file == entry.path, label);
                                if clicked.clicked() {
                                    self.remote_file.clone_from(&entry.path);
                                }
                                if directory && clicked.double_clicked() {
                                    self.remote_directory.clone_from(&entry.path);
                                    command = Some(SftpClientCommand::List(
                                        session_id,
                                        entry.path.clone(),
                                    ));
                                }
                            }
                            if view.truncated {
                                ui.weak("目录超过 512 项；当前基础版只显示前 512 项。");
                            }
                        });
                    if let Some(transfer) = &view.transfer {
                        ui.separator();
                        let total = transfer
                            .total_bytes
                            .map_or_else(|| "?".into(), |value| value.to_string());
                        ui.label(format!(
                            "传输：{} / {} B · {}",
                            transfer.bytes_transferred, total, view.status
                        ));
                        if transfer.state == SftpTransferState::Running as i32
                            && ui.button("取消传输").clicked()
                        {
                            command = Some(SftpClientCommand::Cancel(
                                session_id,
                                transfer.transfer_id.clone(),
                            ));
                        }
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("本地绝对路径");
                    ui.text_edit_singleline(&mut self.local_file);
                });
                ui.horizontal(|ui| {
                    ui.label("远程文件");
                    ui.text_edit_singleline(&mut self.remote_file);
                });
                ui.horizontal(|ui| {
                    if ui.button("上传到远程路径").clicked() {
                        command = Some(SftpClientCommand::Upload(
                            session_id,
                            self.local_file.clone(),
                            self.remote_file.clone(),
                        ));
                    }
                    if ui.button("下载到本地路径").clicked() {
                        command = Some(SftpClientCommand::Download(
                            session_id,
                            self.remote_file.clone(),
                            self.local_file.clone(),
                        ));
                    }
                });
                ui.weak("先写临时文件并检查已有目标；远端提交结果取决于服务器。");
            });
        self.open = open;
        command
    }
}
