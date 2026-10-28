//! Upload plans and explicit recovery, using only the daemon control connection.
use crate::{
    host_group_connection::HostGroupView,
    upload_connection::{UploadConnection, UploadView},
};
use cshell_application::*;
use cshell_domain::{HostGroupId, HostId, HostSelection, TaskRunId};
use cshell_ipc::TaskOperation;
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

pub(crate) struct UploadPanel {
    draft: BatchUploadDefinition,
    query: String,
    group: Option<HostGroupId>,
    selection: Option<HostSelection>,
    selection_mode: u8,
    create_confirm: bool,
    builder_stamp: Vec<u8>,
    start_confirm: bool,
    recovery_confirm: bool,
    stamp: Option<(TaskRunId, Vec<u8>)>,
    generation: Option<u64>,
    control_stamp: Option<(bool, bool, bool)>,
    selected: Option<HostId>,
    action: u8,
    filter: String,
    phase: Option<UploadPhase>,
    run_id: String,
    refresh_at: Option<Instant>,
    status: String,
}
impl Default for UploadPanel {
    fn default() -> Self {
        Self {
            draft: BatchUploadDefinition {
                publication_policy: None,
                name: "批量上传".into(),
                local_source: String::new(),
                remote_destination: String::new(),
                size: 0,
                sha256: [0; 32],
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
            builder_stamp: Vec::new(),
            start_confirm: false,
            recovery_confirm: false,
            stamp: None,
            generation: None,
            control_stamp: None,
            selected: None,
            action: 0,
            filter: String::new(),
            phase: None,
            run_id: String::new(),
            refresh_at: None,
            status: String::new(),
        }
    }
}
impl UploadPanel {
    pub(crate) fn deactivate(&mut self) {
        self.create_confirm = false;
        self.start_confirm = false;
        self.recovery_confirm = false;
    }
    pub(crate) fn use_selection(&mut self, selection: HostSelection) {
        self.selection = Some(selection);
        self.selection_mode = 2;
        self.create_confirm = false;
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
        connection: &UploadConnection,
        view: &UploadView,
        hosts: Option<&HostGroupView>,
        open: &mut bool,
    ) -> egui::Rect {
        let controls = (view.busy, view.writes_allowed, view.cancel_busy);
        if self.control_stamp != Some(controls) {
            self.deactivate();
            self.control_stamp = Some(controls);
        }
        if self.generation != Some(view.generation) {
            let stamp = view
                .document
                .as_ref()
                .map(|doc| (doc.id, serde_json::to_vec(&**doc).unwrap_or_default()));
            if self.stamp != stamp {
                if self.stamp.as_ref().map(|s| s.0) != stamp.as_ref().map(|s| s.0) {
                    self.selected = None;
                }
                self.start_confirm = false;
                self.recovery_confirm = false;
                self.stamp = stamp;
            }
            self.generation = Some(view.generation);
        }
        let rect = egui::CentralPanel::default()
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("upload-panel-scroll")
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.heading("多主机上传");
                            if ui.button("返回终端").clicked() {
                                *open = false;
                                self.start_confirm = false;
                                self.recovery_confirm = false;
                            }
                        });
                        ui.label(&view.status);
                        if !self.status.is_empty() {
                            ui.colored_label(cshell_ui::theme::WARNING, &self.status);
                        }
                        if !view.writes_allowed {
                            ui.colored_label(
                                cshell_ui::theme::WARNING,
                                "请求结果不明。重新载入保存状态后再执行后续操作。",
                            );
                        }
                        ui.collapsing("创建上传计划", |ui| {
                            self.builder(ui, connection, view, hosts)
                        });
                        ui.separator();
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add_enabled(!view.busy, egui::Button::new("重新加载上传列表"))
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
                            .id_salt("upload-run-list")
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
                                                )),
                                            )
                                            .clicked()
                                        {
                                            let _queued = connection.get(row.id, true);
                                        }
                                    }
                                },
                            );
                        ui.horizontal_wrapped(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.run_id)
                                    .hint_text("上传运行标识")
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
                                        let _queued = connection.get(id, true);
                                        self.status.clear();
                                    }
                                    _ => self.status = "请输入有效运行标识".into(),
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
        if let Some(document) = &view.document {
            if document.active
                && !view.busy
                && self
                    .refresh_at
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(2))
                && connection.get(document.id, false)
            {
                self.refresh_at = Some(Instant::now());
            }
        }
        rect
    }
    fn builder(
        &mut self,
        ui: &mut egui::Ui,
        connection: &UploadConnection,
        view: &UploadView,
        hosts: Option<&HostGroupView>,
    ) {
        let before = self.draft.clone();
        ui.label("计划名称");
        ui.add(egui::TextEdit::singleline(&mut self.draft.name).char_limit(128));
        crate::task_definition_editor::draw_publication_policy(
            ui,
            &mut self.draft.publication_policy,
        );
        ui.label("本机源文件的绝对路径");
        ui.add(egui::TextEdit::singleline(&mut self.draft.local_source).char_limit(4096));
        ui.label("远端完整目的路径（必须尚不存在）");
        ui.add(
            egui::TextEdit::singleline(&mut self.draft.remote_destination)
                .hint_text("/srv/releases/app.tar")
                .char_limit(4000),
        );
        ui.horizontal_wrapped(|ui| {
            ui.label("并发");
            ui.add(egui::DragValue::new(&mut self.draft.concurrency).range(1..=50));
        });
        ui.collapsing("超时策略", |ui| {
            for (label, value) in [
                ("排队 ms", &mut self.draft.queue_timeout_ms),
                ("连接 ms", &mut self.draft.connect_timeout_ms),
                ("传输 ms", &mut self.draft.transfer_timeout_ms),
            ] {
                ui.horizontal_wrapped(|ui| {
                    ui.label(label);
                    ui.add(egui::DragValue::new(value).range(1..=86_400_000_u64));
                });
            }
        });
        if before != self.draft {
            self.create_confirm = false;
            if view.hashing {
                connection.cancel_hash();
            }
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!view.busy, egui::Button::new("生成文件大小与 SHA-256"))
                .clicked()
            {
                if self.draft.validate().is_err() {
                    self.status = "请填写有效路径、名称和超时策略".into();
                } else if connection.describe(self.draft.clone()) {
                    self.status.clear();
                    self.create_confirm = false;
                }
            }
            if view.hashing && ui.button("取消摘要计算").clicked() {
                connection.cancel_hash();
            }
        });
        let manifest = view
            .manifest
            .as_ref()
            .filter(|manifest| matches_draft(manifest, &self.draft));
        if let Some(manifest) = manifest {
            ui.label(format!(
                "{} 字节 · SHA-256 {}",
                manifest.size,
                digest(manifest.sha256)
            ));
        } else {
            ui.label("保存前先生成当前源文件的内容摘要");
        }
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
                egui::ComboBox::from_id_salt("upload-group")
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
        let expected = snapshot
            .zip(selection.as_ref())
            .and_then(|(snapshot, selection)| {
                select_hosts(&snapshot.catalog, selection, MAX_HOST_TARGETS).ok()
            });
        let builder_stamp = serde_json::to_vec(&(
            manifest,
            &selection,
            snapshot.map(|s| (s.revision, s.profile_revision)),
        ))
        .unwrap_or_default();
        if self.builder_stamp != builder_stamp {
            self.create_confirm = false;
            self.builder_stamp = builder_stamp;
        }
        if let Some(expected) = &expected {
            ui.label(format!("匹配 {} 个目标", expected.len()));
            egui::ScrollArea::vertical()
                .id_salt("upload-preview")
                .max_height(160.0)
                .show_rows(
                    ui,
                    ui.text_style_height(&egui::TextStyle::Body) + 4.0,
                    expected.len(),
                    |ui, range| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        for host in &expected[range] {
                            ui.label(format!(
                                "{} · {}:{} · {}",
                                host.name, host.address, host.port, host.id
                            ));
                        }
                    },
                );
        }
        if let Some(snapshot) = snapshot {
            ui.label(format!(
                "主机版本 {} / Profile 版本 {}",
                snapshot.revision, snapshot.profile_revision
            ));
        }
        ui.checkbox(
            &mut self.create_confirm,
            "确认源摘要、目的路径、策略和当前匹配目标",
        );
        if ui
            .add_enabled(
                !view.busy
                    && view.writes_allowed
                    && self.create_confirm
                    && manifest.is_some()
                    && expected.as_ref().is_some_and(|ids| !ids.is_empty())
                    && hosts.is_some_and(|h| h.writable && !h.busy),
                egui::Button::new("保存冻结计划"),
            )
            .clicked()
            && let (Some(manifest), Some(selection), Some(expected), Some(snapshot)) =
                (manifest, selection, expected, snapshot)
        {
            if connection.create(
                manifest.clone(),
                selection,
                expected
                    .into_iter()
                    .map(|host| host.id)
                    .collect::<BTreeSet<_>>(),
                snapshot.revision,
                snapshot.profile_revision,
            ) {
                self.create_confirm = false;
                self.status.clear();
            }
        }
    }
    fn review(
        &mut self,
        ui: &mut egui::Ui,
        connection: &UploadConnection,
        view: &UploadView,
        document: &UploadStatusDocument,
    ) {
        ui.separator();
        ui.heading(&document.definition.name);
        ui.label(document.id.to_string());
        ui.label(format!("源：{}", document.definition.local_source));
        ui.label(format!("目的：{}", document.definition.remote_destination));
        ui.label(format!(
            "文件策略：{:?}（None 为不覆盖）",
            document.definition.publication_policy
        ));
        ui.label(format!(
            "{} 字节 · SHA-256 {}",
            document.definition.size,
            digest(document.definition.sha256)
        ));
        ui.label(format!(
            "冻结 {} 个目标 · 主机版本 {} / Profile 版本 {} · 并发 {}",
            document.targets.len(),
            document.host_revision,
            document.profile_revision,
            document.definition.concurrency
        ));
        ui.label(format!(
            "排队 / 连接 / 传输超时：{} / {} / {} ms",
            document.definition.queue_timeout_ms,
            document.definition.connect_timeout_ms,
            document.definition.transfer_timeout_ms
        ));
        ui.label("源文件按声明的大小和摘要核对；远端写入独占临时文件，校验后仅发布至尚不存在的目的路径。");
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!view.busy, egui::Button::new("重新载入保存状态"))
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
                                .any(|t| t.row.phase == UploadPhase::Pending)),
                    egui::Button::new("取消上传 / 恢复操作"),
                )
                .clicked()
                && !connection.cancel(document.id)
            {
                self.status = "取消队列忙，请查询状态后重试".into();
            }
        });
        let writable = !view.busy && !view.cancel_busy && view.writes_allowed && !document.active;
        if document
            .targets
            .iter()
            .any(|target| target.row.phase == UploadPhase::Pending)
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
                let _queued = connection.act(TaskOperation::UploadStart, document.id, None, 0);
                self.start_confirm = false;
            }
        }
        if let Some(job) = &document.recovery {
            ui.label(format!(
                "最近恢复操作：{:?} · {:?} · {} · 核对版本 {}",
                job.operation, job.status, job.host_id, job.row_revision
            ));
            if let Some(reason) = job.error {
                ui.label(format!("恢复操作失败：{reason:?}"));
            }
            if let Some(probe) = &job.probe {
                ui.label(format!(
                    "探测版本 {}；临时文件 {}；目的文件 {}",
                    job.row_revision,
                    content(probe.temporary.as_ref()),
                    content(probe.destination.as_ref())
                ));
                ui.label("探测只反映文件内容；相同内容不能证明该运行完成了发布。");
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .char_limit(256)
                    .hint_text("目标名称、地址或标识"),
            );
            egui::ComboBox::from_id_salt("upload-phase-filter")
                .truncate()
                .selected_text(self.phase.map_or("所有状态".into(), |p| format!("{p:?}")))
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut self.phase, None, "所有状态");
                    for phase in [
                        UploadPhase::Pending,
                        UploadPhase::Creating,
                        UploadPhase::Writing,
                        UploadPhase::Verifying,
                        UploadPhase::PublishIntent,
                        UploadPhase::Committed,
                        UploadPhase::CleanupRequired,
                        UploadPhase::Succeeded,
                        UploadPhase::Failed,
                        UploadPhase::Cancelled,
                        UploadPhase::Unknown,
                    ] {
                        ui.selectable_value(&mut self.phase, Some(phase), format!("{phase:?}"));
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
            .id_salt("upload-targets")
            .max_height(280.0)
            .show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body) + 10.0,
                rows.len(),
                |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for target in &rows[range] {
                        if ui
                            .selectable_label(
                                self.selected == Some(target.host_id),
                                format!(
                                    "{} · {}:{} · {:?} · {}/{} 字节 · v{}",
                                    target.name,
                                    target.address,
                                    target.port,
                                    target.row.phase,
                                    target.row.bytes,
                                    document.definition.size,
                                    target.row.revision
                                ),
                            )
                            .clicked()
                        {
                            if self.selected != Some(target.host_id) {
                                self.recovery_confirm = false;
                            }
                            self.selected = Some(target.host_id);
                        }
                    }
                },
            );
        if let Some(target) = self.selected.and_then(|id| document.target(id)) {
            let row = &target.row;
            crate::task_definition_editor::show_publication_receipt(ui, row.publication);
            ui.label(format!(
                "{} · {} · {:?} / {:?}",
                target.name, target.host_id, row.phase, row.intent
            ));
            ui.label(format!("临时文件归属：{} · 内容已校验：{} · 已提交：{} · 显式认领：{} · 临时文件已清理：{}",
                row.temporary_owned, row.verified, row.committed, row.adopted, row.temporary_cleaned));
            if let Some(reason) = row.reason {
                ui.label(format!("原因：{reason:?}"));
            }
            if ui
                .add_enabled(writable, egui::Button::new("只读探测远端文件"))
                .clicked()
            {
                let _queued = connection.act(
                    TaskOperation::UploadProbe,
                    document.id,
                    Some(target.host_id),
                    0,
                );
            }
            let old_action = self.action;
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut self.action, 0, "续传");
                ui.radio_value(&mut self.action, 1, "清理临时文件");
                ui.radio_value(&mut self.action, 2, "认领已存在目的文件");
            });
            if old_action != self.action {
                self.recovery_confirm = false;
            }
            let (operation, allowed, label) = match self.action {
                0 => (
                    TaskOperation::UploadResume,
                    row.phase == UploadPhase::Unknown
                        && row.temporary_owned
                        && !row.verified
                        && !row.committed
                        && row.intent != UploadIntent::Publish,
                    "确认校验现有临时文件前缀后续传",
                ),
                1 => (
                    TaskOperation::UploadCleanup,
                    row.temporary_owned
                        && !row.temporary_cleaned
                        && matches!(
                            row.phase,
                            UploadPhase::Failed
                                | UploadPhase::Cancelled
                                | UploadPhase::CleanupRequired
                                | UploadPhase::Committed
                                | UploadPhase::Unknown
                        )
                        && !(row.phase == UploadPhase::Unknown
                            && row.intent == UploadIntent::Publish),
                    "确认只删除此运行拥有的临时文件",
                ),
                _ => (
                    TaskOperation::UploadAdopt,
                    row.phase == UploadPhase::Unknown
                        && row.intent == UploadIntent::Publish
                        && row.verified
                        && document.recovery.as_ref().is_some_and(|job| {
                            job.host_id == target.host_id
                                && job.row_revision == row.revision
                                && job
                                    .probe
                                    .as_ref()
                                    .and_then(|probe| probe.destination.as_ref())
                                    .is_some_and(|value| {
                                        value.size == document.definition.size
                                            && value.sha256 == document.definition.sha256
                                    })
                        }),
                    "确认将内容一致的目的文件认领为结果；这不证明发布归属",
                ),
            };
            ui.checkbox(&mut self.recovery_confirm, label);
            if !allowed {
                ui.label("当前保存状态不满足此操作条件；认领前需完成当前版本的只读探测。");
            }
            if ui
                .add_enabled(
                    writable && allowed && self.recovery_confirm,
                    egui::Button::new("执行选定恢复操作"),
                )
                .clicked()
            {
                let _queued =
                    connection.act(operation, document.id, Some(target.host_id), row.revision);
                self.recovery_confirm = false;
            }
        }
    }
}
fn matches_draft(manifest: &BatchUploadDefinition, draft: &BatchUploadDefinition) -> bool {
    let mut normalized = manifest.clone();
    normalized.size = draft.size;
    normalized.sha256 = draft.sha256;
    &normalized == draft
}
fn digest(value: [u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn content(value: Option<&UploadContent>) -> String {
    value.map_or("不存在".into(), |value| {
        format!("{} 字节 / SHA-256 {}", value.size, digest(value.sha256))
    })
}
