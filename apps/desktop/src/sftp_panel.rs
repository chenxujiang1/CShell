use crate::sftp_connection::{SftpClientCommand, SftpView};
use cshell_application::{SftpQueueDirection, SftpQueueDraft, SftpQueueState};
use cshell_domain::SessionId;
use cshell_ipc::SftpOperation;
use cshell_ipc::{MAX_SFTP_PATH_BYTES, SftpCleanupLocation, SftpTransferState};
mod advanced;
mod browser;
mod layout;
mod visuals;
mod connection;
mod tabs;
pub(crate) use tabs::{RemoteTab,TabAction};
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod workflow_preview;

pub struct SftpPanel {
    terminal_path: Option<TerminalPath>,
    pub open: bool,
    pub(crate) remote_tabs:Vec<RemoteTab>,
    pub(crate) available_hosts:Vec<(cshell_domain::ProfileId,String,String)>,
    pub(crate) verification_overrides:std::collections::BTreeMap<cshell_domain::ProfileId,cshell_domain::SftpVerification>,
    tab_action:Option<TabAction>,
    host_prompt: Option<crate::host_key_prompt::HostKeyPrompt>,
    prompt_attempt: Option<SessionId>,
    profile_command: Option<crate::profile_connection::ProfileClientCommand>,
    page: u8,
    remote_directory: String,
    remote_file: String,
    local_file: String,
    bound_session_id: Option<SessionId>,
    local_directory: String,
    default_local_directory: String,
    default_directory_status: Option<String>,
    queue_confirm: bool,
    start_queue_confirm: bool,
    edit_queue_confirm: bool,
    queue_stamp: Option<u64>,
    recover_queue_confirm: bool,
    advanced: advanced::AdvancedSftp,
    browser: browser::BrowserState,
    layout: layout::LayoutState,
}
struct TerminalPath { session:SessionId,path:String,download:bool,phase:u8,generation:u64 }

impl std::fmt::Debug for SftpPanel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpPanel")
            .field("open", &self.open)
            .field("bound_session_id", &self.bound_session_id)
            .finish_non_exhaustive()
    }
}

impl Default for SftpPanel {
    fn default() -> Self {
        Self {
            terminal_path: None,
            open: false,
            remote_tabs:Vec::new(),available_hosts:Vec::new(),tab_action:None,
            verification_overrides:Default::default(),
            host_prompt:None,prompt_attempt:None,profile_command:None,
            page: 0,
            remote_directory: "/".into(),
            remote_file: String::new(),
            local_file: String::new(),
            bound_session_id: None,
            local_directory: String::new(),
            default_local_directory: crate::sftp_settings::local_directory(),
            default_directory_status: None,
            queue_confirm: false,
            start_queue_confirm: false,
            edit_queue_confirm: false,
            queue_stamp: None,
            recover_queue_confirm: false,
            advanced: advanced::AdvancedSftp::default(),
            browser: browser::BrowserState::default(),
            layout: layout::LayoutState::load(),
        }
    }
}

impl SftpPanel {
    pub(crate) fn queue_terminal_path(&mut self,session:SessionId,path:String,download:bool) {
        self.page=0;self.open=true;
        self.terminal_path=Some(TerminalPath{session,path,download,phase:0,generation:0});
    }
    fn browse(&self, session: SessionId, local: bool, path: String) -> SftpClientCommand {
        SftpClientCommand::BrowseManaged(
            session,
            cshell_application::SftpBrowseOptions {
                local,
                path,
                links: if self.advanced.follow_links {
                    cshell_application::SftpLinkPolicy::Follow
                } else {
                    cshell_application::SftpLinkPolicy::NoFollow
                },
            },
        )
    }
    // Retained for dialog layout previews; the desktop uses draw_contents in a native window.
    #[cfg(test)]
    pub fn draw(
        &mut self,
        context: &egui::Context,
        session_id: Option<SessionId>,
        view: Option<&SftpView>,
    ) -> Option<SftpClientCommand> {
        if !self.open {
            return None;
        }
        let mut open = self.open;
        let mut command = None;
        let bounds = context.content_rect().shrink(12.0);
        cshell_ui::form::dialog(context, "SFTP 文件")
            .open(&mut open)
            .default_size([
                1560.0_f32.min(bounds.width()),
                800.0_f32.min(bounds.height()),
            ])
            .min_size(egui::vec2(660.0, 400.0).min(bounds.size()))
            .vscroll(false)
            .resizable(true)
            .show(context, |ui| {
                command = self.draw_contents(ui, session_id, view)
            });
        self.open = open;
        command
    }

