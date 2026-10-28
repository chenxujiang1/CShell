mod editor_review;
use super::*;
use crate::sftp_connection::WorkspaceCommand;
use cshell_application::*;
mod editor_workspace;
pub(super) struct AdvancedSftp {
    review:editor_review::ReviewPanel,
    documents:editor_workspace::Documents,
    pub follow_links: bool,
    pub policy: Option<FilePublicationPolicy>,
    pub rate: String,
    pub exclusive: bool,
    concurrency: u32,
    pending: Option<(cshell_domain::SftpQueueItemId, u64, SftpQueueDraft)>,
    pending_confirm: bool,
    settings_confirm: bool,
    file_local: bool,
    file_action: SftpFileAction,
    file_path: String,
    file_target: String,
    mode: String,
    apply_confirm: bool,
    file_stamp: Option<(SessionId, u64)>,
    old_session: String,
    migration_confirm: bool,
    migration_stamp: Option<(SessionId, u64, u64)>,
    editor_executable: String,
    editor_arguments: String,
    editor_confirm: bool,
    edit_buffer: String,
    edit_dirty: bool,
    edit_stamp: Option<(SessionId, u64, SftpVerifiedContent, SftpVerifiedContent)>,
    edit_control_stamp: Option<(SessionId, u64)>,
    replace_draft_confirm: bool,
    save_confirm: bool,
    rebind_confirm: bool,
    close_confirm: bool,
    history_all: bool,
    forget_confirm: bool,
    discard_private: bool,
    editor_target: Option<String>,
    editor_notice: Option<String>,
    save_pending: Option<(SessionId,u64,u64)>,
    discard_draft_confirm: bool,
}
impl Default for AdvancedSftp {
    fn default() -> Self {
        Self {
            review:Default::default(),
            documents:Default::default(),
            follow_links: false,
            policy: None,
            rate: String::new(),
            exclusive: false,
            concurrency: 3,
            pending: None,
            pending_confirm: false,
            settings_confirm: false,
            file_local: false,
            file_action: SftpFileAction::Inspect,
            file_path: String::new(),
            file_target: String::new(),
            mode: "755".into(),
            apply_confirm: false,
            file_stamp: None,
            old_session: String::new(),
            migration_confirm: false,
            migration_stamp: None,
            editor_executable: String::new(),
            editor_arguments: String::new(),
            editor_confirm: false,
            edit_buffer: String::new(),
            edit_dirty: false,
            edit_stamp: None,
            edit_control_stamp: None,
            replace_draft_confirm: false,
            save_confirm: false,
            rebind_confirm: false,
            close_confirm: false,
            history_all: false,
            forget_confirm: false,
            discard_private: false,
            editor_target: None,
            editor_notice: None,
            save_pending: None,
            discard_draft_confirm: false,
        }
    }
}
pub(super) fn workspace(
    scope: SessionId,
    wire: Option<SessionId>,
    operation: SftpOperation,
    data: Vec<u8>,
    revision: u64,
    object: Option<SessionId>,
) -> SftpClientCommand {
    SftpClientCommand::Workspace(
        scope,
        WorkspaceCommand {
            operation,
            data,
            revision,
            object: object
                .map(|i| i.as_uuid().as_bytes().to_vec())
                .unwrap_or_default(),
            source_session: wire,
            epoch: 0,
        },
    )
}
fn parse_id(value: &str) -> Option<SessionId> {
    if value.len() != 36 {
        return None;
    }
    serde_json::from_value::<SessionId>(serde_json::Value::String(value.to_owned()))
        .ok()
        .filter(|i| !i.as_uuid().is_nil())
}
fn hex(content: SftpVerifiedContent) -> String {
    content.sha256.iter().map(|b| format!("{b:02x}")).collect()
}
impl AdvancedSftp {
    pub(super) fn editor_request_rejected(&mut self){self.save_pending=None;self.editor_target=None;self.editor_notice=Some("请求未提交，修改仍保留。请检查连接后重试。".into());}
    pub(super) fn prepare_editor(&mut self,path:&str,session:SessionId)->bool {
        if self.documents.entries.len()>=8 && !self.documents.entries.values().any(|d|d.path==path&&d.session==session) {self.editor_notice=Some("最多同时编辑 8 个文件，请先关闭不再使用的文件。".into());return false;}
        if self.edit_buffer.len()>MAX_SFTP_INLINE_EDIT_BYTES {self.editor_notice=Some("当前草稿超过内置编辑上限，请先缩减或保存到外部编辑器。".into());return false;}
        self.remember_document();
        self.editor_target=Some(path.into());self.editor_notice=None;true
    }
    pub(super) fn existing_document(&self,path:&str,session:SessionId)->Option<SessionId>{self.documents.entries.iter().find(|(_,d)|d.path==path&&d.session==session).map(|(id,_)|*id)}
    pub(super) fn reset(&mut self) {
        self.keep_unsaved_documents();
        let executable = std::mem::take(&mut self.editor_executable);
        let arguments = std::mem::take(&mut self.editor_arguments);
        let documents=std::mem::take(&mut self.documents);
        let draft = self
            .edit_dirty
            .then(|| (std::mem::take(&mut self.edit_buffer), self.edit_stamp));
        *self = Self::default();
        self.editor_executable = executable;
        self.editor_arguments = arguments;
        self.documents=documents;
        if let Some((text, stamp)) = draft {
            self.edit_buffer = text;
            self.edit_stamp = stamp;
            self.edit_dirty = true;
        }
    }
    pub(super) fn draft(
        &self,
        direction: SftpQueueDirection,
        local: String,
        remote: String,
        verification: cshell_domain::SftpVerification,
    ) -> Result<SftpQueueDraft, &'static str> {
        let rate_limit = if self.rate.trim().is_empty() {
            None
        } else {
            Some(
                self.rate
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "速率请输入正整数 KiB/s")?
                    .checked_mul(1024)
                    .ok_or("速率超限")?,
            )
        };
        if self.policy == Some(FilePublicationPolicy::BackupReplace) && !self.exclusive {
            return Err("请确认独占修改目标并保留原文件备份");
        }
        let draft = SftpQueueDraft {
            verification: Some(if self.policy.is_some(){cshell_domain::SftpVerification::Full}else{verification}),
            verified: true,
            direction,
            local_path: local,
            remote_path: remote,
            publication_policy: self.policy,
            follow_links: self.follow_links,
            rate_limit,
            ..Default::default()
        };
        draft.validate()?;
        Ok(draft)
    }
    pub(super) fn controls(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: Option<&SftpView>,
        command: &mut Option<SftpClientCommand>,
    ) -> bool {
        let mut changed = false;
        ui.collapsing("路径、冲突和传输设置", |ui| {
            ui.weak("校验策略在 工具 → 全局默认设置 中选择，SSH 配置可单独覆盖。替换 / 跳过相同内容会执行严格内容核对。");
            changed |= ui
                .checkbox(
                    &mut self.follow_links,
                    "允许解析浏览路径及源文件的软链接（目标软链接仍拒绝替换）",
                )
                .changed();
            let previous = self.policy;
            egui::ComboBox::from_id_salt("sftp-publication-policy")
                .truncate()
                .selected_text(match self.policy {
                    None => "拒绝已有目标",
                    Some(FilePublicationPolicy::SkipIdentical) => "跳过相同内容",
                    Some(FilePublicationPolicy::BackupReplace) => "保留备份后替换",
                })
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut self.policy, None, "拒绝已有目标");
                    ui.selectable_value(
                        &mut self.policy,
                        Some(FilePublicationPolicy::SkipIdentical),
                        "跳过相同内容",
                    );
                    ui.selectable_value(
                        &mut self.policy,
                        Some(FilePublicationPolicy::BackupReplace),
                        "保留备份后替换",
                    );
                });
            if previous != self.policy {
                self.exclusive = false;
                changed = true;
            }
            if self.policy == Some(FilePublicationPolicy::BackupReplace) {
                changed |= ui
                    .checkbox(
                        &mut self.exclusive,
                        "确认目标由我独占修改；原文件作为独立备份保留",
                    )
                    .changed();
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("每条传输速率上限 KiB/s（空为不限）");
                changed |= ui.text_edit_singleline(&mut self.rate).changed();
            });
            if let Some(view) = view
                && let Some(queue) = &view.queue
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("当前并发 {}", queue.concurrency.unwrap_or(3)));
                    ui.add(egui::Slider::new(&mut self.concurrency, 1..=4).text("新的并发"));
                });
                ui.checkbox(
                    &mut self.settings_confirm,
                    "确认调整暂停队列的并发；并行项须为不同目标的校验传输",
                );
                if ui
                    .add_enabled(
                        self.settings_confirm
                            && queue.paused
                            && queue.migrated_to.is_none()
                            && !queue
                                .items
                                .iter()
                                .any(|i| i.state == SftpQueueState::Running)
                            && view.queue_writable
                            && !view.queue_busy,
                        egui::Button::new("保存队列设置"),
                    )
                    .clicked()
                {
                    *command = Some(SftpClientCommand::QueueWrite(
                        session,
                        SftpOperation::QueueConfigure,
                        serde_json::to_vec(&SftpQueueSettings {
                            concurrency: self.concurrency,
                        })
                        .unwrap_or_default(),
                        queue.revision,
                        Vec::new(),
                    ));
                    self.settings_confirm = false;
                }
            }
        });
        changed
    }
    pub(super) fn pending_edit(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let Some(queue) = &view.queue else {
            return;
        };
        ui.collapsing("调整待派发条目的路径与冲突策略", |ui| {
            for row in queue
                .items
                .iter()
                .filter(|i| i.state == SftpQueueState::Pending)
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{} ↔ {}",
                        row.draft.local_path, row.draft.remote_path
                    ));
                    if ui
                        .add_enabled(
                            row.draft.verified
                                && row.draft.expected_source.is_none()
                                && row.draft.expected_destination.is_none(),
                            egui::Button::new("编辑此条"),
                        )
                        .clicked()
                    {
                        self.pending = Some((row.id, queue.revision, row.draft.clone()));
                        self.pending_confirm = false;
                    }
                });
            }
            let Some((id, revision, draft)) = &mut self.pending else {
                return;
            };
            if *revision != queue.revision {
                self.pending_confirm = false;
                ui.weak("队列已变化；重新选择此条进行编辑。");
                return;
            }
            ui.label("本机完整路径");
            if ui.text_edit_singleline(&mut draft.local_path).changed() {
                self.pending_confirm = false;
            }
            ui.label("远端完整路径");
            if ui.text_edit_singleline(&mut draft.remote_path).changed() {
                self.pending_confirm = false;
            }
            egui::ComboBox::from_id_salt("sftp-pending-policy")
                .truncate()
                .selected_text(match draft.publication_policy {
                    None => "拒绝已有目标",
                    Some(FilePublicationPolicy::SkipIdentical) => "跳过相同内容",
                    Some(FilePublicationPolicy::BackupReplace) => "保留备份后替换",
                })
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    for (policy, label) in [
                        (None, "拒绝已有目标"),
                        (Some(FilePublicationPolicy::SkipIdentical), "跳过相同内容"),
                        (Some(FilePublicationPolicy::BackupReplace), "保留备份后替换"),
                    ] {
                        if ui
                            .selectable_value(&mut draft.publication_policy, policy, label)
                            .changed()
                        {
                            self.pending_confirm = false;
                        }
                    }
                });
            if ui
                .checkbox(&mut draft.follow_links, "读取源时允许解析软链接")
                .changed()
            {
                self.pending_confirm = false;
            }
            ui.checkbox(
                &mut self.pending_confirm,
                "确认更新此待派发条目；选择替换时确认独占目标并保留原文件备份",
            );
            if ui
                .add_enabled(
                    self.pending_confirm
                        && queue.paused
                        && queue.migrated_to.is_none()
                        && view.queue_writable
                        && !view.queue_busy
                        && !queue
                            .items
                            .iter()
                            .any(|i| i.state == SftpQueueState::Running)
                        && draft.validate().is_ok(),
                    egui::Button::new("保存此条，不启动传输"),
                )
                .clicked()
            {
                *command = Some(SftpClientCommand::QueueWrite(
                    session,
                    SftpOperation::QueueEdit,
                    serde_json::to_vec(draft).unwrap_or_default(),
                    *revision,
                    id.as_uuid().as_bytes().to_vec(),
                ));
                self.pending_confirm = false;
            }
        });
    }
    pub(super) fn file_management(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        local_path: &str,
        remote_path: &str,
        command: &mut Option<SftpClientCommand>,
    ) {
        ui.collapsing("文件管理", |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.selectable_label(self.file_local, "本机").clicked() {
                    self.file_local = true;
                    self.file_path = local_path.into();
                    self.apply_confirm = false;
                }
                if ui.selectable_label(!self.file_local, "远端").clicked() {
                    self.file_local = false;
                    self.file_path = remote_path.into();
                    self.apply_confirm = false;
                }
                if ui.button("使用当前选择").clicked() {
                    self.file_path = if self.file_local {
                        local_path
                    } else {
                        remote_path
                    }
                    .into();
                    self.apply_confirm = false;
                }
            });
            egui::ComboBox::from_id_salt("sftp-file-action")
                .truncate()
                .selected_text(format!("{:?}", self.file_action))
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    for (action, label) in [
                        (SftpFileAction::Inspect, "属性与链接目标"),
                        (SftpFileAction::Mkdir, "新建目录"),
                        (SftpFileAction::Rename, "重命名／移动"),
                        (SftpFileAction::Delete, "删除文件、链接或空目录"),
                        (SftpFileAction::Chmod, "修改 POSIX 权限"),
                        (SftpFileAction::Symlink, "新建软链接"),
                    ] {
                        if ui
                            .selectable_value(&mut self.file_action, action, label)
                            .changed()
                        {
                            self.apply_confirm = false;
                        }
                    }
                });
            ui.label("完整路径");
            if ui.text_edit_singleline(&mut self.file_path).changed() {
                self.apply_confirm = false;
            }
            if matches!(
                self.file_action,
                SftpFileAction::Rename | SftpFileAction::Symlink
            ) {
                ui.label("目标完整路径");
                if ui.text_edit_singleline(&mut self.file_target).changed() {
                    self.apply_confirm = false;
                }
            }
            if matches!(
                self.file_action,
                SftpFileAction::Mkdir | SftpFileAction::Chmod
            ) {
                ui.label("权限（八进制，如 755）");
                ui.text_edit_singleline(&mut self.mode);
            }
            let draft = SftpFileDraft {
                local: self.file_local,
                action: self.file_action,
                path: self.file_path.clone(),
                target: if matches!(
                    self.file_action,
                    SftpFileAction::Rename | SftpFileAction::Symlink
                ) {
                    self.file_target.clone()
                } else {
                    String::new()
                },
                permissions: if matches!(
                    self.file_action,
                    SftpFileAction::Mkdir | SftpFileAction::Chmod
                ) {
                    u32::from_str_radix(self.mode.trim(), 8).ok()
                } else {
                    None
                },
            };
            if ui
                .add_enabled(
                    !view.workspace_busy && draft.validate().is_ok(),
                    egui::Button::new("检查并生成操作预览"),
                )
                .clicked()
            {
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::FileInspect,
                    serde_json::to_vec(&draft).unwrap_or_default(),
                    0,
                    None,
                ));
                self.apply_confirm = false;
            }
            if let Some(job) = &view.file_job {
                if self.file_stamp != Some((job.id, job.revision)) {
                    self.file_stamp = Some((job.id, job.revision));
                    self.apply_confirm = false;
                    self.forget_confirm = false;
                }
                ui.separator();
                ui.label(format!(
                    "{:?} · {:?} · {}",
                    job.draft.action, job.state, job.draft.path
                ));
                if !job.draft.target.is_empty() {
                    ui.label(format!("→ {}", job.draft.target));
                }
                ui.label(&job.detail);
                if let Some(stamp) = &job.source {
                    ui.label(format!(
                        "类型 {} · 大小 {:?} · 权限 {:?}",
                        stamp.kind,
                        stamp.size,
                        stamp.permissions.map(|p| format!("{p:o}"))
                    ));
                    if let Some(content) = stamp.content {
                        ui.monospace(hex(content));
                    }
                    if let Some(link) = &stamp.link_target {
                        ui.label(format!("软链接 → {link}"));
                    }
                }
                ui.checkbox(
                    &mut self.apply_confirm,
                    "确认执行上方已检查的操作；删除不可撤销，不递归删除目录",
                );
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            self.apply_confirm
                                && view.file_writable
                                && !view.workspace_busy
                                && job.session == session
                                && job.state == SftpFileState::Ready
                                && job.draft.action != SftpFileAction::Inspect,
                            egui::Button::new("执行此操作"),
                        )
                        .clicked()
                    {
                        *command = Some(workspace(
                            session,
                            None,
                            SftpOperation::FileApply,
                            Vec::new(),
                            job.revision,
                            Some(job.id),
                        ));
                        self.apply_confirm = false;
                    }
                    if ui.button("重新载入操作记录").clicked() {
                        *command = Some(workspace(
                            session,
                            Some(job.session),
                            SftpOperation::FileJobGet,
                            Vec::new(),
                            0,
                            Some(job.id),
                        ));
                    }
                    if ui
                        .add_enabled(
                            view.file_writable
                                && !view.workspace_busy
                                && matches!(
                                    job.state,
                                    SftpFileState::Inspecting | SftpFileState::Applying
                                ),
                            egui::Button::new("请求取消"),
                        )
                        .clicked()
                    {
                        *command = Some(workspace(
                            session,
                            Some(job.session),
                            SftpOperation::FileJobCancel,
                            Vec::new(),
                            job.revision,
                            Some(job.id),
                        ));
                    }
                });
                ui.checkbox(
                    &mut self.forget_confirm,
                    "确认移除已结束的操作记录（不删除目标文件）",
                );
                if ui
                    .add_enabled(
                        self.forget_confirm
                            && !view.workspace_busy
                            && !matches!(
                                job.state,
                                SftpFileState::Inspecting | SftpFileState::Applying
                            ),
                        egui::Button::new("移除操作记录"),
                    )
                    .clicked()
                {
                    *command = Some(workspace(
                        session,
                        Some(job.session),
                        SftpOperation::WorkspaceForget,
                        serde_json::to_vec(&SftpWorkspaceForget {
                            editor: false,
                            discard_private_files: false,
                        })
                        .unwrap_or_default(),
                        job.revision,
                        Some(job.id),
                    ));
                    self.forget_confirm = false;
                }
            }
        });
    }
    pub(super) fn migration(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        ui.collapsing("迁移旧会话队列", |ui| {
            if let Some(peer) = &view.peer {
                ui.label(format!(
                    "当前 {}:{} · {}",
                    peer.host.host, peer.host.port, peer.username
                ));
                ui.monospace(&peer.host_key_sha256);
            }
            if ui.button("读取当前已验证的主机身份").clicked() {
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::PeerGet,
                    Vec::new(),
                    0,
                    None,
                ));
            }
            ui.label("原会话 UUID");
            if ui.text_edit_singleline(&mut self.old_session).changed() {
                self.migration_confirm = false;
            }
            if let Some(old) = parse_id(&self.old_session)
                && old != session
                && ui.button("载入原会话队列预览").clicked()
            {
                *command = Some(workspace(
                    session,
                    Some(old),
                    SftpOperation::QueueGet,
                    Vec::new(),
                    0,
                    None,
                ));
            }
            if let (Some(source), Some(target)) = (&view.migration_source, &view.queue) {
                if self.migration_stamp
                    != Some((source.session_id, source.revision, target.revision))
                {
                    self.migration_stamp =
                        Some((source.session_id, source.revision, target.revision));
                    self.migration_confirm = false;
                }
                ui.label(format!(
                    "原队列 {} · {} 条 · {}",
                    source.session_id,
                    source.items.len(),
                    if source.paused { "暂停" } else { "运行中" }
                ));
                if let Some(peer) = &source.peer {
                    ui.label(format!(
                        "原 {}:{} · {}",
                        peer.host.host, peer.host.port, peer.username
                    ));
                    ui.monospace(&peer.host_key_sha256);
                } else {
                    ui.weak("旧记录没有保存身份；只有原连接仍在时才能由 daemon 核对。");
                }
                egui::ScrollArea::vertical()
                    .id_salt("sftp-migration-preview")
                    .max_height(80.0)
                    .show(ui, |ui| {
                        for item in &source.items {
                            ui.label(format!(
                                "{:?} · {} ↔ {}",
                                item.state, item.draft.local_path, item.draft.remote_path
                            ));
                        }
                    });
                ui.checkbox(
                    &mut self.migration_confirm,
                    "确认相同主机、账号、主机密钥及跳板路由；锁住原队列并迁入当前空队列，保持暂停",
                );
                if let Some(peer) = &view.peer
                    && ui
                        .add_enabled(
                            self.migration_confirm
                                && source.paused
                                && !source
                                    .items
                                    .iter()
                                    .any(|i| i.state == SftpQueueState::Running)
                                && target.paused
                                && target.items.is_empty()
                                && view.queue_writable
                                && !view.workspace_busy
                                && source.peer.as_ref().is_none_or(|p| p == peer),
                            egui::Button::new("确认迁移／完成先前的迁移"),
                        )
                        .clicked()
                {
                    *command = Some(workspace(
                        session,
                        Some(source.session_id),
                        SftpOperation::QueueMigrate,
                        serde_json::to_vec(&SftpQueueMigration {
                            target: session,
                            target_revision: target.revision,
                            peer: peer.clone(),
                        })
                        .unwrap_or_default(),
                        source.revision,
                        None,
                    ));
                    self.migration_confirm = false;
                }
                if !source.paused {
                    ui.weak("请在原会话中暂停队列，等待在途传输完成，再重新载入预览。");
                }
            }
        });
    }
    pub(super) fn editor(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        remote: &str,
        command: &mut Option<SftpClientCommand>,
    ) {
        egui::Frame::NONE.show(ui, |ui| {
            ui.heading("远程编辑");
            if let Some(notice)=&self.editor_notice {ui.colored_label(cshell_ui::theme::WARNING,notice);}
            if let Some(target)=&self.editor_target {
                if view.edit.as_ref().is_some_and(|e|e.remote_path==*target && e.session==session) {self.editor_target=None;}
                else if view.error.is_some() && !view.workspace_busy {self.editor_target=None;}
                else {ui.horizontal(|ui|{ui.spinner();ui.label(format!("正在打开 {target}…"));});return;}
            }
            if (view.edit.is_none() || view.edit.as_ref().is_some_and(|e|e.closed)) && ui
                .add_enabled(
                    !view.workspace_busy && !self.edit_dirty && sftp_remote_path(remote, false),
                    egui::Button::new("编辑所选文件"),
                )
                .clicked()
            {
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::EditOpen,
                    serde_json::to_vec(&SftpBrowseOptions {
                        local: false,
                        path: remote.into(),
                        links: if self.follow_links {
                            SftpLinkPolicy::Follow
                        } else {
                            SftpLinkPolicy::NoFollow
                        },
                    })
                    .unwrap_or_default(),
                    0,
                    None,
                ));
            }
            let Some(edit) = &view.edit else {
                return;
            };
            self.activate_document(edit,view);
            if self.document_tabs(ui,session,view,command) {return;}
            if self.save_pending.is_some_and(|(id,revision,generation)|id!=edit.id || revision!=edit.revision || (view.generation!=generation && !view.workspace_busy && view.error.is_some())) {self.save_pending=None;}
            if self.edit_control_stamp != Some((edit.id, edit.revision)) {
                self.edit_control_stamp = Some((edit.id, edit.revision));
                self.save_confirm = false;
                self.editor_confirm = false;
                self.close_confirm = false;
                self.rebind_confirm = false;
                self.replace_draft_confirm = false;
                self.forget_confirm = false;
                self.discard_private = false;
                self.discard_draft_confirm = false;
            }
            if edit.state == SftpEditState::Preparing
                && edit.session == session
                && let Some(native) = edit.native_transfer
                && ui.button("请求取消工作副本下载").clicked()
            {
                *command = Some(SftpClientCommand::Cancel(
                    edit.session,
                    native.as_uuid().as_bytes().to_vec(),
                ));
            }
            ui.add(egui::Label::new(egui::RichText::new(&edit.remote_path).strong()).truncate()).on_hover_text(&edit.remote_path);
            ui.horizontal_wrapped(|ui| {
                ui.label(match edit.state {SftpEditState::Preparing=>"正在读取文件",SftpEditState::Ready if self.save_pending.is_some()=>"正在保存修改",SftpEditState::Ready if edit.queued_item.is_some()=>"正在上传修改",SftpEditState::Ready=>"可编辑",SftpEditState::Unknown=>"状态待核对",SftpEditState::Closed=>"编辑已关闭"});
                ui.weak(super::browser::bytes(edit.current.size));
                if self.edit_dirty {ui.colored_label(cshell_ui::theme::WARNING,"有未保存的修改");}
                else if edit.current!=edit.base {ui.colored_label(cshell_ui::theme::WARNING,"工作副本有修改");}
            });
            ui.collapsing("文件与校验详情",|ui| {
                ui.label(format!("工作副本：{}", edit.local_path));ui.label(&edit.detail);
                ui.monospace(format!("远端基础 SHA-256 {}", hex(edit.base)));
            });
            self.editor_review(ui,session,view,edit,command);
            let stamp = (edit.id, edit.revision, edit.current, edit.base);
            if self.edit_stamp.as_ref().is_none_or(|s| s.0 != edit.id) {
                if !self.edit_dirty {
                    self.edit_buffer = view.edit_text.as_deref().cloned().unwrap_or_default();
                    self.edit_stamp = Some(stamp);
                }
                self.save_confirm = false;
                self.editor_confirm = false;
                self.close_confirm = false;
                self.rebind_confirm = false;
            } else if self
                .edit_stamp
                .as_ref()
                .is_some_and(|s| s.2 == edit.current && s.3 == edit.base)
            {
                self.edit_stamp = Some(stamp);
            }
            if let Some(text) = &view.edit_text {
                if !self.edit_dirty && self.edit_stamp != Some(stamp) {
                    self.edit_buffer = (**text).clone();
                    self.edit_stamp = Some(stamp);
                }
                if self.edit_dirty
                    && sftp_digest(self.edit_buffer.as_bytes()) == edit.current
                    && self.edit_stamp.as_ref().is_some_and(|s| s.0 == edit.id)
                {
                    self.edit_dirty = false;
                    self.edit_stamp = Some(stamp);
                }
                if self.editor_text(ui,edit) {
                    self.edit_dirty = true;
                    self.discard_draft_confirm = false;
                    self.save_confirm = false;
                }
                if self.edit_stamp != Some(stamp) {
                    ui.weak("工作副本或远端基础已变化；当前草稿仍保留。");
                    ui.checkbox(
                        &mut self.replace_draft_confirm,
                        "确认选择如何处理当前草稿与新版本",
                    );
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                self.replace_draft_confirm,
                                egui::Button::new("重新载入工作副本并替换草稿"),
                            )
                            .clicked()
                        {
                            self.edit_buffer = (**text).clone();
                            self.edit_dirty = false;
                            self.edit_stamp = Some(stamp);
                            self.replace_draft_confirm = false;
                            self.save_confirm = false;
                        }
                        if ui
                            .add_enabled(
                                self.replace_draft_confirm
                                    && self.edit_buffer.len() <= MAX_SFTP_INLINE_EDIT_BYTES,
                                egui::Button::new("保留草稿，以当前基础版本重新核对保存"),
                            )
                            .clicked()
                        {
                            self.edit_stamp = Some(stamp);
                            self.replace_draft_confirm = false;
                            self.save_confirm = false;
                        }
                    });
                }
            } else {
                ui.weak("大文件或非 UTF-8 内容请使用外部编辑器；工作文件上限 8 MiB。");
            }
            ui.collapsing("外部编辑器",|ui| {
            self.external_editor_tools(ui);
            ui.label("外部编辑器完整可执行路径");
            if ui
                .text_edit_singleline(&mut self.editor_executable)
                .changed()
            {
                self.editor_confirm = false;
            }
            ui.label("参数 JSON 数组；{file} 替换为工作文件，未指定则追加文件路径");
            if ui
                .text_edit_singleline(&mut self.editor_arguments)
                .changed()
            {
                self.editor_confirm = false;
            }
            let arguments = if self.editor_arguments.trim().is_empty() {
                Some(Vec::<String>::new())
            } else {
                serde_json::from_str::<Vec<String>>(&self.editor_arguments).ok()
            };
            ui.weak("外部修改会自动检测；回传仍由“保存并上传”明确提交。");
            if ui
                .add_enabled(
                    arguments.is_some()
                        && std::path::Path::new(&self.editor_executable).is_absolute()
                        && view.edit_writable
                        && !view.workspace_busy
                        && edit.session == session
                        && edit.state == SftpEditState::Ready
                        && !edit.closed,
                    egui::Button::new("用外部编辑器打开"),
                )
                .clicked()
            {
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::EditLaunch,
                    serde_json::to_vec(&SftpEditorLaunch {
                        executable: self.editor_executable.clone(),
                        arguments: arguments.unwrap_or_default(),
                    })
                    .unwrap_or_default(),
                    edit.revision,
                    Some(edit.id),
                ));
                self.editor_confirm = false;
            }
            });
            if self.edit_buffer.len() > MAX_SFTP_INLINE_EDIT_BYTES {
                ui.colored_label(
                    cshell_ui::theme::DANGER,
                    "内置编辑草稿超过 256 KiB，请缩减或复制到外部编辑器。",
                );
            }
            if !self.documents.external_footer {self.editor_save_controls(ui,session,view,command);}
            ui.collapsing("恢复与记录管理",|ui| {
            if self.edit_dirty {
                ui.checkbox(&mut self.discard_draft_confirm,"确认放弃未保存的编辑草稿");
                if ui.add_enabled(self.discard_draft_confirm && view.edit_text.is_some(),egui::Button::new("放弃草稿，恢复工作副本")).clicked(){
                    self.edit_buffer=view.edit_text.as_deref().cloned().unwrap_or_default();self.edit_dirty=false;self.edit_stamp=Some(stamp);self.discard_draft_confirm=false;self.editor_notice=None;
                }
            }
            if ui.button("重新读取工作副本（保留草稿）").clicked() {
                *command = Some(workspace(
                    session,
                    Some(edit.session),
                    SftpOperation::EditGet,
                    Vec::new(),
                    0,
                    Some(edit.id),
                ));
            }
            if edit.session != session {
                ui.checkbox(
                    &mut self.rebind_confirm,
                    "确认将编辑记录绑定到当前相同的已验证主机；已有回传队列须先迁移",
                );
                if ui
                    .add_enabled(
                        self.rebind_confirm
                            && view.edit_writable
                            && !view.workspace_busy
                            && view.peer.as_ref() == Some(&edit.peer),
                        egui::Button::new("重新绑定编辑会话"),
                    )
                    .clicked()
                {
                    *command = Some(workspace(
                        session,
                        None,
                        SftpOperation::EditRebind,
                        Vec::new(),
                        edit.revision,
                        Some(edit.id),
                    ));
                    self.rebind_confirm = false;
                }
            }
            ui.checkbox(
                &mut self.close_confirm,
                "确认关闭此编辑记录，保留工作文件和未结束的回传",
            );
            if ui
                .add_enabled(
                    self.close_confirm
                        && view.edit_writable
                        && !view.workspace_busy
                        && edit.state != SftpEditState::Preparing,
                    egui::Button::new("关闭编辑记录"),
                )
                .clicked()
            {
                *command = Some(workspace(
                    session,
                    Some(edit.session),
                    SftpOperation::EditClose,
                    Vec::new(),
                    edit.revision,
                    Some(edit.id),
                ));
                self.close_confirm = false;
            }
            if edit.closed {
                ui.checkbox(&mut self.forget_confirm, "确认移除关闭的编辑记录");
                ui.checkbox(
                    &mut self.discard_private,
                    "同时永久删除此记录的私有工作副本及快照（未结束回传禁止删除）",
                );
                if ui
                    .add_enabled(
                        self.forget_confirm
                            && !view.workspace_busy
                            && (!self.discard_private || edit.queued_item.is_none()),
                        egui::Button::new("移除编辑记录与所选私有文件"),
                    )
                    .clicked()
                {
                    *command = Some(workspace(
                        session,
                        Some(edit.session),
                        SftpOperation::WorkspaceForget,
                        serde_json::to_vec(&SftpWorkspaceForget {
                            editor: true,
                            discard_private_files: self.discard_private,
                        })
                        .unwrap_or_default(),
                        edit.revision,
                        Some(edit.id),
                    ));
                    self.forget_confirm = false;
                    self.discard_private = false;
                }
            }
            });
        });
    }
    pub(super) fn history(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        ui.collapsing("操作与编辑历史", |ui| {
            ui.checkbox(&mut self.history_all, "包括旧会话记录");
            if ui.button("载入历史").clicked() {
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::WorkspaceList,
                    serde_json::to_vec(&SftpWorkspacePage {
                        offset: 0,
                        all_sessions: self.history_all,
                    })
                    .unwrap_or_default(),
                    0,
                    None,
                ));
            }
            egui::ScrollArea::vertical()
                .id_salt("sftp-workspace-history")
                .max_height(160.0)
                .show(ui, |ui| {
                    for item in &view.history {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!(
                                "{} · {} · {}",
                                if item.editor {
                                    "编辑"
                                } else {
                                    "文件操作"
                                },
                                item.state,
                                item.path
                            ));
                            if ui.button("载入").clicked() {
                                *command = Some(workspace(
                                    session,
                                    Some(item.session),
                                    if item.editor {
                                        SftpOperation::EditGet
                                    } else {
                                        SftpOperation::FileJobGet
                                    },
                                    Vec::new(),
                                    0,
                                    Some(item.id),
                                ));
                            }
                            if item.session != session && ui.button("预览此会话队列").clicked()
                            {
                                self.old_session = item.session.to_string();
                                *command = Some(workspace(
                                    session,
                                    Some(item.session),
                                    SftpOperation::QueueGet,
                                    Vec::new(),
                                    0,
                                    None,
                                ));
                            }
                        });
                    }
                });
            if view.history_more && ui.button("下一页历史").clicked() {
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::WorkspaceList,
                    serde_json::to_vec(&SftpWorkspacePage {
                        offset: view.history_offset.saturating_add(64),
                        all_sessions: self.history_all,
                    })
                    .unwrap_or_default(),
                    0,
                    None,
                ));
            }
        });
    }
}

#[cfg(test)]
#[path="editor_tests.rs"]
mod editor_tests;
