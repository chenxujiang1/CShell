//! Per-host, bounded editor drafts. Opening or switching never submits remote changes.
use super::*;
use std::collections::BTreeMap;
type Stamp = (SessionId, u64, SftpVerifiedContent, SftpVerifiedContent);
#[derive(Clone)]
pub(super) struct Document {
    pub path: String,
    pub session: SessionId,
    buffer: String,
    stamp: Option<Stamp>,
    dirty: bool,
}
#[derive(Default)]
pub(super) struct Documents {
    picker: Option<std::sync::mpsc::Receiver<Option<std::path::PathBuf>>>,
    preferences_loaded: bool,
    pub external_footer: bool,
    pub entries: BTreeMap<SessionId, Document>,
    active: Option<SessionId>,
    pub close_requested: bool,
    orphan: Option<SessionId>,
    orphan_discard: bool,
    find_open: bool,
    query: String,
    replacement: String,
    search_from: usize,
    last_match: Option<std::ops::Range<usize>>,
}
impl AdvancedSftp {
    pub(super) fn keep_unsaved_documents(&mut self) {
        self.remember_document();
        self.documents.entries.retain(|_, d| d.dirty);
        self.documents.active = None;
    }
    pub(super) fn external_editor_tools(&mut self, ui: &mut egui::Ui) {
        if !self.documents.preferences_loaded {
            self.documents.preferences_loaded = true;
            if let Some(path) = crate::display_settings::settings_path()
                .map(|p| p.with_file_name("sftp-editor.json"))
                && std::fs::metadata(&path).is_ok_and(|m| m.len() <= 8192)
                && let Ok(data) = std::fs::read(path)
                && let Ok(prefs) = serde_json::from_slice::<EditorPreferences>(&data)
                && self.editor_executable.is_empty()
            {
                self.editor_executable = prefs.executable;
                self.editor_arguments = prefs.arguments;
            }
        }
        if let Some(receiver) = &self.documents.picker
            && let Ok(path) = receiver.try_recv()
        {
            self.documents.picker = None;
            if let Some(path) = path {
                self.editor_executable = path.to_string_lossy().into();
                self.editor_arguments = "[\"{file}\"]".into();
            }
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("系统文本编辑器").clicked() {
                let candidates = if cfg!(windows) {
                    vec![
                        std::env::var_os("SystemRoot")
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(|| "C:/Windows".into())
                            .join("System32/notepad.exe"),
                    ]
                } else if cfg!(target_os = "macos") {
                    vec!["/System/Applications/TextEdit.app/Contents/MacOS/TextEdit".into()]
                } else {
                    vec![
                        "/usr/bin/gedit".into(),
                        "/usr/bin/kate".into(),
                        "/usr/bin/mousepad".into(),
                    ]
                };
                if let Some(path) = candidates.into_iter().find(|p| p.is_file()) {
                    self.editor_executable = path.to_string_lossy().into();
                    self.editor_arguments = "[\"{file}\"]".into();
                } else {
                    self.editor_notice = Some("没有找到系统文本编辑器，请使用浏览选择。".into());
                }
            }
            if ui.button("VS Code").clicked() {
                let mut candidates = vec![];
                for base in [
                    std::env::var_os("LOCALAPPDATA"),
                    std::env::var_os("ProgramFiles"),
                ] {
                    if let Some(base) = base {
                        let base = std::path::PathBuf::from(base);
                        candidates.push(base.join("Programs/Microsoft VS Code/Code.exe"));
                        candidates.push(base.join("Microsoft VS Code/Code.exe"));
                    }
                }
                candidates.extend([
                    std::path::PathBuf::from("/usr/bin/code"),
                    std::path::PathBuf::from(
                        "/Applications/Visual Studio Code.app/Contents/MacOS/Electron",
                    ),
                ]);
                if let Some(path) = candidates.into_iter().find(|p| p.is_file()) {
                    self.editor_executable = path.to_string_lossy().into();
                    self.editor_arguments = "[\"--wait\", \"{file}\"]".into();
                } else {
                    self.editor_notice =
                        Some("没有找到 VS Code，请使用浏览选择其可执行程序。".into());
                }
            }
            if ui
                .add_enabled(
                    self.documents.picker.is_none(),
                    egui::Button::new("浏览编辑器…"),
                )
                .clicked()
            {
                let (send, recv) = std::sync::mpsc::channel();
                if std::thread::Builder::new()
                    .name("cshell-editor-picker".into())
                    .spawn(move || {
                        let path = rfd::FileDialog::new()
                            .set_title("选择文本编辑器可执行程序")
                            .pick_file();
                        let _ = send.send(path);
                    })
                    .is_ok()
                {
                    self.documents.picker = Some(recv);
                }
            }
            if ui.button("保存为默认编辑器").clicked() {
                let prefs = EditorPreferences {
                    executable: self.editor_executable.clone(),
                    arguments: self.editor_arguments.clone(),
                };
                let result = crate::display_settings::settings_path()
                    .ok_or_else(|| "无法确定设置目录".to_owned())
                    .and_then(|p| prefs.save(&p.with_file_name("sftp-editor.json")));
                self.editor_notice = Some(match result {
                    Ok(()) => "默认编辑器已保存；后续会话共用此设置。".into(),
                    Err(e) => format!("默认编辑器未保存：{e}"),
                });
            }
        });
    }
    pub(in crate::sftp_panel) fn editor_page(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        remote: &str,
        command: &mut Option<SftpClientCommand>,
    ) {
        let height = if self.documents.close_requested {
            112.0
        } else {
            76.0
        };
        let footer = egui::Panel::bottom("sftp-editor-footer")
            .exact_size(height)
            .frame(cshell_ui::form::utility_bar())
            .show(ui, |_ui| {});
        self.documents.external_footer = true;
        egui::ScrollArea::vertical()
            .id_salt("sftp-editor-page")
            .max_height(ui.available_height())
            .show(ui, |ui| {
                if view.edit.is_none() && self.document_tabs(ui, session, view, command) {
                    return;
                }
                if self.orphan_document(ui) {
                    return;
                }
                self.editor(ui, session, view, remote, command);
            });
        self.documents.external_footer = false;
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(footer.response.rect.shrink(6.0))
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| self.editor_save_controls(ui, session, view, command),
        );
    }
    pub(super) fn editor_save_controls(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let Some(edit) = &view.edit else {
            ui.weak("选择远程文件并按 F4 开始编辑");
            return;
        };
        let stamp = (edit.id, edit.revision, edit.current, edit.base);
        let save_key = !egui::Popup::is_any_open(ui.ctx())
            && ui
                .ctx()
                .input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S));
        let queue = view.queue.as_ref();
        let recovering = queue.is_some_and(|q| {
            edit.queued_item.is_some_and(|id| {
                q.items
                    .iter()
                    .any(|r| r.id == id && r.state == SftpQueueState::Pending)
            })
        });
        let ready = self.documents.orphan.is_none()
            && self.documents.active == Some(edit.id)
            && !self.documents.close_requested
            && self.save_pending.is_none()
            && view.edit_writable
            && view.queue_writable
            && !view.workspace_busy
            && !view.queue_busy
            && edit.session == session
            && edit.state == SftpEditState::Ready
            && !edit.closed
            && (edit.queued_item.is_none() || recovering)
            && (recovering || self.edit_dirty || edit.current != edit.base)
            && (!self.edit_dirty || self.edit_stamp == Some(stamp))
            && self.edit_buffer.len() <= MAX_SFTP_INLINE_EDIT_BYTES
            && queue.is_some();
        ui.horizontal_wrapped(|ui| {
            let clicked = ui
                .add_enabled(
                    ready,
                    egui::Button::new(if recovering {
                        "继续上传已保存版本"
                    } else {
                        "保存并上传    Ctrl+S"
                    })
                    .fill(cshell_ui::theme::ACCENT_FILL),
                )
                .clicked();
            if ready && command.is_none() && (clicked || save_key && !recovering)
                && let Some(queue) = queue
            {
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::EditSave,
                    serde_json::to_vec(&SftpEditSave {
                        content: (!recovering && self.edit_dirty).then(|| self.edit_buffer.clone()),
                        queue_revision: queue.revision,
                        accept_exclusive_replace: true,
                    })
                    .unwrap_or_default(),
                    edit.revision,
                    Some(edit.id),
                ));
                self.save_confirm = false;
                self.editor_notice = None;
                self.save_pending = Some((edit.id, edit.revision, view.generation));
            }
            self.close_document_ui(ui, session, view, command);
        });
        ui.weak("保存时检查远端变化并备份原文件；上传进度在“文件与传输”中查看。");
    }
    pub(super) fn remember_document(&mut self) {
        if let Some(id) = self.documents.active
            && let Some(doc) = self.documents.entries.get_mut(&id)
        {
            doc.buffer = self.edit_buffer.clone();
            doc.stamp = self.edit_stamp;
            doc.dirty = self.edit_dirty;
        }
    }
    pub(super) fn activate_document(&mut self, edit: &SftpEditDocument, view: &SftpView) {
        if self.documents.active != Some(edit.id) {
            self.remember_document();
            if let Some(doc) = self.documents.entries.get(&edit.id) {
                self.edit_buffer = doc.buffer.clone();
                self.edit_stamp = doc.stamp;
                self.edit_dirty = doc.dirty;
            } else {
                self.edit_buffer = view.edit_text.as_deref().cloned().unwrap_or_default();
                self.edit_dirty = false;
                self.edit_stamp = Some((edit.id, edit.revision, edit.current, edit.base));
            }
            self.documents.active = Some(edit.id);
            self.documents.last_match = None;
            self.documents.search_from = 0;
            self.documents.close_requested = false;
        }
        if edit.closed {
            self.documents.entries.remove(&edit.id);
        } else {
            self.documents
                .entries
                .entry(edit.id)
                .and_modify(|d| {
                    d.path = edit.remote_path.clone();
                    d.session = edit.session;
                })
                .or_insert_with(|| Document {
                    path: edit.remote_path.clone(),
                    session: edit.session,
                    buffer: self.edit_buffer.clone(),
                    stamp: self.edit_stamp,
                    dirty: self.edit_dirty,
                });
        }
    }
    pub(super) fn document_tabs(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) -> bool {
        let mut target = None;
        egui::ScrollArea::horizontal()
            .id_salt("sftp-editor-tabs")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (id, doc) in &self.documents.entries {
                        let active = self.documents.active == Some(*id);
                        let dirty = if active { self.edit_dirty } else { doc.dirty };
                        let title = format!(
                            "{}{}",
                            doc.path.rsplit('/').next().unwrap_or(&doc.path),
                            if dirty { " •" } else { "" }
                        );
                        let response = ui
                            .add_enabled(
                                !view.workspace_busy,
                                egui::Button::new(if doc.session == session {
                                    title
                                } else {
                                    format!("旧连接 · {title}")
                                })
                                .selected(active),
                            )
                            .on_hover_text(&doc.path);
                        if response.clicked() {
                            if doc.session != session {
                                self.documents.orphan = Some(*id);
                                self.documents.orphan_discard = false;
                            } else {
                                self.documents.orphan = None;
                                if !active {
                                    target = Some(*id);
                                }
                            }
                        }
                        response.context_menu(|ui| {
                            if ui.button("复制草稿内容").clicked() {
                                ui.ctx().copy_text(if active {
                                    self.edit_buffer.clone()
                                } else {
                                    doc.buffer.clone()
                                });
                                ui.close();
                            }
                        });
                    }
                })
            });
        if let Some(id) = target {
            self.remember_document();
            *command = Some(workspace(
                session,
                None,
                SftpOperation::EditGet,
                Vec::new(),
                0,
                Some(id),
            ));
            return true;
        }
        false
    }
    fn orphan_document(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(id) = self.documents.orphan else {
            return false;
        };
        let Some(doc) = self.documents.entries.get(&id) else {
            self.documents.orphan = None;
            return false;
        };
        ui.heading("旧连接的本地草稿");
        ui.label(&doc.path);
        ui.weak("草稿仍保留在内存中。不会将旧连接的内容自动上传到新连接。");
        if ui.button("复制完整草稿").clicked() {
            ui.ctx().copy_text(doc.buffer.clone());
        }
        let mut text = doc.buffer.clone();
        ui.add(
            egui::TextEdit::multiline(&mut text)
                .code_editor()
                .interactive(false)
                .desired_rows(8),
        );
        ui.checkbox(
            &mut self.documents.orphan_discard,
            "确认不再需要这份本地草稿",
        );
        if ui
            .add_enabled(
                self.documents.orphan_discard,
                egui::Button::new("关闭旧草稿标签"),
            )
            .clicked()
        {
            self.documents.entries.remove(&id);
            self.documents.orphan = None;
        }
        if ui.button("返回当前连接编辑器").clicked() {
            self.documents.orphan = None;
        }
        true
    }
    pub(super) fn editor_text(&mut self, ui: &mut egui::Ui, edit: &SftpEditDocument) -> bool {
        let ctx = ui.ctx().clone();
        if !egui::Popup::is_any_open(&ctx)
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F))
        {
            self.documents.find_open = true;
        }
        let mut next = false;
        let mut replace = false;
        let mut all = false;
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            if ui.small_button("查找 / 替换    Ctrl+F").clicked() {
                self.documents.find_open = !self.documents.find_open;
            }
            ui.weak(format!(
                "{} 行 · UTF-8",
                self.edit_buffer.split('\n').count()
            ));
        });
        if self.documents.find_open {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.documents.query)
                            .hint_text("查找文本")
                            .desired_width(150.0),
                    )
                    .changed()
                {
                    self.documents.search_from = 0;
                    self.documents.last_match = None;
                }
                next = ui.button("下一处    F3").clicked();
                ui.add(
                    egui::TextEdit::singleline(&mut self.documents.replacement)
                        .hint_text("替换为")
                        .desired_width(150.0),
                );
                replace = ui
                    .add_enabled(
                        self.documents.last_match.is_some(),
                        egui::Button::new("替换此处"),
                    )
                    .clicked();
                all = ui
                    .add_enabled(
                        !self.documents.query.is_empty(),
                        egui::Button::new("全部替换"),
                    )
                    .clicked();
                if ui.small_button("收起").clicked() {
                    self.documents.find_open = false;
                }
            });
            next |= ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F3));
        }
        let editable = edit.state == SftpEditState::Ready && !edit.closed;
        if editable
            && replace
            && let Some(range) = self.documents.last_match.take()
        {
            if self.edit_buffer.get(range.clone()) == Some(self.documents.query.as_str())
                && self.edit_buffer.len() - range.len() + self.documents.replacement.len()
                    <= MAX_SFTP_INLINE_EDIT_BYTES
            {
                self.edit_buffer
                    .replace_range(range.clone(), &self.documents.replacement);
                self.documents.search_from = range.start + self.documents.replacement.len();
                changed = true;
                next = true;
            }
        }
        if editable && all {
            let count = self.edit_buffer.matches(&self.documents.query).count();
            if self
                .edit_buffer
                .len()
                .saturating_sub(count * self.documents.query.len())
                .saturating_add(count.saturating_mul(self.documents.replacement.len()))
                <= MAX_SFTP_INLINE_EDIT_BYTES
            {
                self.edit_buffer = self
                    .edit_buffer
                    .replace(&self.documents.query, &self.documents.replacement);
                changed = count > 0;
                self.documents.last_match = None;
                self.documents.search_from = 0;
                self.editor_notice = Some(format!("已替换 {count} 处；修改尚未上传"));
            } else {
                self.editor_notice = Some("替换结果超过内置编辑上限，未修改内容".into());
            }
        }
        let reserved=if self.documents.external_footer {if self.documents.close_requested{197.0}else{161.0}}else{220.0};
        let height = (ctx.content_rect().bottom() - ui.cursor().min.y - reserved).clamp(42.0, 320.0);
        let lines = (1..=self.edit_buffer.split('\n').count())
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let response = egui::ScrollArea::vertical()
            .id_salt(("editor-scroll", edit.id))
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(lines)
                            .monospace()
                            .color(cshell_ui::theme::MUTED),
                    ));
                    ui.add_enabled(
                        editable,
                        egui::TextEdit::multiline(&mut self.edit_buffer)
                            .id(egui::Id::new(("sftp-editor-text", edit.id)))
                            .code_editor()
                            .char_limit(MAX_SFTP_INLINE_EDIT_BYTES)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner
            })
            .inner;
        if response.changed() {
            changed = true;
            self.documents.last_match = None;
            self.documents.search_from = 0;
        }
        if next && !self.documents.query.is_empty() {
            let from = self.documents.search_from.min(self.edit_buffer.len());
            if let Some(start) = self
                .edit_buffer
                .get(from..)
                .and_then(|s| s.find(&self.documents.query).map(|n| n + from))
                .or_else(|| self.edit_buffer.find(&self.documents.query))
            {
                let end = start + self.documents.query.len();
                let mut state =
                    egui::text_edit::TextEditState::load(&ctx, response.id).unwrap_or_default();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(self.edit_buffer[..start].chars().count()),
                        egui::text::CCursor::new(self.edit_buffer[..end].chars().count()),
                    )));
                state.store(&ctx, response.id);
                response.request_focus();
                self.documents.last_match = Some(start..end);
                self.documents.search_from = end;
            } else {
                self.editor_notice = Some("没有找到匹配文本".into());
            }
        }
        changed
    }
    pub(super) fn close_document_ui(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        if self.documents.orphan.is_some() {
            return;
        }
        let Some(edit) = &view.edit else {
            return;
        };
        let ready = view.edit_writable
            && !view.workspace_busy
            && !edit.closed
            && edit.session == session
            && edit.state != SftpEditState::Preparing;
        let mut close = false;
        if ui
            .add_enabled(ready, egui::Button::new("关闭当前文件"))
            .clicked()
        {
            if self.edit_dirty {
                self.documents.close_requested = true;
            } else {
                close = true;
            }
        }
        if self.documents.close_requested {
            ui.colored_label(cshell_ui::theme::WARNING, "此文件有未保存修改。");
            ui.horizontal_wrapped(|ui| {
                if ui.button("返回编辑并保存").clicked() {
                    self.documents.close_requested = false;
                }
                if ui
                    .add_enabled(ready, egui::Button::new("放弃修改并关闭"))
                    .clicked()
                {
                    self.edit_dirty = false;
                    self.documents.close_requested = false;
                    close = true;
                }
            });
        }
        if close && command.is_none() {
            *command = Some(workspace(
                session,
                Some(edit.session),
                SftpOperation::EditClose,
                Vec::new(),
                edit.revision,
                Some(edit.id),
            ));
        }
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EditorPreferences {
    executable: String,
    arguments: String,
}
impl EditorPreferences {
    fn save(&self, path: &std::path::Path) -> Result<(), String> {
        if !std::path::Path::new(&self.executable).is_absolute()
            || !std::path::Path::new(&self.executable).is_file()
        {
            return Err("请选择实际存在的编辑器可执行程序".into());
        }
        let args: Vec<String> = if self.arguments.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&self.arguments).map_err(|_| "参数须为 JSON 字符串数组")?
        };
        if args.len() > 32 || args.iter().any(|s| s.contains('\0') || s.len() > 1024) {
            return Err("编辑器参数超出范围".into());
        }
        let data = serde_json::to_vec_pretty(self).map_err(|_| "编辑器设置编码失败")?;
        if data.len() > 8192 {
            return Err("编辑器设置过长".into());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "无法创建设置目录")?;
        }
        let temporary = path.with_extension(format!("pending-{}", std::process::id()));
        std::fs::write(&temporary, data).map_err(|_| "无法写入设置")?;
        std::fs::rename(&temporary, path).map_err(|_| "无法保存设置")?;
        Ok(())
    }
}