    pub fn draw_contents(
        &mut self,
        ui: &mut egui::Ui,
        session_id: Option<SessionId>,
        view: Option<&SftpView>,
    ) -> Option<SftpClientCommand> {
        let context = ui.ctx().clone();
        if self.bound_session_id != session_id {
            let local=self.take_local_ui();
            self.bound_session_id = session_id;
            self.remote_directory = "/".into();
            self.remote_file.clear();
            self.local_file.clear();
            self.local_directory.clear();
            self.queue_confirm = false;
            self.start_queue_confirm = false;
            self.edit_queue_confirm = false;
            self.recover_queue_confirm = false;
            self.queue_stamp = None;
            self.advanced.reset();
            self.browser = Default::default();
            self.restore_local_ui(local);
        }
        let view = view.filter(|view| view.session_id == session_id);
        let stamp = view.and_then(|v| v.queue.as_ref()).map(|q| q.revision);
        if self.queue_stamp != stamp {
            self.queue_confirm = false;
            self.start_queue_confirm = false;
            self.edit_queue_confirm = false;
            self.recover_queue_confirm = false;
            self.queue_stamp = stamp;
        }
        let mut command = None;
        (|| {
            let Some(session) = session_id else {
                ui.label("在会话管理器中右键服务器，选择“打开 SFTP”。");
                return;
            };
            self.host_toolbar(ui,session,view,&mut command);
            if let Some(view)=view && crate::connection_diagnostics::draw(ui,view.connection_trace.as_ref(),view.connection_phase==crate::sftp_connection::ConnectionPhase::Connecting,!view.connection_phase.busy()&&command.is_none()) {
                if let Some(profile)=view.connection_profile{command=Some(crate::sftp_connection::lifecycle::connect(profile,view.connection_previous.or(Some(session))));}
            }
            ui.separator();
            if let Some(view) = view {
                if let Some(error) = &view.error {
                    let feedback=cshell_ui::feedback::explain(error);
                    cshell_ui::form::utility_bar().show(ui,|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(cshell_ui::theme::WARNING,feedback.title);
                            if !view.connection_phase.busy() && ui.button("刷新任务状态").clicked(){command=Some(SftpClientCommand::QueueGet(session));}
                            let connected=view.connection_profile.is_none()||view.connection_phase==crate::sftp_connection::ConnectionPhase::Connected;
                            if connected && ui.button("刷新远端目录").clicked(){command=Some(self.navigate(session,false,view.remote_loaded_directory.clone().unwrap_or_else(||self.remote_directory.clone())));}
                        });
                        if !feedback.guidance.is_empty(){ui.weak(feedback.guidance);}
                        ui.collapsing("错误详情",|ui|{ui.label(error);});
                    });
                }
                if view.connection_profile.is_some() && view.connection_phase != crate::sftp_connection::ConnectionPhase::Connected {self.page=0;}
                if self.page!=0 {self.draw_remote_tabs(ui);}
                if self.page == 0 {
                    self.draw_browser(ui, &context, session, view, &mut command);
                } else if self.page==2 {
                    self.advanced.editor_page(ui,session,view,&self.remote_file,&mut command);
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("sftp-advanced-pages")
                        .show(ui, |ui| {
                            if self.page == 1 && self
                                .advanced
                                .controls(ui, session, Some(view), &mut command)
                            {
                                self.queue_confirm = false;
                            }
                            match self.page {
                                1 => {
                                    self.draw_queue(ui, session, view, &mut command);
                                    self.advanced.pending_edit(ui, session, view, &mut command);
                                    self.advanced.file_management(
                                        ui,
                                        session,
                                        view,
                                        &self.local_file,
                                        &self.remote_file,
                                        &mut command,
                                    );
                                }
                                2 => self.advanced.editor(
                                    ui,
                                    session,
                                    view,
                                    &self.remote_file,
                                    &mut command,
                                ),
                                _ => {
                                    self.advanced.migration(ui, session, view, &mut command);
                                    self.advanced.history(ui, session, view, &mut command);
                                }
                            }
                            self.draw_transfer_warning(ui, view);
                        });
                }
            } else {
                ui.spinner();
                ui.label("正在载入本地和远端目录…");
                if ui.button("重新载入文件会话").clicked() {
                    command = Some(SftpClientCommand::OpenBrowser(session));
                }
            }
        })();
        self.draw_host_prompt(&context,view,&mut command);
        if let (Some(session), Some(view)) = (session_id, view) {
            self.file_operation_dialog(&context, session, view, &mut command);
        }
        if let Some(error)=self.layout.flush(&context) {self.browser.set_layout_error(error);}
        if command.is_none() && let Some(view)=view { command=self.process_terminal_path(&context,view); }
        command
    }

    fn draw_transfer_warning(&self, ui: &mut egui::Ui, view: &SftpView) {
        if let Some(transfer) = &view.transfer
            && transfer.state == SftpTransferState::Failed as i32
        {
            if transfer.destination_committed {
                ui.colored_label(
                    cshell_ui::theme::DANGER,
                    "目标文件已提交；临时文件清理失败。不要重复传输。",
                );
            }
            if let Some(path) = transfer.cleanup_path.as_deref().filter(|path| {
                !path.is_empty() && path.len() <= MAX_SFTP_PATH_BYTES && !path.contains('\0')
            }) {
                ui.label(
                    match SftpCleanupLocation::try_from(transfer.cleanup_location) {
                        Ok(SftpCleanupLocation::Remote) => "远程临时文件",
                        Ok(SftpCleanupLocation::DaemonLocal) => "本地临时文件",
                        _ => "临时文件（位置未说明）",
                    },
                );
                ui.add(egui::Label::new(path).wrap().selectable(true));
            }
        }
    }
    fn draw_queue(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let Some(queue) = &view.queue else {
            return;
        };
        if self.queue_stamp != Some(queue.revision) {
            self.recover_queue_confirm = false;
            self.start_queue_confirm = false;
            self.edit_queue_confirm = false;
            self.queue_stamp = Some(queue.revision);
        }
        ui.collapsing("持久传输队列", |ui| {
            ui.label(format!("版本 {} · {} 条 · {}", queue.revision, queue.items.len(), if queue.paused { "暂停／等待启动" } else { "托管中" }));
            ui.label("新条目核对完整 SHA-256 后发布；暂停只停止后续派发。恢复单个条目需要明确选择，不会继续派发其他 Pending。");
            let ready = view.queue_writable && !view.queue_busy && queue.migrated_to.is_none();
            if let Some(target)=queue.migrated_to{ui.label(format!("已锁定并迁移到会话 {target}；在目标会话载入队列继续。"));}
            ui.checkbox(&mut self.start_queue_confirm, "确认启动这些 Pending 路径");
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(ready && self.start_queue_confirm && queue.paused && !queue.items.iter().any(|i| i.state == SftpQueueState::Running), egui::Button::new("确认启动／继续")).clicked() {
                    let operation = SftpOperation::QueueStartManaged;
                    *command = Some(SftpClientCommand::QueueControl(session, operation, queue.revision, Vec::new())); self.start_queue_confirm = false;
                }
                if ui.add_enabled(ready && !queue.paused, egui::Button::new("暂停后续条目")).clicked() {
                    *command = Some(SftpClientCommand::QueueControl(session, SftpOperation::QueuePause, queue.revision, Vec::new()));
                }
            });
            ui.checkbox(&mut self.edit_queue_confirm, "确认取消或从队列中移除条目（不删除文件）");
            ui.checkbox(&mut self.recover_queue_confirm, "确认所选条目的恢复操作：续传、认领匹配内容，或删除已核对的暂存文件");
            egui::ScrollArea::vertical().id_salt("sftp-queue-items").max_height(200.0).show(ui, |ui| {
                for item in &queue.items {
                    ui.push_id(item.id, |ui| {
                        ui.label(format!("{:?} · {:?} · {}", item.draft.direction, item.state, item.id));
                        ui.label(if item.draft.verified { "完整校验模式" } else { "旧会话传输模式" });
                        ui.label(format!("{} ↔ {}", item.draft.local_path, item.draft.remote_path));
                        if let Some(verified) = &item.verified {
                            ui.label(format!("已确认 {} B · 完整校验 {} · 发布 {} · 清理 {} · 意图 {:?}", verified.bytes, verified.verified, verified.committed, verified.cleaned, verified.intent));
                            if let Some(content) = verified.content { ui.label(format!("SHA-256 {}", content.sha256.iter().map(|b| format!("{b:02x}")).collect::<String>())); }
                            if !verified.cleaned && let Ok(path) = item.temporary_path() { ui.label(format!("{}：{path}", if verified.temporary_owned { "已确认暂存文件" } else { "计划暂存文件（创建未确认）" })); }
                            if let Some(publication) = &verified.publication {
                                if publication.skipped_identical { ui.label("目标内容相同，已复用现有文件"); }
                                if let Some(previous) = publication.previous {
                                    ui.label(format!("原目标 {} B · SHA-256 {}", previous.size, previous.sha256.iter().map(|b| format!("{b:02x}")).collect::<String>()));
                                    if !publication.skipped_identical && item.draft.publication_policy == Some(cshell_application::FilePublicationPolicy::BackupReplace) && let Ok(path) = item.backup_path() {
                                        ui.add(egui::Label::new(format!("{}：{path}", if publication.backup_verified { "已校验并保留备份" } else { "计划备份（尚未确认）" })).wrap().selectable(true));
                                    }
                                }
                            }
                        }
                        if let Some(receipt) = &item.receipt {
                            if let Some(detail) = &receipt.detail { ui.label(detail); }
                            ui.label(format!("{} 字节 · 目标确认提交 {}", receipt.bytes, receipt.destination_committed));
                            if let Some(path) = &receipt.cleanup_path { ui.label(format!("保留临时文件（{}）：{path}", if receipt.cleanup_remote { "远端" } else { "daemon本机" })); }
                        }
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_enabled(ready&&self.start_queue_confirm&&queue.paused&&item.state==SftpQueueState::Pending&&item.draft.verified&&!queue.items.iter().any(|i|i.state==SftpQueueState::Running),egui::Button::new("只启动此条")).clicked(){*command=Some(SftpClientCommand::QueueControl(session,SftpOperation::QueueStartOne,queue.revision,item.id.as_uuid().as_bytes().to_vec()));self.start_queue_confirm=false;}
                            let recover = ready && self.recover_queue_confirm && queue.paused && !queue.items.iter().any(|i| i.state == SftpQueueState::Running);
                            for (allowed, operation, label) in [(item.can_resume_verified(), SftpOperation::QueueResumeVerified, "核对前缀后续传"), (item.can_adopt_verified(), SftpOperation::QueueAdoptVerified, "探测并认领匹配目标"), (item.can_cleanup_verified(), SftpOperation::QueueCleanupVerified, "核对并清理暂存文件")] {
                                if ui.add_enabled(recover && allowed, egui::Button::new(label)).clicked() { *command = Some(SftpClientCommand::QueueControl(session, operation, queue.revision, item.id.as_uuid().as_bytes().to_vec())); self.recover_queue_confirm = false; }
                            }
                            if ui.add_enabled(ready && self.edit_queue_confirm && item.state == SftpQueueState::Pending, egui::Button::new("取消待派发")).clicked() {
                                *command = Some(SftpClientCommand::QueueControl(session, SftpOperation::QueueCancelPending, queue.revision, item.id.as_uuid().as_bytes().to_vec())); self.edit_queue_confirm = false;
                            }
                            if ui.add_enabled(ready && self.edit_queue_confirm && item.state != SftpQueueState::Running, egui::Button::new("移除元数据")).clicked() {
                                *command = Some(SftpClientCommand::QueueControl(session, SftpOperation::QueueRemove, queue.revision, item.id.as_uuid().as_bytes().to_vec())); self.edit_queue_confirm = false;
                            }
                            if let Some(id) = item.transfer_id && item.state == SftpQueueState::Running && ui.button("请求取消当前传输").clicked() {
                                *command = Some(SftpClientCommand::Cancel(session, id.as_uuid().as_bytes().to_vec()));
                            }
                        });
                    });
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cshell_ipc::SftpTransfer;

    #[test]
    fn cleanup_is_visible_only_for_source_session_and_never_replayed_on_draw() {
        let id = SessionId::new();
        let marker = "/PRIVATE_RECOVERY_SENTINEL.tmp";
        let view = SftpView {
            session_id: Some(id),
            status: "目标已提交，清理失败".into(),
            transfer: Some(SftpTransfer {
                state: SftpTransferState::Failed as i32,
                cleanup_path: Some(marker.into()),
                destination_committed: true,
                cleanup_location: SftpCleanupLocation::Remote as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut panel = SftpPanel {
            open: true,
            ..Default::default()
        };
        let mut render = |session| {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 760.0),
                    )),
                    ..Default::default()
                },
                |ui| assert!(panel.draw(ui.ctx(), Some(session), Some(&view)).is_none()),
            );
            output.textures_delta.clear();
            output
                .platform_output
                .accesskit_update
                .unwrap_or_else(|| panic!("accessible SFTP window"))
                .nodes
                .into_iter()
                .flat_map(|(_, node)| {
                    [
                        node.label().map(str::to_owned),
                        node.value().map(str::to_owned),
                    ]
                    .into_iter()
                    .flatten()
                })
                .collect::<Vec<_>>()
        };
        let _ = render(id);
        let labels = render(id);
        assert!(
            labels.iter().any(|label| label == marker),
            "rendered text: {labels:?}"
        );
        assert!(labels.iter().any(|label| label.contains("不要重复传输")));
        assert!(labels.iter().any(|label| label == "远程临时文件"));
        let labels = render(SessionId::new());
        assert!(
            !labels
                .iter()
                .any(|label| label.contains(marker) || label.contains("不要重复传输"))
        );
        panel.remote_file = marker.into();
        panel.local_file = marker.into();
        assert!(
            !format!(
                "{panel:?} {view:?} {:?}",
                SftpClientCommand::Upload(id, marker.into(), marker.into())
            )
            .contains(marker)
        );
    }
}
