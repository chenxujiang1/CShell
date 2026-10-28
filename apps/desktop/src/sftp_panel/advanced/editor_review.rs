use super::*;
use cshell_domain::SftpQueueItemId;
type Signature = (SessionId, u64, [u8; 32], [u8; 32], u8);
#[derive(Default)]
pub(super) struct ReviewPanel {
    doc: Option<SessionId>,
    open: bool,
    tab: u8,
    ack: bool,
    replace_draft: bool,
    backup: Option<SftpQueueItemId>,
    seen: Option<Signature>,
    pub(super) pending: Option<(SessionId, u64, SftpVerifiedContent)>,
    worker: Option<std::sync::mpsc::Receiver<(Signature, Result<Vec<TextDiffRow>, String>)>>,
    cache: Option<(Signature, Vec<TextDiffRow>)>,
    error: Option<String>,
}
fn review_request(
    session: SessionId,
    edit: &SftpEditDocument,
    backup: Option<SftpQueueItemId>,
) -> SftpClientCommand {
    workspace(
        session,
        None,
        SftpOperation::EditReview,
        serde_json::to_vec(&SftpEditReviewRequest {
            revision: edit.revision,
            backup,
        })
        .unwrap_or_default(),
        0,
        Some(edit.id),
    )
}
impl AdvancedSftp {
    #[cfg(test)]
    pub(in crate::sftp_panel) fn preview_editor_review(&mut self, id: SessionId) {
        self.review = ReviewPanel {
            doc: Some(id),
            open: true,
            tab: 1,
            ..Default::default()
        };
    }
    pub(super) fn editor_review(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        edit: &SftpEditDocument,
        command: &mut Option<SftpClientCommand>,
    ) {
        if self.review.doc != Some(edit.id) {
            self.review = ReviewPanel {
                doc: Some(edit.id),
                ..Default::default()
            };
        }
        if let Some((id, revision, base)) = self.review.pending {
            if edit.id == id && edit.revision > revision && edit.base == base {
                self.edit_stamp = Some((edit.id, edit.revision, edit.current, edit.base));
                self.review.pending = None;
                self.editor_notice =
                    Some("已接受服务器版本为保存基线，草稿仍保留；尚未上传。".into());
            } else if !view.workspace_busy && view.error.is_some() {
                self.review.pending = None;
            }
        }
        let available = edit.state == SftpEditState::Ready
            && !edit.closed
            && edit.queued_item.is_none()
            && edit.session == session;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    available && !view.workspace_busy && command.is_none(),
                    egui::Button::new("查看差异 / 冲突 / 备份"),
                )
                .clicked()
            {
                self.review.open = true;
                self.review.seen = None;
                self.review.error = None;
                *command = Some(review_request(session, edit, self.review.backup));
            }
            if self.review.open && ui.button("收起对照").clicked() {
                self.review.open = false;
            }
        });
        if !self.review.open {
            return;
        }
        let Some(value) = view
            .edit_review
            .as_ref()
            .filter(|v| v.id == edit.id && v.session == session && v.revision == edit.revision)
        else {
            if view.workspace_busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在读取版本和已验证备份…");
                });
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            } else {
                ui.weak("对照未载入或已过期，请点击查看差异；本地草稿保留。");
            }
            return;
        };
        let conflict =
            value.remote_content != edit.base || value.remote_permissions != edit.base_permissions;
        if conflict {
            ui.colored_label(
                cshell_ui::theme::WARNING,
                "服务器文件已变化：请对照并合并草稿，再明确接受保存基线。",
            );
        }
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.review.tab, 0, "基础 → 草稿");
            ui.selectable_value(&mut self.review.tab, 1, "服务器 → 草稿");
            ui.selectable_value(&mut self.review.tab, 2, "备份 → 草稿");
        });
        if self.review.tab == 2 {
            let mut chosen = None;
            let mut clear = false;
            egui::ComboBox::from_id_salt(("editor-backups", edit.id))
                .selected_text(
                    value
                        .backups
                        .iter()
                        .position(|b| Some(b.item) == self.review.backup)
                        .map_or("选择已验证备份…".into(), |i| {
                            format!("备份 {}", i + 1)
                        }),
                )
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.review.backup.is_none(), "不载入备份")
                        .clicked()
                    {
                        clear = true;
                    }
                    for (index, backup) in value.backups.iter().enumerate() {
                        if ui
                            .selectable_label(
                                self.review.backup == Some(backup.item),
                                format!(
                                    "备份 {} · {}",
                                    index + 1,
                                    super::super::browser::bytes(backup.content.size)
                                ),
                            )
                            .on_hover_text(&backup.path)
                            .clicked()
                        {
                            chosen = Some(backup.item);
                        }
                    }
                    if value.backups.is_empty() {
                        ui.weak("此连接的队列中暂无已验证备份。");
                    }
                });
            if clear && !view.workspace_busy && command.is_none() {
                self.review.backup = None;
                self.review.seen = None;
                *command = Some(review_request(session, edit, None));
            }
            if let Some(item) = chosen
                && !view.workspace_busy
                && command.is_none()
            {
                self.review.backup = Some(item);
                self.review.replace_draft = false;
                self.review.seen = None;
                *command = Some(review_request(session, edit, Some(item)));
            }
            if let Some(text) = value
                .backup_text
                .as_ref()
                .filter(|_| value.selected_backup == self.review.backup)
            {
                if self.edit_dirty {
                    ui.checkbox(
                        &mut self.review.replace_draft,
                        "允许用备份替换当前未保存草稿",
                    );
                }
                if ui
                    .add_enabled(
                        !self.edit_dirty || self.review.replace_draft,
                        egui::Button::new("载入备份为草稿（尚不上传）"),
                    )
                    .clicked()
                {
                    self.edit_buffer = text.clone();
                    self.edit_dirty = true;
                    self.edit_stamp = Some((edit.id, edit.revision, edit.current, edit.base));
                    self.review.ack = false;
                    self.editor_notice = Some(
                        "备份已载入草稿。核对后使用保存并上传完成恢复；当前服务器版本仍受检查。"
                            .into(),
                    );
                }
                if ui.button("复制备份路径").clicked() {
                    if let Some(backup) = value
                        .backups
                        .iter()
                        .find(|b| Some(b.item) == self.review.backup)
                    {
                        ui.ctx().copy_text(backup.path.clone());
                    }
                }
            }
        }
        let left = match self.review.tab {
            0 => value.base.as_ref(),
            1 => Some(&value.remote),
            _ => value
                .backup_text
                .as_ref()
                .filter(|_| value.selected_backup == self.review.backup),
        };
        let Some(left) = left else {
            ui.weak(if self.review.tab == 0 {
                "基础文本未保留；可对照当前服务器，草稿不会被替换。"
            } else {
                "请选择并读取备份；超过 256 KiB 的文件请使用外部编辑器。"
            });
            return;
        };
        let signature = (
            edit.id,
            edit.revision,
            sftp_digest(left.as_bytes()).sha256,
            sftp_digest(self.edit_buffer.as_bytes()).sha256,
            self.review.tab,
        );
        if let Some(worker) = &self.review.worker {
            match worker.try_recv() {
                Ok((sig, result)) => {
                    self.review.worker = None;
                    match result {
                        Ok(rows) if sig == signature => {
                            self.review.cache = Some((sig, rows));
                            self.review.error = None;
                        }
                        Ok(_) => {}
                        Err(error) => self.review.error = Some(error),
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.review.worker = None;
                    self.review.error = Some("差异计算已中断，请点击刷新差异重试".into());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let changed = self.review.seen != Some(signature);
        if changed {
            self.review.ack = false;
        }
        let refresh = ui
            .add_enabled(self.review.worker.is_none(), egui::Button::new("刷新差异"))
            .clicked();
        if (self.review.seen.is_none() || refresh || changed && self.review.cache.is_none())
            && self.review.worker.is_none()
        {
            let left = left.clone();
            let right = self.edit_buffer.clone();
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("cshell-editor-diff".into())
                .spawn(move || {
                    let value = align_text_output(left.as_bytes(), right.as_bytes())
                        .map_err(|_| "文本差异超过处理范围，请使用外部编辑器".into());
                    let _ = sender.send((signature, value));
                }) {
                Ok(_) => {
                    self.review.worker = Some(receiver);
                    self.review.seen = Some(signature);
                }
                Err(_) => {
                    self.review.seen = Some(signature);
                    self.review.error = Some("差异计算线程无法启动，请点击刷新差异重试".into());
                }
            }
        }
        if self.review.worker.is_some() {
            ui.spinner();
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
        if let Some(error) = &self.review.error {
            ui.colored_label(cshell_ui::theme::WARNING, error);
        }
        let current = self
            .review
            .cache
            .as_ref()
            .filter(|(sig, _)| *sig == signature);
        if let Some((_, rows)) = current {
            let edits = rows
                .iter()
                .filter(|r| r.kind != TextDiffKind::Equal)
                .count();
            ui.weak(format!(
                "左侧行号 / 草稿行号 · {edits} 行变更；仅显示对照，不自动合并。"
            ));
            egui::ScrollArea::vertical()
                .id_salt(("editor-diff", edit.id))
                .max_height(180.0)
                .show_rows(ui, 18.0, rows.len(), |ui, range| {
                    for row in &rows[range] {
                        let (sign, color) = match row.kind {
                            TextDiffKind::Equal => (" ", cshell_ui::theme::MUTED),
                            TextDiffKind::Delete => ("−", cshell_ui::theme::DANGER),
                            TextDiffKind::Insert => ("+", cshell_ui::theme::ACCENT),
                        };
                        let text = row.text.chars().take(1024).collect::<String>();
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{sign} {:>5} {:>5} {}",
                                    row.left_line.map_or(String::new(), |v| v.to_string()),
                                    row.right_line.map_or(String::new(), |v| v.to_string()),
                                    text.trim_end_matches(['\r', '\n'])
                                ))
                                .monospace()
                                .color(color),
                            )
                            .truncate(),
                        );
                    }
                });
        } else if changed {
            ui.weak("草稿或版本已变化，点击刷新差异后继续核对。");
        }
        if conflict && self.review.tab == 1 {
            ui.checkbox(
                &mut self.review.ack,
                "已核对服务器差异，保留草稿并使用此版本作为保存基线",
            );
            if ui
                .add_enabled(
                    current.is_some()
                        && self.review.ack
                        && !view.workspace_busy
                        && view.edit_writable
                        && command.is_none(),
                    egui::Button::new("接受此保存基线（尚不上传）"),
                )
                .clicked()
            {
                self.review.pending = Some((edit.id, edit.revision, value.remote_content));
                *command = Some(workspace(
                    session,
                    None,
                    SftpOperation::EditRebase,
                    serde_json::to_vec(&SftpEditRebase {
                        remote: value.remote_content,
                        permissions: value.remote_permissions,
                    })
                    .unwrap_or_default(),
                    edit.revision,
                    Some(edit.id),
                ));
            }
        }
    }
}
