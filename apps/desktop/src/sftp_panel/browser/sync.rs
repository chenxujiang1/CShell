mod presets;
use super::*;
use cshell_application::{SftpSyncApply, SftpSyncDifference as Diff, SftpSyncOptions};

#[derive(Default)]
pub(super) struct Panel {
    open: bool,
    presets:Vec<crate::sftp_settings::SyncPreset>,presets_loaded:bool,preset_id:Option<SessionId>,preset_name:String,
    excludes_text:String,preserve_modified:bool,preset_navigation:Option<presets::Navigation>,
    session: Option<SessionId>,
    id: Option<SessionId>,
    direction: Option<SftpQueueDirection>,
    compare_content: Option<bool>,
    selected: BTreeSet<String>,
    replace: bool,
    show_matches: bool,
    submitted: bool,
    local_error: Option<String>,
    outbound: bool,
}
impl Panel {
    #[cfg(test)]
    pub(super) fn preview(session: SessionId, id: SessionId) -> Self {
        Self {
            open: true,
            session: Some(session),
            id: Some(id),
            direction: Some(SftpQueueDirection::Upload),
            selected: ["app/config.toml".into()].into_iter().collect(),
            ..Default::default()
        }
    }
    pub fn open(&mut self) {
        self.open = true;
    }
    pub(super) fn request_rejected(&mut self) {
        if !std::mem::take(&mut self.outbound) {
            return;
        }
        self.submitted = false;
        self.local_error = Some("请求尚未发送，请稍后再次点击；当前选择已保留。".into());
    }
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        verification: cshell_domain::SftpVerification,
        command: &mut Option<SftpClientCommand>,
    ) {
        self.outbound = false;
        if self.session != Some(session) {
            let open = self.session.is_none() && self.open;
            *self = Self {
                session: Some(session),
                open,
                ..Default::default()
            };
        }
        let ready = view.connection_phase == crate::sftp_connection::ConnectionPhase::Connected
            || view.connection_profile.is_none();

        if !self.open {
            return;
        }
        self.advance_preset(session,view,command);
        let ctx = ui.ctx().clone();
        let mut close = false;
        let response =
            egui::Modal::new(egui::Id::new(("sftp-sync-review", session))).show(&ctx, |ui| {
                ui.set_width(
                    (ctx.content_rect().width() * 0.88)
                        .clamp(260.0, 1000.0)
                        .min((ctx.content_rect().width() - 40.0).max(100.0)),
                );
                ui.horizontal_wrapped(|ui|{ui.heading("目录比较与同步");self.preset_tools(ui,view);});
                if self.preset_navigation.is_some(){ui.weak("正在进入方案目录，尚未开始传输…");}
                if let Some(peer) = &view.peer {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{}@{}:{}",
                                peer.username, peer.host.host, peer.host.port
                            ))
                            .strong(),
                        )
                        .truncate(),
                    );
                }
                ui.add(
                    egui::Label::new(format!(
                        "本地：{}",
                        view.local_directory.as_deref().unwrap_or("未选择")
                    ))
                    .truncate(),
                )
                .on_hover_text(view.local_directory.as_deref().unwrap_or(""));
                ui.add(
                    egui::Label::new(format!(
                        "远端：{}",
                        view.remote_loaded_directory.as_deref().unwrap_or("未选择")
                    ))
                    .truncate(),
                )
                .on_hover_text(view.remote_loaded_directory.as_deref().unwrap_or(""));
                ui.weak("比较元数据 · 保留目标端多余文件 · 五分钟内有效")
                    .on_hover_text("按名称、类型、大小和修改时间比较；元数据一致不证明内容相同。");
                let busy = view.sync_pending.is_some();
                if busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            if view
                                .sync_pending
                                .is_some_and(|(_, op)| op == SftpOperation::SyncApply)
                            {
                                "正在重新核对目录并提交；完成后可在传输区控制任务。"
                            } else {
                                "正在递归读取目录；此步骤不修改文件。"
                            },
                        );
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(150));
                }
                if let Some(error) = view.sync_error.as_ref().or(self.local_error.as_ref()) {
                    ui.add(egui::Label::new(egui::RichText::new(error).color(cshell_ui::theme::WARNING)).truncate()).on_hover_text(error);
                }
                let mut direction = self.direction.unwrap_or(SftpQueueDirection::Upload);
                if self.direction.is_none() {
                    self.direction = Some(direction);
                }
                let mut compare_content=*self.compare_content.get_or_insert(true);
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.selectable_value(
                            &mut direction,
                            SftpQueueDirection::Upload,
                            "本地 → 远端",
                        );
                        ui.selectable_value(
                            &mut direction,
                            SftpQueueDirection::Download,
                            "远端 → 本地",
                        );
                        ui.checkbox(&mut compare_content,"轻量核对内容").on_hover_text("只核对同尺寸但元数据不同的普通文件：每文件最多 1 MiB，每侧总计 8 MiB / 32 个文件，最多 10 秒。未核对项目仍显示元数据差异；不下载大文件做比较。");
                    });
                    if ui
                        .add_enabled(
                            !busy && ready && self.preset_navigation.is_none() && command.is_none(),
                            egui::Button::new(if self.id.is_some() {
                                "重新比较"
                            } else {
                                "开始比较"
                            }),
                        )
                        .clicked()
                    {
                        if let (Some(local), Some(remote)) =
                            (&view.local_directory, &view.remote_loaded_directory)
                        {
                            let id = SessionId::new();
                            self.id = Some(id);
                            self.selected.clear();
                            self.submitted = false;
                            self.local_error=None;self.outbound=true;
                            *command = Some(SftpClientCommand::Sync(
                                session,
                                SftpOperation::SyncScan,
                                id,
                                serde_json::to_vec(&SftpSyncOptions {
                                    local: local.clone(),
                                    remote: remote.clone(),
                                    direction,
                                    verification,
                                    compare_content,excludes:self.rules(),preserve_modified:self.preserve_modified,
                                })
                                .unwrap_or_default(),
                            ));
                        }
                    }
                });
                if self.direction != Some(direction) {
                    self.direction = Some(direction);
                    self.id = None;
                    self.selected.clear();
                }
                if self.compare_content!=Some(compare_content){self.compare_content=Some(compare_content);self.id=None;self.selected.clear();}
                let preview = view
                    .sync_preview
                    .as_ref()
                    .filter(|p| Some(p.id) == self.id && p.options.direction == direction);
                if let Some(p) = preview {
                    let stale = view.local_directory.as_ref() != Some(&p.options.local)
                        || view.remote_loaded_directory.as_ref() != Some(&p.options.remote);
                    if stale {
                        ui.colored_label(cshell_ui::theme::WARNING, "左右目录已变化，请重新比较。");
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label("同名变更：");
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.selectable_value(&mut self.replace, false, "跳过");
                            ui.selectable_value(&mut self.replace, true, "备份后替换");
                        });
                        ui.checkbox(&mut self.show_matches, "显示一致项");
                    });
                    self.selected.retain(|path| {
                        p.entries
                            .iter()
                            .any(|e| e.relative == *path && e.eligible(direction, self.replace))
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(!busy, egui::Button::new("选择所有可同步项"))
                            .clicked()
                        {
                            self.selected = p
                                .entries
                                .iter()
                                .filter(|e| e.eligible(direction, self.replace))
                                .map(|e| e.relative.clone())
                                .collect();
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new("清空选择"))
                            .clicked()
                        {
                            self.selected.clear();
                        }
                        ui.label(format!(
                            "共 {} 项 · 已选 {} 项",
                            p.entries.len(),
                            self.selected.len()
                        ));
                    });
                    let entries = p
                        .entries
                        .iter()
                        .filter(|e| self.show_matches || !matches!(e.difference,Diff::MetadataMatch|Diff::ContentMatch))
                        .collect::<Vec<_>>();
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        ui.add_sized([105.0, 20.0], egui::Label::new("差异"));
                        let width = (ui.available_width() - 170.0).max(80.0);
                        ui.add_sized([width, 20.0], egui::Label::new("相对路径"));
                        ui.add_sized([77.0, 20.0], egui::Label::new("本地"));
                        ui.add_sized([77.0, 20.0], egui::Label::new("远端"));
                    });
                    let h = (ctx.content_rect().height() - 450.0).clamp(24.0, 360.0);
                    egui::ScrollArea::vertical()
                        .id_salt("sftp-sync-rows")
                        .max_height(h)
                        .show_rows(ui, 26.0, entries.len(), |ui, range| {
                            for index in range {
                                let e = entries[index];
                                let eligible = e.eligible(direction, self.replace);
                                let mut checked = self.selected.contains(&e.relative);
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(
                                            !busy && eligible,
                                            egui::Checkbox::without_text(&mut checked),
                                        )
                                        .changed()
                                    {
                                        if checked {
                                            self.selected.insert(e.relative.clone());
                                        } else {
                                            self.selected.remove(&e.relative);
                                        }
                                    }
                                    let label = match e.difference {
                                        Diff::LocalOnly => "仅本地",
                                        Diff::RemoteOnly => "仅远端",
                                        Diff::Changed => "元数据差异",
                                        Diff::ContentChanged=>"内容不同",
                                        Diff::ContentMatch=>"内容一致",
                                        Diff::MetadataMatch => "元数据一致",
                                        Diff::TypeConflict => "类型冲突 · 跳过",
                                    };
                                    ui.add_sized([105.0, 24.0], egui::Label::new(label).truncate()).on_hover_text(if e.difference==Diff::Changed{"内容尚未核对，可能超出轻量核对预算；不代表内容必然不同。"}else{label});
                                    let width = (ui.available_width() - 170.0).max(80.0);
                                    ui.add_sized(
                                        [width, 24.0],
                                        egui::Label::new(&e.relative).truncate(),
                                    )
                                    .on_hover_text(&e.relative);
                                    for metadata in [&e.local, &e.remote] {
                                        let text = metadata.as_ref().map_or("—".into(), |m| {
                                            if m.directory {
                                                "文件夹".into()
                                            } else {
                                                bytes(m.size)
                                            }
                                        });
                                        let modified = metadata
                                            .as_ref()
                                            .and_then(|m| m.modified)
                                            .and_then(|t| u32::try_from(t).ok());
                                        ui.add_sized(
                                            [77.0, 24.0],
                                            egui::Label::new(text).truncate(),
                                        )
                                        .on_hover_text(
                                            format!("修改时间：{}", columns::modified(modified)),
                                        );
                                    }
                                });
                            }
                        });
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        let label = format!(
                            "{} {} 项",
                            if direction == SftpQueueDirection::Upload {
                                "上传"
                            } else {
                                "下载"
                            },
                            self.selected.len()
                        );
                        if ui
                            .add_enabled(
                                !busy
                                    && !stale
                                    && !self.submitted
                                    && !self.selected.is_empty()
                                    && command.is_none(),
                                egui::Button::new(label),
                            )
                            .clicked()
                        {
                            self.submitted = true;
                        self.local_error=None;self.outbound=true;
                            *command = Some(SftpClientCommand::Sync(
                                session,
                                SftpOperation::SyncApply,
                                p.id,
                                serde_json::to_vec(&SftpSyncApply {
                                    selected: self.selected.iter().cloned().collect(),
                                    backup_replace: self.replace,
                                })
                                .unwrap_or_default(),
                            ));
                        }
                        ui.weak(if self.replace {
                            "选中的同名变更会先备份，再替换。"
                        } else {
                            "仅添加所选缺失项，不覆盖同名文件。"
                        });
                    });
                } else if self.submitted && view.trees.contains_key(&self.id.unwrap_or(session)) {
                    ui.colored_label(
                        cshell_ui::theme::ACCENT,
                        "同步已提交，可在下方传输区查看进度、暂停或取消。",
                    );
                    close = true;
                } else if !busy {
                    ui.label("选择传输方向后点击“重新比较”，检查差异并选择要传输的项目。");
                }
                if ui
                    .button(if busy {
                        "收起（后台继续）"
                    } else {
                        "关闭"
                    })
                    .clicked()
                {
                    close = true;
                }
            });
        if close
            || !egui::Popup::is_any_open(&ctx) && response.is_top_modal
                && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.open = false;self.preset_navigation=None;
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
    }
    pub fn blocks(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_unsent_sync_requests_release_submit_and_keep_selected_paths() {
        let mut panel = Panel {
            submitted: true,
            selected: ["keep".into()].into_iter().collect(),
            ..Default::default()
        };
        panel.request_rejected();
        assert!(panel.submitted);
        assert!(panel.local_error.is_none());
        panel.outbound = true;
        panel.request_rejected();
        assert!(!panel.submitted);
        assert!(panel.selected.contains("keep"));
        assert!(panel.local_error.is_some());
    }
    use super::super::tests::{draw, fixture};
    use super::*;
    use crate::ui_layout_gate::{bounds, context};
    #[test]
    fn sync_review_keeps_submit_visible_and_requires_fresh_matching_directories() {
        for size in [egui::vec2(720.0, 480.0), egui::vec2(1280.0, 800.0)] {
            let ctx = context();
            let (mut panel, mut view, session) = fixture();
            let id = SessionId::new();
            let root = view.local_directory.clone().unwrap();
            let remote = view.remote_loaded_directory.clone().unwrap();
            let preview = cshell_application::SftpSyncPreview {
                id,
                session,
                options: SftpSyncOptions {
                    local: root,
                    remote,
                    direction: SftpQueueDirection::Upload,
                    verification: Default::default(),
                    compare_content: true,excludes:Vec::new(),preserve_modified:false,
                },
                entries: vec![cshell_application::SftpSyncEntry {
                    relative: "nested/config.conf".into(),
                    local: Some(cshell_application::SftpSyncMetadata {
                        directory: false,
                        size: 10,
                        modified: Some(1),
                    }),
                    remote: None,
                    difference: Diff::LocalOnly,
                }],
            };
            view.sync_preview = Some(std::sync::Arc::new(preview));
            panel.browser.sync = Panel {
                open: true,
                session: Some(session),
                id: Some(id),
                direction: Some(SftpQueueDirection::Upload),
                selected: ["nested/config.conf".into()].into_iter().collect(),
                ..Default::default()
            };
            for _ in 0..4 {
                draw(&ctx, &mut panel, &view, size, vec![]);
            }
            let (tree, command) = draw(&ctx, &mut panel, &view, size, vec![]);
            assert!(command.is_none());
            let button = tree
                .nodes
                .iter()
                .find(|(_, n)| {
                    n.role() == egui::accesskit::Role::Button && n.label() == Some("上传 1 项")
                })
                .expect("submit visible");
            let rect = bounds(&button.1);
            assert!(
                rect.height() > 12.0 && rect.bottom() < size.y && rect.top() > 0.0,
                "{size:?}: {rect:?}"
            );
            assert!(!button.1.is_disabled());
            view.remote_loaded_directory = Some("/different".into());
            let (tree, command) = draw(&ctx, &mut panel, &view, size, vec![]);
            assert!(command.is_none());
            assert!(
                tree.nodes
                    .iter()
                    .find(|(_, n)| n.role() == egui::accesskit::Role::Button
                        && n.label() == Some("上传 1 项"))
                    .unwrap()
                    .1
                    .is_disabled()
            );
        }
    }
}
