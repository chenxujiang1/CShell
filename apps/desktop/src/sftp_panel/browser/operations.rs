//! List-local file operations use the existing reviewed, versioned workspace protocol.
use super::*;
use cshell_application::{SftpFileAction, SftpFileDraft, SftpFileState};
use std::collections::VecDeque;

pub(super) struct Dialog {
    local: bool,
    action: SftpFileAction,
    directory: String,
    entries: Vec<cshell_ipc::SftpDirectoryEntry>,
    name: String,
    mode: String,
    focus: bool,
    run: Option<Run>,
    error: Option<String>,
}
struct Run {
    remaining: VecDeque<SftpFileDraft>,
    current: SftpFileDraft,
    before: Option<SessionId>,
    job: Option<SessionId>,
    applied_revision: Option<u64>,
    generation: u64,
    epoch: u64,
    completed: usize,
    total: usize,
}
impl Dialog {
    pub(super) fn rejected(&mut self) {
        self.error = Some("请求未提交，操作已停止。请关闭弹框后重新选择操作。".into());
    }
    fn drafts(&self) -> Result<Vec<SftpFileDraft>, &'static str> {
        if self.action == SftpFileAction::Inspect {
            return Err("当前仅查看属性");
        }
        let join = |name: &str| child_path(self.local, &self.directory, name);
        let mode = || {
            u32::from_str_radix(self.mode.trim(), 8)
                .ok()
                .filter(|p| *p <= 0o7777)
                .ok_or("请输入 0000–7777 的八进制权限")
        };
        let drafts = match self.action {
            SftpFileAction::Mkdir => vec![SftpFileDraft {
                local: self.local,
                action: self.action,
                path: join(&self.name)?,
                target: String::new(),
                permissions: Some(mode()?),
            }],
            SftpFileAction::Rename if self.entries.len() == 1 => vec![SftpFileDraft {
                local: self.local,
                action: self.action,
                path: self.entries[0].path.clone(),
                target: join(&self.name)?,
                permissions: None,
            }],
            SftpFileAction::Chmod if !self.entries.is_empty() && self.entries.len() <= 128 => {
                let permissions = Some(mode()?);
                self.entries
                    .iter()
                    .map(|entry| SftpFileDraft {
                        local: self.local,
                        action: self.action,
                        path: entry.path.clone(),
                        target: String::new(),
                        permissions,
                    })
                    .collect()
            }
            _ => return Err("请选择可操作的文件或目录"),
        };
        for draft in &drafts {
            draft
                .validate()
                .map_err(|_| "名称、路径或权限无效；重命名时请使用不同的名称")?;
        }
        Ok(drafts)
    }
}
fn child_path(local: bool, directory: &str, name: &str) -> Result<String, &'static str> {
    if name.is_empty()
        || name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || local
            && cfg!(windows)
            && (name.contains(['<', '>', ':', '"', '|', '?', '*']) || name.ends_with(['.', ' ']))
    {
        return Err("请输入一个有效名称，不包含路径分隔符或上级目录");
    }
    Ok(if local {
        std::path::Path::new(directory)
            .join(name)
            .to_string_lossy()
            .into_owned()
    } else {
        format!("{}/{}", directory.trim_end_matches('/'), name)
    })
}
fn file_entries(view: &SftpView, local: bool) -> &[cshell_ipc::SftpDirectoryEntry] {
    if local {
        &view.local_entries
    } else {
        &view.entries
    }
}
impl SftpPanel {
    pub(super) fn open_remote_editor(&mut self, session: SessionId, view: &SftpView, path: &str) -> Option<SftpClientCommand> {
        if view.workspace_busy || self.browser.operation.is_some()
            || (view.connection_profile.is_some() && view.connection_phase != crate::sftp_connection::ConnectionPhase::Connected)
            || !view.entries.iter().any(|e|e.path==path && e.kind==1) {return None;}
        self.page=2;
        if view.edit.as_ref().is_some_and(|e|e.session==session && e.remote_path==path && !e.closed) {return None;}
        if !self.advanced.prepare_editor(path,session) {return None;}
        self.remote_file=path.into();
        if let Some(id)=self.advanced.existing_document(path,session){return Some(super::super::advanced::workspace(session,None,SftpOperation::EditGet,Vec::new(),0,Some(id)));}
        Some(super::super::advanced::workspace(session,None,SftpOperation::EditOpen,
            serde_json::to_vec(&cshell_application::SftpBrowseOptions{local:false,path:path.into(),links:if self.advanced.follow_links{cshell_application::SftpLinkPolicy::Follow}else{cshell_application::SftpLinkPolicy::NoFollow}}).unwrap_or_default(),0,None))
    }
    fn selected_entries(
        &self,
        view: &SftpView,
        local: bool,
    ) -> Vec<cshell_ipc::SftpDirectoryEntry> {
        let paths = &self.browser.selection[usize::from(!local)].paths;
        file_entries(view, local)
            .iter()
            .filter(|entry| paths.contains(&entry.path))
            .cloned()
            .collect()
    }
    pub(super) fn drag_selection(
        &self,
        session: SessionId,
        view: &SftpView,
        local: bool,
        entry: &cshell_ipc::SftpDirectoryEntry,
    ) -> Vec<DragFile> {
        let selected = self.selected_entries(view, local);
        let entries = if selected.iter().any(|e| e.path == entry.path) {
            selected
        } else {
            vec![entry.clone()]
        };
        if entries.len() > 128 || entries.iter().any(|entry| !matches!(entry.kind,1|2)) {
            return Vec::new();
        }
        entries
            .into_iter()
            .filter(|e| matches!(e.kind,1|2))
            .map(|e| DragFile {
                session,
                local,
                path: e.path,
                name: e.name,
            })
            .collect()
    }
    pub(super) fn transfer_selection(&mut self, session: SessionId, view: &SftpView, local: bool) {
        let entries = self.selected_entries(view, local);
        if entries.is_empty() || entries.len() > 128 || entries.iter().any(|e| !matches!(e.kind,1|2)) {
            self.browser.error =
                Some("请选择 1–128 个普通文件或文件夹。".into());
            return;
        }
        if entries.iter().any(|e|e.kind==2){self.stage_tree(session,view,local,entries.into_iter().map(|e|e.path).collect());return;}
        for e in entries {
            self.stage_file(
                session,
                view,
                &DragFile {
                    session,
                    local,
                    path: e.path,
                    name: e.name,
                },
                !local,
            );
        }
    }
    pub(super) fn open_file_dialog(
        &mut self,
        view: &SftpView,
        local: bool,
        action: SftpFileAction,
    ) {
        let Some(directory) = loaded_path(view, local) else {
            return;
        };
        let entries = self.selected_entries(view, local);
        if action != SftpFileAction::Mkdir && entries.is_empty() {
            return;
        }
        if action == SftpFileAction::Rename && entries.len() != 1 {
            return;
        }
        let mode = entries
            .first()
            .and_then(|e| e.permissions)
            .map(|p| p & 0o7777)
            .unwrap_or(if entries.first().is_some_and(|e| e.kind == 2) {
                0o755
            } else {
                0o644
            });
        self.browser.navigation.active = usize::from(!local);
        self.browser.operation = Some(Dialog {
            local,
            action,
            directory: directory.into(),
            name: if action == SftpFileAction::Rename {
                entries[0].name.clone()
            } else {
                String::new()
            },
            mode: if action != SftpFileAction::Mkdir
                && entries.first().is_some_and(|e| e.permissions.is_none())
            {
                String::new()
            } else {
                format!(
                    "{:04o}",
                    if action == SftpFileAction::Mkdir {
                        0o755
                    } else {
                        mode
                    }
                )
            },
            entries,
            focus: true,
            run: None,
            error: None,
        });
    }
    pub(super) fn file_menu(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        local: bool,
        command: &mut Option<SftpClientCommand>,
    ) {
        cshell_ui::form::menu(ui);
        ui.set_max_width(260.0);
        if local {self.default_directory_menu(ui,view);ui.separator();}
        let entries = self.selected_entries(view, local);
        let connected=view.connection_profile.is_none() || view.connection_phase==crate::sftp_connection::ConnectionPhase::Connected;
        let ready = connected && !view.workspace_busy
            && self.browser.operation.is_none()
            && !view.file_job.as_ref().is_some_and(|j| {
                matches!(j.state, SftpFileState::Inspecting | SftpFileState::Applying)
            });
        if !local && cshell_ui::form::menu_item(ui,"编辑文件    F4",ready && entries.len()==1 && entries[0].kind==1).clicked() {
            *command=self.open_remote_editor(session,view,&entries[0].path);ui.close();
        }
        if entries.len() == 1 && entries[0].kind == 2 && ui.button("打开目录").clicked() {
            *command = Some(self.navigate(session, local, entries[0].path.clone()));
            ui.close();
        }
        if ui
            .add_enabled(
                connected && !entries.is_empty() && entries.len() <= 128 && entries.iter().all(|e| matches!(e.kind,1|2)),
                egui::Button::new(if local {
                    "上传所选文件"
                } else {
                    "下载所选文件"
                }),
            )
            .clicked()
        {
            self.transfer_selection(session, view, local);
            ui.close();
        }
        ui.separator();
        for (action, label, enabled) in [
            (SftpFileAction::Rename, "重命名    F2", entries.len() == 1),
            (
                SftpFileAction::Mkdir,
                "新建目录    Ctrl+Shift+N",
                loaded_path(view, local).is_some(),
            ),
            (SftpFileAction::Inspect, "属性 / 权限", !entries.is_empty()),
        ] {
            if ui
                .add_enabled(ready && enabled, egui::Button::new(label).min_size(egui::vec2(ui.available_width(),28.0)).right_text(()))
                .clicked()
            {
                self.open_file_dialog(view, local, action);
                ui.close();
            }
        }
        if ui
            .add_enabled(!entries.is_empty(), egui::Button::new("复制完整路径"))
            .clicked()
        {
            ui.ctx().copy_text(
                entries
                    .iter()
                    .map(|e| e.path.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            ui.close();
        }
        if ui.button("刷新目录    F5").clicked() {
            if let Some(path) = loaded_path(view, local) {
                *command = Some(self.navigate(session, local, path.into()));
            }
            ui.close();
        }
    }
    pub(super) fn file_shortcuts(&mut self, ui: &egui::Ui, session: SessionId, view: &SftpView, command: &mut Option<SftpClientCommand>) {
        if view.connection_profile.is_some() && view.connection_phase!=crate::sftp_connection::ConnectionPhase::Connected {return;}
        let ctx = ui.ctx();
        if ctx.text_edit_focused()
            || egui::Popup::is_any_open(ctx)
            || self.browser.operation.is_some()
        {
            return;
        }
        let pressed = |modifiers, key| ctx.input_mut(|input| input.consume_key(modifiers, key));
        if pressed(egui::Modifiers::NONE,egui::Key::Tab) || pressed(egui::Modifiers::SHIFT,egui::Key::Tab) {
            self.browser.navigation.active ^= 1;
            ctx.memory_mut(|m|{if let Some(id)=m.focused(){m.surrender_focus(id);}});return;
        }
        let local = self.browser.navigation.active == 0;
        let side=usize::from(!local);
        let order=self.browser.sort[side].unwrap_or_else(||columns::SortState::initial(local)).sorted(file_entries(view,local));
        for (key,delta,absolute) in [(egui::Key::ArrowUp,-1,false),(egui::Key::ArrowDown,1,false),(egui::Key::Home,-1,true),(egui::Key::End,1,true),(egui::Key::PageUp,-10,false),(egui::Key::PageDown,10,false)] {
            for modifiers in [egui::Modifiers::SHIFT,egui::Modifiers::NONE] {
                if pressed(modifiers,key) {
                    if let Some(path)=self.browser.selection[side].move_cursor(&order,delta,absolute,modifiers) {if local{self.local_file=path;}else{self.remote_file=path;}}
                    return;
                }
            }
        }
        if pressed(egui::Modifiers::NONE,egui::Key::Space) {
            if let Some(path)=self.browser.selection[side].cursor.clone(){self.browser.selection[side].click(&path,&order,egui::Modifiers::CTRL);}return;
        }
        if pressed(egui::Modifiers::NONE,egui::Key::Enter) {
            if let Some(entry)=self.browser.selection[side].cursor.as_ref().and_then(|p|file_entries(view,local).iter().find(|e|&e.path==p)) {
                if entry.kind==2{*command=Some(self.navigate(session,local,entry.path.clone()));}
                else if !local && entry.kind==1{*command=self.open_remote_editor(session,view,&entry.path);}
            }return;
        }
        if pressed(egui::Modifiers::CTRL, egui::Key::A) {
            self.browser.selection[usize::from(!local)].paths = file_entries(view, local)
                .iter()
                .map(|e| e.path.clone())
                .collect();
        } else if !view.workspace_busy
            && !view.file_job.as_ref().is_some_and(|j| {
                matches!(j.state, SftpFileState::Inspecting | SftpFileState::Applying)
            })
        {
            if !local && pressed(egui::Modifiers::NONE,egui::Key::F4) {
                let entries=self.selected_entries(view,false);
                if entries.len()==1 { *command=self.open_remote_editor(session,view,&entries[0].path); }
            } else if pressed(egui::Modifiers::NONE, egui::Key::F2) {
                self.open_file_dialog(view, local, SftpFileAction::Rename);
            } else if pressed(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::N) {
                self.open_file_dialog(view, local, SftpFileAction::Mkdir);
            }
        }
    }
    pub(super) fn advance_file_operation(
        &mut self,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let Some(dialog) = &mut self.browser.operation else {
            return;
        };
        if dialog.error.is_some() {
            return;
        }
        let Some(run) = &mut dialog.run else {
            return;
        };
        if view.workspace_epoch != run.epoch {
            dialog.error = Some("操作状态发生变化，已停止后续操作。请刷新目录核对结果。".into());
            return;
        }
        if view.workspace_busy || command.is_some() {
            return;
        }
        if view.generation != run.generation && view.error.is_some() {
            dialog.error = Some(format!(
                "已完成 {}/{} 项；连接或操作出现错误，已停止后续提交。请刷新目录核对结果。\n{}",
                run.completed,
                run.total,
                view.error.as_deref().unwrap_or_default()
            ));
            return;
        }
        let job = view.file_job.as_ref().filter(|job| {
            job.session == session
                && job.draft == run.current
                && run
                    .job
                    .map_or(Some(job.id) != run.before, |id| job.id == id)
        });
        let Some(job) = job else {
            if view.generation != run.generation {
                dialog.error = Some("检查未完成，操作已停止。请查看上方错误并刷新目录。".into());
            }
            return;
        };
        run.job = Some(job.id);
        match job.state {
            SftpFileState::Ready if run.applied_revision.is_none() && view.file_writable => {
                run.applied_revision = Some(job.revision);
                run.generation = view.generation;
                *command = Some(super::super::advanced::workspace(
                    session,
                    None,
                    SftpOperation::FileApply,
                    Vec::new(),
                    job.revision,
                    Some(job.id),
                ));
            }
            SftpFileState::Ready if run.applied_revision.is_none() && !view.file_writable => {
                dialog.error = Some("文件操作权限已失效，请关闭弹框并重新检查。".into());
            }
            SftpFileState::Ready
                if run.applied_revision.is_some() && view.generation != run.generation =>
            {
                dialog.error = Some(
                    "执行结果未确认，已停止后续操作；请刷新目录核对，不会自动重复提交。".into(),
                );
            }
            SftpFileState::Succeeded if run.applied_revision.is_some() => {
                run.completed += 1;
                if let Some(draft) = run.remaining.pop_front() {
                    run.current = draft;
                    run.before = Some(job.id);
                    run.job = None;
                    run.applied_revision = None;
                    run.generation = view.generation;
                    *command = Some(super::super::advanced::workspace(
                        session,
                        None,
                        SftpOperation::FileInspect,
                        serde_json::to_vec(&run.current).unwrap_or_default(),
                        0,
                        None,
                    ));
                } else {
                    let path = dialog.directory.clone();
                    let local = dialog.local;
                    self.browser.operation = None;
                    *command = Some(self.browse(session, local, path));
                }
            }
            SftpFileState::Unknown | SftpFileState::Cancelled => {
                dialog.error = Some(format!(
                    "已完成 {}/{} 项；操作已停止，请刷新目录核对。\n{}",
                    run.completed, run.total, job.detail
                ));
            }
            _ => {}
        }
    }
    pub(in crate::sftp_panel) fn file_operation_dialog(
        &mut self,
        ctx: &egui::Context,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let Some(dialog) = &mut self.browser.operation else {
            return;
        };
        let mut close = false;
        let mut submit = false;
        let active = dialog.run.is_some() && dialog.error.is_none();
        let response =
            egui::Modal::new(egui::Id::new("sftp-list-file-operation")).show(ctx, |ui| {
                ui.set_width((ctx.content_rect().width() - 64.0).clamp(240.0, 480.0));
                ui.heading(match dialog.action {
                    SftpFileAction::Rename => "重命名",
                    SftpFileAction::Mkdir => "新建目录",
                    _ => "属性 / 权限",
                });
                ui.add(
                    egui::Label::new(format!(
                        "{} · {}",
                        if dialog.local { "本地" } else { "远端" },
                        dialog.directory
                    ))
                    .truncate(),
                )
                .on_hover_text(&dialog.directory);
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("file-operation-body")
                    .max_height((ctx.content_rect().height() - 220.0).max(120.0))
                    .show(ui, |ui| {
                        ui.add_enabled_ui(dialog.run.is_none(), |ui| {
                            if matches!(
                                dialog.action,
                                SftpFileAction::Rename | SftpFileAction::Mkdir
                            ) {
                                if dialog.action == SftpFileAction::Rename {
                                    ui.add(
                                        egui::Label::new(format!(
                                            "原名称：{}",
                                            dialog.entries[0].name
                                        ))
                                        .truncate(),
                                    );
                                }
                                ui.label("新名称");
                                let mut field = egui::TextEdit::singleline(&mut dialog.name)
                                    .char_limit(255)
                                    .desired_width(f32::INFINITY)
                                    .show(ui);
                                if dialog.focus {
                                    field.response.request_focus();
                                    field.state.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::two(
                                            egui::text::CCursor::new(0),
                                            egui::text::CCursor::new(dialog.name.chars().count()),
                                        ),
                                    ));
                                    field.state.store(ctx, field.response.id);
                                    dialog.focus = false;
                                }
                                if (field.response.has_focus() || field.response.lost_focus())
                                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                {
                                    submit = true;
                                }
                            } else {
                                ui.label(format!("已选择 {} 项", dialog.entries.len()));
                                for entry in &dialog.entries {
                                    ui.add(egui::Label::new(&entry.path).truncate())
                                        .on_hover_text(&entry.path);
                                }
                                if dialog.entries.len() == 1 {
                                    let entry = &dialog.entries[0];
                                    for column in [
                                        columns::Column::Kind,
                                        columns::Column::Modified,
                                        columns::Column::Permissions,
                                        columns::Column::Size,
                                    ] {
                                        ui.label(format!(
                                            "{}：{}",
                                            column.label(),
                                            column.value(entry)
                                        ));
                                    }
                                    ui.label(format!(
                                        "所有者 UID：{}    组 GID：{}",
                                        entry
                                            .uid
                                            .map(|v| v.to_string())
                                            .unwrap_or_else(|| "—".into()),
                                        entry
                                            .gid
                                            .map(|v| v.to_string())
                                            .unwrap_or_else(|| "—".into())
                                    ));
                                } else {
                                    let total = dialog
                                        .entries
                                        .iter()
                                        .filter(|entry| entry.kind == 1)
                                        .filter_map(|entry| entry.size)
                                        .fold(0u64, u64::saturating_add);
                                    ui.label(format!(
                                        "已知文件大小合计：{}（不含目录内容）",
                                        bytes(total)
                                    ));
                                }
                            }
                            let can_chmod = !(dialog.local && cfg!(windows))
                                && dialog.entries.iter().all(|e| matches!(e.kind, 1 | 2))
                                && dialog.entries.len() <= 128;
                            if dialog.action == SftpFileAction::Mkdir && !dialog.local
                                || matches!(
                                    dialog.action,
                                    SftpFileAction::Inspect | SftpFileAction::Chmod
                                ) && can_chmod
                            {
                                ui.separator();
                                if matches!(
                                    dialog.action,
                                    SftpFileAction::Inspect | SftpFileAction::Chmod
                                ) {
                                    dialog.action = SftpFileAction::Chmod;
                                    ui.weak("将以下权限应用到所有选中项，不递归修改子项");
                                    if dialog
                                        .entries
                                        .iter()
                                        .any(|e| e.permissions != dialog.entries[0].permissions)
                                    {
                                        ui.colored_label(
                                            cshell_ui::theme::ACCENT,
                                            "所选项权限不同，应用后将统一为下方权限。",
                                        );
                                    }
                                    if dialog.entries.iter().any(|e| e.permissions.is_none()) {
                                        ui.weak("部分权限未提供，请输入要设置的八进制权限。");
                                    }
                                }
                                permissions(ui, &mut dialog.mode);
                            } else if matches!(
                                dialog.action,
                                SftpFileAction::Inspect | SftpFileAction::Chmod
                            ) {
                                ui.weak(if dialog.local && cfg!(windows) {
                                    "Windows 本地权限由 ACL 管理，此处仅查看属性。"
                                } else {
                                    "符号链接或特殊文件的权限不在此处修改。"
                                });
                            }
                        });
                        if let Some(run) = &dialog.run {
                            ui.label(format!("已完成 {}/{} 项", run.completed, run.total));
                        }
                        if active {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("正在检查并执行，请稍候…");
                            });
                        }
                        if let Some(error) = &dialog.error {
                            ui.colored_label(cshell_ui::theme::DANGER, error);
                        }
                    });
                ui.separator();
                let drafts = dialog.drafts();
                let editing = dialog.run.is_none();
                if editing && dialog.action != SftpFileAction::Inspect {
                    if let Err(error) = &drafts {
                        ui.add(egui::Label::new(*error).wrap());
                    }
                }
                ui.horizontal(|ui| {
                    if editing
                        && dialog.action != SftpFileAction::Inspect
                        && ui
                            .add_enabled(
                                drafts.is_ok() && !view.workspace_busy && command.is_none(),
                                egui::Button::new("应用"),
                            )
                            .clicked()
                    {
                        submit = true;
                    }
                    if ui
                        .add_enabled(
                            !active,
                            egui::Button::new(if editing { "取消" } else { "关闭" }),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
                if submit
                    && editing
                    && !view.workspace_busy
                    && command.is_none()
                    && let Ok(drafts) = drafts
                {
                    let total = drafts.len();
                    let mut remaining: VecDeque<_> = drafts.into();
                    let Some(current) = remaining.pop_front() else {
                        dialog.error = Some("请选择要操作的文件".into());
                        return;
                    };
                    *command = Some(super::super::advanced::workspace(
                        session,
                        None,
                        SftpOperation::FileInspect,
                        serde_json::to_vec(&current).unwrap_or_default(),
                        0,
                        None,
                    ));
                    dialog.run = Some(Run {
                        remaining,
                        current,
                        before: view.file_job.as_ref().map(|j| j.id),
                        job: None,
                        applied_revision: None,
                        generation: view.generation,
                        epoch: view.workspace_epoch,
                        completed: 0,
                        total,
                    });
                    dialog.error = None;
                }
            });
        // Ignore backdrop clicks, and do not discard an operation while its receipt is pending.
        if close
            || !active
                && response.is_top_modal
                && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.browser.operation = None;
            ctx.memory_mut(|m|{if let Some(id)=m.focused(){m.surrender_focus(id);}});
        }
        if active {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}
fn permissions(ui: &mut egui::Ui, mode: &mut String) {
    ui.horizontal(|ui| {
        ui.label("八进制权限");
        ui.add(
            egui::TextEdit::singleline(mode)
                .char_limit(4)
                .desired_width(72.0),
        );
    });
    if let Ok(mut value) = u32::from_str_radix(mode.trim(), 8)
        && value <= 0o7777
    {
        let old = value;
        egui::Grid::new("posix-permissions")
            .min_col_width(76.0)
            .show(ui, |ui| {
                ui.allocate_exact_size(egui::vec2(76.0, 24.0), egui::Sense::hover());
                ui.label("读取 r");
                ui.label("写入 w");
                ui.label("执行 x");
                ui.end_row();
                for (label, shift) in [("所有者", 6), ("用户组", 3), ("其他人", 0)] {
                    ui.label(label);
                    for (bit, label) in [(4, "读取"), (2, "写入"), (1, "执行")] {
                        let mask = bit << shift;
                        let mut enabled = value & mask != 0;
                        ui.push_id((shift, bit), |ui| {
                            if ui.checkbox(&mut enabled, label).changed() {
                                if enabled {
                                    value |= mask;
                                } else {
                                    value &= !mask;
                                }
                            }
                        });
                    }
                    ui.end_row();
                }
            });
        ui.horizontal_wrapped(|ui| {
            for (bit, label) in [(0o4000, "Set UID"), (0o2000, "Set GID"), (0o1000, "Sticky")] {
                let mut enabled = value & bit != 0;
                if ui.checkbox(&mut enabled, label).changed() {
                    if enabled {
                        value |= bit;
                    } else {
                        value &= !bit;
                    }
                }
            }
        });
        if value != old {
            *mode = format!("{value:04o}");
        }
    }
}
#[cfg(test)]
mod tests;
