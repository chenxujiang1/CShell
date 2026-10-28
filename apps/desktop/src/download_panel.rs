//! Frozen download review and explicit, revision-bound local recovery.
use crate::{
    download_connection::{
        DownloadAction, DownloadConnection, DownloadExpectedTargets, DownloadView,
    },
    host_group_connection::HostGroupView,
};
use cshell_application::*;
use cshell_domain::{HostGroupId, HostId, HostRecord, HostSelection, TaskRunId};
use std::time::{Duration, Instant};

pub(crate) struct DownloadPanel {
    draft: BatchDownloadDefinition,
    query: String,
    group: Option<HostGroupId>,
    selection: Option<HostSelection>,
    selection_mode: u8,
    create_confirm: bool,
    start_confirm: bool,
    recovery_confirm: bool,
    preview_stamp: Vec<u8>,
    preview: Vec<HostRecord>,
    preview_error: String,
    builder_stamp: Vec<u8>,
    document_stamp: Option<(TaskRunId, Vec<u8>)>,
    generation: Option<u64>,
    control_stamp: Option<(bool, bool, bool, bool)>,
    selected: Option<HostId>,
    recovery_action: u8,
    filter: String,
    phase: Option<DownloadPhase>,
    run_id: String,
    refresh_at: Option<Instant>,
    read_requested: bool,
    status: String,
}
impl Default for DownloadPanel {
    fn default() -> Self {
        Self {
            draft: BatchDownloadDefinition {
                publication_policy: None,
                name: "批量下载".into(),
                remote_source: String::new(),
                local_directory: String::new(),
                local_name: "download.bin".into(),
                maximum_bytes: MAX_DOWNLOAD_FILE_BYTES,
                concurrency: 5,
                queue_timeout_ms: 300_000,
                connect_timeout_ms: 30_000,
                transfer_timeout_ms: 3_600_000,
            },
            query: "all".into(),
            group: None,
            selection: None,
            selection_mode: 0,
            create_confirm: false,
            start_confirm: false,
            recovery_confirm: false,
            preview_stamp: Vec::new(),
            preview: Vec::new(),
            preview_error: String::new(),
            builder_stamp: Vec::new(),
            document_stamp: None,
            generation: None,
            control_stamp: None,
            selected: None,
            recovery_action: 0,
            filter: String::new(),
            phase: None,
            run_id: String::new(),
            refresh_at: None,
            read_requested: false,
            status: String::new(),
        }
    }
}
impl DownloadPanel {
    pub(crate) fn deactivate(&mut self) {
        self.create_confirm = false;
        self.start_confirm = false;
        self.recovery_confirm = false;
    }
    pub(crate) fn use_selection(&mut self, selection: HostSelection) {
        self.selection = Some(selection);
        self.selection_mode = 2;
        self.deactivate();
    }
    fn selection(&self) -> Option<HostSelection> {
        match self.selection_mode {
            0 => Some(HostSelection::Selector {
                expression: self.query.clone(),
            }),
            1 => self.group.map(|id| HostSelection::Group { id }),
            _ => self.selection.clone(),
        }
    }
    pub(crate) fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &DownloadConnection,
        view: &DownloadView,
        hosts: Option<&HostGroupView>,
        open: &mut bool,
    ) -> egui::Rect {
        let controls = (
            view.available,
            view.writes_allowed,
            view.busy,
            view.cancel_busy,
        );
        if self.control_stamp != Some(controls) {
            self.deactivate();
            self.control_stamp = Some(controls);
        }
        if self.generation != Some(view.generation) {
            let stamp = view
                .document
                .as_ref()
                .map(|doc| (doc.id, serde_json::to_vec(&**doc).unwrap_or_default()));
            if self.document_stamp != stamp {
                if self.document_stamp.as_ref().map(|s| s.0) != stamp.as_ref().map(|s| s.0) {
                    self.selected = None;
                    self.refresh_at = None;
                }
                self.start_confirm = false;
                self.recovery_confirm = false;
                self.document_stamp = stamp;
            }
            self.generation = Some(view.generation);
        }
        let rect = egui::CentralPanel::default()
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("download-panel-scroll")
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.heading("多主机下载");
                            if ui.button("返回终端").clicked() {
                                *open = false;
                                self.deactivate();
                            }
                        });
                        ui.label(&view.status);
                        if !self.status.is_empty() {
                            ui.colored_label(cshell_ui::theme::WARNING, &self.status);
                        }
                        if !view.writes_allowed {
                            ui.colored_label(
                                cshell_ui::theme::WARNING,
                                "请求结果不明。重新载入保存状态后再执行后续修改。",
                            );
                        }
                        ui.collapsing("创建下载计划", |ui| {
                            self.builder(ui, connection, view, hosts)
                        });
                        ui.separator();
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add_enabled(!view.busy, egui::Button::new("重新加载下载列表"))
                                .clicked()
                            {
                                let _queued = connection.list(None);
                            }
                            if ui
                                .add_enabled(
                                    !view.busy && view.rows.len() == 64,
                                    egui::Button::new("下一页"),
                                )
                                .clicked()
                            {
                                let _queued = connection.list(view.rows.last().map(|row| row.id));
                            }
                            if view.after.is_some() {
                                ui.label("按运行标识分页");
                            }
                        });
                        egui::ScrollArea::vertical()
                            .id_salt("download-runs")
                            .max_height(160.0)
                            .show_rows(
                                ui,
                                ui.text_style_height(&egui::TextStyle::Body) + 10.0,
                                view.rows.len(),
                                |ui, range| {
                                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                                    for row in &view.rows[range] {
                                        if ui
                                            .add_enabled(
                                                !view.busy,
                                                egui::Button::new(format!(
                                                    "{} · {} 个目标 · {}",
                                                    row.name, row.targets, row.id
                                                ))
                                                .truncate(),
                                            )
                                            .clicked()
                                        {
                                            self.select_run(connection, row.id);
                                        }
                                    }
                                },
                            );
                        ui.horizontal_wrapped(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.run_id)
                                    .hint_text("下载运行标识")
                                    .char_limit(36),
                            );
                            if ui
                                .add_enabled(!view.busy, egui::Button::new("按标识载入"))
                                .clicked()
                            {
                                match serde_json::from_value::<TaskRunId>(
                                    serde_json::Value::String(self.run_id.clone()),
                                ) {
                                    Ok(id) if !id.as_uuid().is_nil() => {
                                        self.select_run(connection, id);
                                    }
                                    _ => self.status = "请输入有效的下载运行标识".into(),
                                }
                            }
                        });
                        if let Some(document) = &view.document {
                            self.review(ui, connection, view, document);
                        }
                    });
            })
            .response
            .rect;
        if *open && let Some(document) = &view.document {
            if (document.active || view.cancel_busy || self.read_requested)
                && !view.busy
                && self
                    .refresh_at
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(2))
                && connection.get(document.id, false)
            {
                self.refresh_at = Some(Instant::now());
                self.read_requested = false;
            }
        }
        rect
    }
    fn builder(
        &mut self,
        ui: &mut egui::Ui,
        connection: &DownloadConnection,
        view: &DownloadView,
        hosts: Option<&HostGroupView>,
    ) {
        ui.label("计划名称");
        ui.add(egui::TextEdit::singleline(&mut self.draft.name).char_limit(128));
        crate::task_definition_editor::draw_publication_policy(
            ui,
            &mut self.draft.publication_policy,
        );
        ui.label("远端源文件的绝对路径");
        ui.add(
            egui::TextEdit::singleline(&mut self.draft.remote_source)
                .hint_text("/srv/reports/result.bin")
                .char_limit(4000),
        );
        ui.label("本机既有目录（绝对路径）");
        ui.add(egui::TextEdit::singleline(&mut self.draft.local_directory).char_limit(3500));
        ui.label("本地文件名（字母、数字、点、下划线、连字符）");
        ui.add(egui::TextEdit::singleline(&mut self.draft.local_name).char_limit(128));
        ui.label("每台主机保存为“主机标识-文件名”；请选择未使用的文件名或目录。");
        ui.label("备份策略保留 .cshell-backup-运行标识-主机标识.bak；不自动删除备份。");
        ui.horizontal_wrapped(|ui| {
            ui.label("并发");
            ui.add(egui::DragValue::new(&mut self.draft.concurrency).range(1..=50));
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("单文件最大字节数");
            ui.add(
                egui::DragValue::new(&mut self.draft.maximum_bytes)
                    .range(0..=MAX_DOWNLOAD_FILE_BYTES),
            );
        });
        ui.collapsing("超时策略", |ui| {
            for (label, value) in [
                ("排队 ms", &mut self.draft.queue_timeout_ms),
                ("连接 ms", &mut self.draft.connect_timeout_ms),
                ("传输 ms", &mut self.draft.transfer_timeout_ms),
            ] {
                ui.horizontal_wrapped(|ui| {
                    ui.label(label);
                    ui.add(egui::DragValue::new(value).range(1..=604_800_000_u64));
                });
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.selection_mode, 0, "查询");
            ui.radio_value(&mut self.selection_mode, 1, "主机组");
            ui.radio_value(&mut self.selection_mode, 2, "主机面板选择");
        });
        let snapshot = hosts.and_then(|view| view.snapshot.as_ref());
        match self.selection_mode {
            0 => {
                ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .char_limit(MAX_HOST_SELECTOR_BYTES)
                        .hint_text("all / tag:web AND env:prod"),
                );
            }
            1 => {
                egui::ComboBox::from_id_salt("download-group")
                    .truncate()
                    .selected_text(self.group.map_or("选择组".into(), |id| id.to_string()))
                    .show_ui(ui, |ui| {
                        cshell_ui::form::menu(ui);
                        if let Some(snapshot) = snapshot {
                            for group in &snapshot.catalog.groups {
                                ui.selectable_value(&mut self.group, Some(group.id), &group.name);
                            }
                        }
                    });
            }
            _ => {
                ui.label(if self.selection.is_some() {
                    "已采用主机面板选择"
                } else {
                    "请从主机面板工具栏采用选择"
                });
            }
        }
        let selection = self.selection();
        let revisions = snapshot.map(|snapshot| (snapshot.revision, snapshot.profile_revision));
        let preview_stamp = serde_json::to_vec(&(&selection, revisions)).unwrap_or_default();
        if self.preview_stamp != preview_stamp {
            self.preview.clear();
            self.preview_error.clear();
            self.preview_stamp = preview_stamp;
            match (snapshot, selection.as_ref()) {
                (Some(snapshot), Some(selection)) => {
                    match select_hosts(&snapshot.catalog, selection, MAX_HOST_TARGETS) {
                        Ok(hosts) => self.preview = hosts,
                        Err(error) => self.preview_error = error.to_string(),
                    }
                }
                _ => self.preview_error = "请先加载主机库并选择目标".into(),
            }
        }
        let valid = self.draft.validate().is_ok();
        let builder_stamp = serde_json::to_vec(&(
            &self.draft,
            &selection,
            revisions,
            hosts.is_some_and(|host| host.writable && !host.busy),
        ))
        .unwrap_or_default();
        if self.builder_stamp != builder_stamp {
            self.create_confirm = false;
            self.builder_stamp = builder_stamp;
        }
        if !valid {
            ui.colored_label(
                cshell_ui::theme::WARNING,
                "请填写有效的名称、绝对路径、简单文件名和策略",
            );
        }
        if !self.preview_error.is_empty() {
            ui.colored_label(cshell_ui::theme::WARNING, &self.preview_error);
        }
        ui.label(format!("匹配 {} 个目标", self.preview.len()));
        egui::ScrollArea::vertical()
            .id_salt("download-preview")
            .max_height(180.0)
            .show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body) * 2.0 + 10.0,
                self.preview.len(),
                |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for host in &self.preview[range] {
                        let label = format!(
                            "{} · {}:{} · {}",
                            host.name, host.address, host.port, host.id
                        );
                        ui.add(egui::Label::new(&label).truncate())
                            .on_hover_text(label);
                        if valid && let Ok(path) = self.draft.destination(host.id) {
                            path_row(ui, path);
                        } else {
                            ui.add(egui::Label::new("本地目的路径尚未填写有效").truncate());
                        }
                    }
                },
            );
        if let Some((host, profile)) = revisions {
            ui.label(format!("主机版本 {host} / Profile 版本 {profile}"));
        }
        ui.checkbox(
            &mut self.create_confirm,
            "确认远端源、每台主机本地目的路径、策略和匹配目标",
        );
        if ui
            .add_enabled(
                view.available
                    && view.writes_allowed
                    && !view.busy
                    && !view.cancel_busy
                    && self.create_confirm
                    && valid
                    && !self.preview.is_empty()
                    && self.preview_error.is_empty()
                    && hosts.is_some_and(|host| host.writable && !host.busy),
                egui::Button::new("保存冻结下载计划"),
            )
            .clicked()
            && let (Some(selection), Some((hosts, profiles))) = (selection, revisions)
        {
            let expected: DownloadExpectedTargets = self
                .preview
                .iter()
                .map(|host| {
                    (
                        host.id,
                        (host.name.clone(), host.address.clone(), host.port),
                    )
                })
                .collect();
            if connection.create(self.draft.clone(), selection, expected, hosts, profiles) {
                self.create_confirm = false;
                self.status.clear();
            } else {
                self.status = "下载请求队列忙或状态变化，请重新核对".into();
            }
        }
    }
    fn review(
        &mut self,
        ui: &mut egui::Ui,
        connection: &DownloadConnection,
        view: &DownloadView,
        document: &DownloadStatusDocument,
    ) {
        ui.separator();
        ui.heading(&document.definition.name);
        ui.label(format!(
            "文件策略：{:?}（None 为不覆盖）",
            document.definition.publication_policy
        ));
        ui.label(document.id.to_string());
        ui.label(format!("远端源：{}", document.definition.remote_source));
        ui.label(format!(
            "本地目录：{} · 文件名：主机标识-{}",
            document.definition.local_directory, document.definition.local_name
        ));
        ui.label(format!(
            "冻结 {} 个目标 · 主机版本 {} / Profile 版本 {} · 并发 {} · 单文件上限 {} 字节",
            document.targets.len(),
            document.host_revision,
            document.profile_revision,
            document.definition.concurrency,
            document.definition.maximum_bytes
        ));
        ui.label(format!(
            "排队 / 连接 / 传输超时：{} / {} / {} ms",
            document.definition.queue_timeout_ms,
            document.definition.connect_timeout_ms,
            document.definition.transfer_timeout_ms
        ));
        ui.label(
            "每台主机在开始读取时固定自己的内容摘要。完整内容核对后保存新文件；既有文件不替换。",
        );
        let mut phases = [0_usize; 12];
        let mut bytes = 0_u64;
        let mut committed = 0_usize;
        for target in &document.targets {
            phases[target.row.phase.code() as usize] += 1;
            bytes = bytes.saturating_add(target.row.bytes);
            committed += usize::from(target.row.committed);
        }
        ui.label(format!(
            "已记录 {} 字节 · 已提交 {} / {} · 已完成 {} · 失败 {} · 取消 {} · 结果不明 {}",
            bytes,
            committed,
            document.targets.len(),
            phases[DownloadPhase::Succeeded.code() as usize],
            phases[DownloadPhase::Failed.code() as usize],
            phases[DownloadPhase::Cancelled.code() as usize],
            phases[DownloadPhase::Unknown.code() as usize]
        ));
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !view.busy && !view.cancel_busy,
                    egui::Button::new("重新载入保存状态"),
                )
                .clicked()
            {
                let _queued = connection.get(document.id, true);
            }
            if ui
                .add_enabled(
                    !view.cancel_busy
                        && (document.active
                            || document
                                .targets
                                .iter()
                                .any(|target| target.row.phase == DownloadPhase::Pending)),
                    egui::Button::new("取消下载 / 恢复操作"),
                )
                .clicked()
            {
                if connection.cancel(document.id) {
                    self.read_requested = true;
                    self.deactivate();
                } else {
                    self.status = "取消队列忙或状态变化，请查询保存状态".into();
                }
            }
        });
        let writable = view.available
            && view.writes_allowed
            && !view.busy
            && !view.cancel_busy
            && !document.active;
        if document
            .targets
            .iter()
            .any(|target| target.row.phase == DownloadPhase::Pending)
        {
            ui.checkbox(
                &mut self.start_confirm,
                "确认启动上述冻结计划中仍待执行的目标",
            );
            if ui
                .add_enabled(
                    writable && self.start_confirm,
                    egui::Button::new("启动待执行目标"),
                )
                .clicked()
            {
                self.submit(connection, document.id, DownloadAction::Start);
            }
        }
        if let Some(job) = &document.recovery {
            ui.label(format!(
                "最近恢复操作：{} · {} · {} · 核对版本 {}",
                recovery_name(job.operation),
                job_name(job.status),
                job.host_id,
                job.row_revision
            ));
            if let Some(error) = job.error {
                ui.label(format!("操作原因：{error:?}"));
            }
            if let Some(probe) = &job.probe {
                ui.label(format!("临时文件：{}", content(probe.temporary)));
                ui.label(format!("目的文件：{}", content(probe.destination)));
                ui.label("内容一致只说明探测结果；不能据此证明此运行完成了发布。");
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .char_limit(256)
                    .hint_text("目标名称、地址或标识"),
            );
            egui::ComboBox::from_id_salt("download-phase-filter")
                .truncate()
                .selected_text(self.phase.map_or("所有状态", phase_name))
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut self.phase, None, "所有状态");
                    for phase in [
                        DownloadPhase::Pending,
                        DownloadPhase::Inspecting,
                        DownloadPhase::Creating,
                        DownloadPhase::Writing,
                        DownloadPhase::Verifying,
                        DownloadPhase::PublishIntent,
                        DownloadPhase::Committed,
                        DownloadPhase::CleanupRequired,
                        DownloadPhase::Succeeded,
                        DownloadPhase::Failed,
                        DownloadPhase::Cancelled,
                        DownloadPhase::Unknown,
                    ] {
                        ui.selectable_value(&mut self.phase, Some(phase), phase_name(phase));
                    }
                });
        });
        let filter = self.filter.to_lowercase();
        let rows: Vec<_> = document
            .targets
            .iter()
            .filter(|target| {
                self.phase.is_none_or(|phase| phase == target.row.phase)
                    && (filter.is_empty()
                        || target.name.to_lowercase().contains(&filter)
                        || target.address.to_lowercase().contains(&filter)
                        || target.host_id.to_string().contains(&filter))
            })
            .collect();
        ui.label(format!(
            "显示 {} / {} 个目标",
            rows.len(),
            document.targets.len()
        ));
        egui::ScrollArea::vertical()
            .id_salt("download-targets")
            .max_height(300.0)
            .show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body) * 2.0 + 10.0,
                rows.len(),
                |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for target in &rows[range] {
                        let total = target
                            .row
                            .content
                            .map_or("待确定".into(), |value| value.size.to_string());
                        let label = format!(
                            "{} · {}:{} · {} · {} / {} 字节 · v{}",
                            target.name,
                            target.address,
                            target.port,
                            phase_name(target.row.phase),
                            target.row.bytes,
                            total,
                            target.row.revision
                        );
                        if ui
                            .add(
                                egui::Button::selectable(
                                    self.selected == Some(target.host_id),
                                    &label,
                                )
                                .truncate(),
                            )
                            .on_hover_text(label)
                            .clicked()
                        {
                            if self.selected != Some(target.host_id) {
                                self.recovery_confirm = false;
                            }
                            self.selected = Some(target.host_id);
                        }
                        if let Ok((_, destination)) = document.local_paths(target.host_id) {
                            path_row(ui, destination);
                        }
                    }
                },
            );
        if let Some(target) = self.selected.and_then(|id| document.target(id)) {
            ui.separator();
            ui.label(format!(
                "{} · {} · {} · 版本 {}",
                target.name,
                target.host_id,
                phase_name(target.row.phase),
                target.row.revision
            ));
            if let Ok((temporary, destination)) = document.local_paths(target.host_id) {
                ui.label("临时文件");
                ui.monospace(temporary.display().to_string());
                ui.label("目的文件");
                ui.monospace(destination.display().to_string());
            }
            ui.label(format!(
                "固定源内容：{}",
                target
                    .row
                    .content
                    .map_or("尚未固定".into(), |value| content(Some(value)))
            ));
            ui.label(format!(
                "临时文件归属：{} · 已校验：{} · 已提交：{} · 显式认领：{} · 临时文件已清理：{}",
                target.row.temporary_owned,
                target.row.verified,
                target.row.committed,
                target.row.adopted,
                target.row.temporary_cleaned
            ));
            if let Some(reason) = target.row.reason {
                ui.label(format!("原因：{reason:?}"));
            }
            if ui
                .add_enabled(
                    view.available && !view.busy && !view.cancel_busy && !document.active,
                    egui::Button::new("只读探测本地文件"),
                )
                .clicked()
            {
                self.submit(
                    connection,
                    document.id,
                    DownloadAction::Probe(target.host_id, target.row.revision),
                );
            }
            let previous = self.recovery_action;
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut self.recovery_action, 0, "续传");
                ui.radio_value(&mut self.recovery_action, 1, "清理临时文件");
                ui.radio_value(&mut self.recovery_action, 2, "认领目的文件");
            });
            if previous != self.recovery_action {
                self.recovery_confirm = false;
            }
            let row = &target.row;
            crate::task_definition_editor::show_publication_receipt(ui, row.publication);
            let (action, allowed, label) = match self.recovery_action {
                0 => (
                    DownloadAction::Resume(target.host_id, row.revision),
                    row.can_resume(),
                    "确认原源摘要及完整本地前缀一致后续传",
                ),
                1 => (
                    DownloadAction::Cleanup(target.host_id, row.revision),
                    row.can_cleanup(),
                    "确认只删除此运行拥有的临时文件；保留目的文件",
                ),
                _ => (
                    DownloadAction::Adopt(target.host_id, row.revision),
                    row.can_adopt() && document.has_matching_destination_probe(target.host_id),
                    "确认认领内容一致的目的文件；这不能证明发布归属",
                ),
            };
            ui.checkbox(&mut self.recovery_confirm, label);
            if !allowed {
                ui.label("当前保存状态不满足此操作条件；认领前需要当前版本的本地只读探测。");
            }
            if ui
                .add_enabled(
                    writable && allowed && self.recovery_confirm,
                    egui::Button::new("执行选定恢复操作"),
                )
                .clicked()
            {
                self.submit(connection, document.id, action);
            }
        }
    }
    fn select_run(&mut self, connection: &DownloadConnection, id: TaskRunId) {
        if connection.get(id, true) {
            self.read_requested = false;
            self.refresh_at = None;
            self.status.clear();
            self.deactivate();
        } else {
            self.status = "下载请求队列忙，请稍后重新载入".into();
        }
    }
    fn submit(&mut self, connection: &DownloadConnection, id: TaskRunId, action: DownloadAction) {
        if connection.act(id, action) {
            self.read_requested = true;
            self.status.clear();
        } else {
            self.status = "下载请求队列忙或保存状态变化，请重新核对".into();
        }
        self.start_confirm = false;
        self.recovery_confirm = false;
    }
}
fn path_row(ui: &mut egui::Ui, path: std::path::PathBuf) {
    let label = path.display().to_string();
    ui.add(egui::Label::new(egui::RichText::new(&label).monospace()).truncate())
        .on_hover_text(label);
}
fn content(value: Option<DownloadContent>) -> String {
    value.map_or("不存在".into(), |content| {
        format!(
            "{} 字节 · SHA-256 {}",
            content.size,
            content
                .sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    })
}
fn phase_name(phase: DownloadPhase) -> &'static str {
    match phase {
        DownloadPhase::Pending => "待执行",
        DownloadPhase::Inspecting => "读取源摘要",
        DownloadPhase::Creating => "创建临时文件",
        DownloadPhase::Writing => "下载中",
        DownloadPhase::Verifying => "核对内容",
        DownloadPhase::PublishIntent => "发布结果待确认",
        DownloadPhase::Committed => "已提交",
        DownloadPhase::CleanupRequired => "等待临时清理",
        DownloadPhase::Succeeded => "已完成",
        DownloadPhase::Failed => "失败",
        DownloadPhase::Cancelled => "已取消",
        DownloadPhase::Unknown => "结果不明",
    }
}
fn recovery_name(operation: DownloadRecoveryOperation) -> &'static str {
    match operation {
        DownloadRecoveryOperation::Probe => "只读探测",
        DownloadRecoveryOperation::Cleanup => "清理临时文件",
        DownloadRecoveryOperation::Adopt => "认领目的文件",
    }
}
fn job_name(status: FileRecoveryStatus) -> &'static str {
    match status {
        FileRecoveryStatus::Running => "处理中",
        FileRecoveryStatus::Completed => "已完成",
        FileRecoveryStatus::Failed => "失败",
    }
}
