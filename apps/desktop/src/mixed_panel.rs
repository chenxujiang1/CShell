//! Ordered mixed-step drafts and frozen-plan control. Native work belongs to daemon.
use crate::{
    host_group_connection::HostGroupView,
    mixed_connection::{MixedConnection, MixedExpectedTargets, MixedOutputKey, MixedView},
    task_panel::TaskReflowWorker,
};
use cshell_application::*;
use cshell_domain::*;
use cshell_render::{LogSurfaceFrame, LogSurfaceModel};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT_DRAFT_KEY: AtomicU64 = AtomicU64::new(1);
struct StepDraft {
    key: u64,
    action: MixedTaskAction,
    editor: Option<crate::task_definition_editor::TaskDefinitionEditor>,
}

fn new_step(kind: u8) -> Option<StepDraft> {
    let key = NEXT_DRAFT_KEY
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .ok()?;
    let action = match kind {
        1 => MixedTaskAction::Upload {
            definition: BatchUploadDefinition {
                publication_policy: None,
                name: "上传文件".into(),
                local_source: String::new(),
                remote_destination: String::new(),
                size: 0,
                sha256: [0; 32],
                concurrency: 1,
                queue_timeout_ms: 300_000,
                connect_timeout_ms: 30_000,
                transfer_timeout_ms: 3_600_000,
            },
        },
        2 => MixedTaskAction::Download {
            definition: BatchDownloadDefinition {
                publication_policy: None,
                name: "下载文件".into(),
                remote_source: String::new(),
                local_directory: String::new(),
                local_name: "result.bin".into(),
                maximum_bytes: MAX_DOWNLOAD_FILE_BYTES,
                concurrency: 1,
                queue_timeout_ms: 300_000,
                connect_timeout_ms: 30_000,
                transfer_timeout_ms: 3_600_000,
            },
        },
        _ => {
            let mut policy = TaskPolicy::default();
            policy.concurrency = 1;
            MixedTaskAction::Command {
                definition: TaskDefinition {
                    id: TaskId::new(),
                    name: "执行命令".into(),
                    shell: RemoteShellProfile::Posix,
                    steps: vec![TaskStep {
                        name: "执行命令".into(),
                        command: TaskCommand::Script {
                            text: String::new(),
                        },
                        success_codes: [0].into_iter().collect(),
                        idempotent: false,
                    }],
                    variables: BTreeMap::new(),
                    environment: BTreeMap::new(),
                    policy,
                },
            }
        }
    };
    let editor = match &action {
        MixedTaskAction::Command { definition } => {
            Some(crate::task_definition_editor::TaskDefinitionEditor::from_definition(definition))
        }
        _ => None,
    };
    Some(StepDraft {
        key,
        action,
        editor,
    })
}
pub(crate) struct MixedPanel {
    name: String,
    policy: MixedTaskPolicy,
    steps: Vec<StepDraft>,
    query: String,
    group: Option<HostGroupId>,
    selection: Option<HostSelection>,
    selection_mode: u8,
    preview_key: Vec<u8>,
    preview: Vec<HostRecord>,
    preview_error: String,
    builder_key: Vec<u8>,
    create_confirm: bool,
    start_confirm: bool,
    gate_confirm: bool,
    rollout_revision: Option<u64>,
    repair_confirm: bool,
    repair_choice: MixedFileOperation,
    repair_stamp: Vec<u8>,
    generation: Option<u64>,
    run_stamp: Option<TaskRunId>,
    document_key: Vec<u8>,
    controls: Option<(bool, bool, bool, bool)>,
    selected: Option<HostId>,
    filter: String,
    state: Option<MixedTaskState>,
    run_id: String,
    refresh_at: Option<Instant>,
    read_requested: bool,
    status: String,
    output_key: Option<MixedOutputKey>,
    output_stamp: Option<u64>,
    surface: Option<LogSurfaceModel>,
    reflow: Option<TaskReflowWorker>,
    columns: u16,
    results: crate::mixed_results_panel::MixedResultsPanel,
    retry_panel: crate::mixed_retry_panel::MixedRetryPanel,
    analysis_panel: crate::output_analysis_panel::OutputAnalysisPanel,
    search_panel: crate::output_search_panel::OutputSearchPanel,
    merge_panel: crate::output_merge_panel::OutputMergePanel,
    archive_panel: crate::output_archive_panel::OutputArchivePanel,
    workspace_panel: crate::output_workspace_panel::OutputWorkspacePanel,
}
impl Default for MixedPanel {
    fn default() -> Self {
        Self {
            name: "混合任务".into(),
            policy: MixedTaskPolicy::default(),
            steps: new_step(0).into_iter().collect(),
            query: "all".into(),
            group: None,
            selection: None,
            selection_mode: 0,
            preview_key: Vec::new(),
            preview: Vec::new(),
            preview_error: String::new(),
            builder_key: Vec::new(),
            create_confirm: false,
            start_confirm: false,
            gate_confirm: false,
            rollout_revision: None,
            repair_confirm: false,
            repair_choice: MixedFileOperation::Resume,
            repair_stamp: Vec::new(),
            generation: None,
            run_stamp: None,
            document_key: Vec::new(),
            controls: None,
            selected: None,
            filter: String::new(),
            state: None,
            run_id: String::new(),
            refresh_at: None,
            read_requested: false,
            status: String::new(),
            output_key: None,
            output_stamp: None,
            surface: None,
            reflow: None,
            columns: 120,
            results: Default::default(),
            retry_panel: Default::default(),
            analysis_panel: Default::default(),
            search_panel: Default::default(),
            merge_panel: Default::default(),
            archive_panel: Default::default(),
            workspace_panel: Default::default(),
        }
    }
}
impl MixedPanel {
    pub fn deactivate(&mut self) {
        self.workspace_panel.deactivate();
        self.gate_confirm = false;
        self.retry_panel.deactivate();
        self.archive_panel.deactivate();
        self.search_panel.deactivate();
        self.results.deactivate();
        self.create_confirm = false;
        self.start_confirm = false;
        self.repair_confirm = false;
    }
    pub fn use_selection(&mut self, selection: HostSelection) {
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
    fn definition(&self, view: &MixedView) -> Result<MixedTaskDefinition, String> {
        let mut steps = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            let mut action = step.action.clone();
            match &mut action {
                MixedTaskAction::Command { definition } => {
                    *definition = step
                        .editor
                        .as_ref()
                        .ok_or("缺少命令步骤编辑器")?
                        .definition(true)?;
                }
                MixedTaskAction::Upload { definition } => {
                    let saved = view
                        .manifests
                        .get(&step.key)
                        .filter(|saved| manifest_matches(saved, definition))
                        .ok_or_else(|| "请先生成当前上传步骤的源摘要".to_owned())?;
                    *definition = saved.clone();
                }
                _ => {}
            }
            steps.push(action);
        }
        Ok(MixedTaskDefinition {
            name: self.name.clone(),
            policy: self.policy.clone(),
            steps,
        })
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &MixedConnection,
        view: &MixedView,
        hosts: Option<&HostGroupView>,
        open: &mut bool,
    ) -> egui::Rect {
        let controls = (
            view.available,
            view.writes_allowed,
            view.busy,
            view.cancel_busy,
        );
        if self.controls != Some(controls) {
            self.deactivate();
            self.controls = Some(controls);
        }
        if self.generation != Some(view.generation) {
            let repair_stamp = view
                .repair
                .as_ref()
                .map(|document| serde_json::to_vec(&**document).unwrap_or_default())
                .unwrap_or_default();
            if self.repair_stamp != repair_stamp {
                self.repair_confirm = false;
                self.repair_stamp = repair_stamp;
            }
            let run = view.plan.as_ref().map(MixedTaskSnapshot::id);
            if self.run_stamp != run {
                self.selected = None;
                self.clear_output();
                self.refresh_at = None;
                self.run_stamp = run;
            }
            let key = view
                .page
                .as_ref()
                .map(|page| serde_json::to_vec(&**page).unwrap_or_default())
                .unwrap_or_default();
            if key != self.document_key {
                self.start_confirm = false;
                self.repair_confirm = false;
                if view.page.as_ref().is_none_or(|page| {
                    self.selected.is_some_and(|id| {
                        !page.targets.iter().any(|target| target.run.host_id == id)
                    })
                }) {
                    self.selected = None;
                    self.clear_output();
                }
                self.document_key = key;
            }
            self.generation = Some(view.generation);
        }
        self.update_output(view);
        egui::Panel::left("mixed-controls")
            .default_size(380.0)
            .size_range(300.0..=520.0)
            .resizable(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("mixed-controls-scroll")
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.heading("混合任务");
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
                                "请求结果不明；重新载入保存状态后再修改。",
                            );
                        }
                        ui.collapsing("创建混合任务", |ui| {
                            self.builder(ui, connection, view, hosts)
                        });
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add_enabled(!view.busy, egui::Button::new("重新加载列表"))
                                .clicked()
                            {
                                let _queued = connection.list(None);
                            }
                            if ui
                                .add_enabled(
                                    !view.busy && view.rows.len() == 64,
                                    egui::Button::new("下一页运行"),
                                )
                                .clicked()
                            {
                                let _queued = connection.list(view.rows.last().map(|row| row.id));
                            }
                        });
                        egui::ScrollArea::vertical()
                            .id_salt("mixed-runs")
                            .max_height(150.0)
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
                                            self.inspect(connection, row.id);
                                        }
                                    }
                                },
                            );
                        ui.horizontal_wrapped(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.run_id)
                                    .char_limit(36)
                                    .hint_text("混合运行标识"),
                            );
                            if ui
                                .add_enabled(!view.busy, egui::Button::new("载入"))
                                .clicked()
                            {
                                match serde_json::from_value::<TaskRunId>(
                                    serde_json::Value::String(self.run_id.clone()),
                                ) {
                                    Ok(id) if !id.as_uuid().is_nil() => {
                                        self.inspect(connection, id)
                                    }
                                    _ => self.status = "请输入有效运行标识".into(),
                                }
                            }
                        });
                        if let (Some(plan), Some(page)) = (&view.plan, &view.page) {
                            self.review(ui, connection, view, plan, page);
                        }
                    });
            });
        if *open && let (Some(plan), Some(page)) = (&view.plan, &view.page) {
            if (page.active
                || view.cancel_busy
                || self.read_requested
                || view.repair.as_ref().is_some_and(|document| {
                    document
                        .job
                        .as_ref()
                        .is_some_and(|job| job.status == FileRecoveryStatus::Running)
                }))
                && !view.busy
                && self
                    .refresh_at
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(2))
                && connection.page(plan.clone(), page.after, false)
            {
                self.refresh_at = Some(Instant::now());
                self.read_requested = false;
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
            .show(ui, |ui| {
                ui.heading("命令输出字节页");
                if let Some(key) = self.output_key {
                    ui.label(format!(
                        "{} · 步骤 {} · {}",
                        key.host,
                        key.step + 1,
                        if key.stderr { "stderr" } else { "stdout" }
                    ));
                    ui.label("每页最多 64 KiB；控制字符、反斜线和非 ASCII 字节明确转义。");
                    if let (Some(plan),Some(page))=(&view.plan,&view.page)
                        &&plan.id()==key.run&&!page.active
                        &&let Some(left)=page.targets.iter().find(|target|target.run.host_id==key.host){
                        let hosts=page.targets.iter().filter(|target|target.run.state!=MixedTaskState::Running&&target.run.steps.get(key.step as usize).is_some_and(|step|matches!(&step.receipt,MixedReceipt::Command{target,..} if target_terminal(target.state)&&target_terminal(target.steps[0].state))))
                            .map(|target|crate::output_analysis_panel::AnalysisHost{host:target.run.host_id,name:target.name.clone(),revision:target.run.revision,child:target.children[key.step as usize]}).collect();
                        if let Some((host,offset))=self.analysis_panel.draw(ui,&connection.analysis,OutputAnalysisRequest{format:"cshell.output-analysis-request".into(),version:1,kind:OutputAnalysisKind::Mixed,run_id:key.run,
                            left:key.host,left_revision:left.run.revision,right:None,right_revision:None,step:key.step,stderr:key.stderr},*plan.fingerprint(),hosts)
                            &&let Some(target)=page.targets.iter().find(|target|target.run.host_id==host){
                            self.read_output(connection,view,MixedOutputKey{host,offset,child:target.children[key.step as usize],revision:target.run.revision,..key});
                        }
                        self.search_output(ui,connection,view,plan,left,key);
                    }
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(!view.busy, egui::Button::new("读取 / 刷新"))
                            .clicked()
                        {
                            self.read_output(connection, view, key);
                        }
                        if ui
                            .add_enabled(
                                !view.busy && key.offset > 0,
                                egui::Button::new("上一页输出"),
                            )
                            .clicked()
                        {
                            self.read_output(
                                connection,
                                view,
                                MixedOutputKey {
                                    offset: key.offset.saturating_sub(64 * 1024),
                                    ..key
                                },
                            );
                        }
                        if let Some(output) = &view.output
                            && output.key == key
                        {
                            ui.label(format!(
                                "{}..{} / {} 字节 · 目标版本 {}",
                                output.context.offset,
                                output.context.next_offset,
                                output.context.length,
                                output.context.target_revision
                            ));
                            if ui
                                .add_enabled(
                                    !view.busy
                                        && output.context.next_offset < output.context.length,
                                    egui::Button::new("下一页输出"),
                                )
                                .clicked()
                            {
                                self.read_output(
                                    connection,
                                    view,
                                    MixedOutputKey {
                                        offset: output.context.next_offset,
                                        ..key
                                    },
                                );
                            }
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("向上").clicked() {
                            self.scroll(-20, 40);
                        }
                        if ui.button("向下").clicked() {
                            self.scroll(20, 40);
                        }
                    });
                } else {
                    ui.label("选择当前目标的已派发命令步骤，读取 stdout 或 stderr。");
                }
                ui.allocate_rect(ui.available_rect_before_wrap(), egui::Sense::hover())
                    .rect
            })
            .inner
    }
    fn search_output(
        &mut self,
        ui: &mut egui::Ui,
        connection: &MixedConnection,
        view: &MixedView,
        plan: &MixedTaskSnapshot,
        left: &MixedTargetView,
        key: MixedOutputKey,
    ) {
        if left.run.state==MixedTaskState::Running||!left.run.steps.get(key.step as usize).is_some_and(|step|matches!(&step.receipt,MixedReceipt::Command{target,..} if target_terminal(target.state)&&target_terminal(target.steps[0].state))){return;}
        let scope = OutputAnalysisRequest {
            format: "cshell.output-analysis-request".into(),
            version: 1,
            kind: OutputAnalysisKind::Mixed,
            run_id: key.run,
            left: key.host,
            left_revision: left.run.revision,
            right: None,
            right_revision: None,
            step: key.step,
            stderr: key.stderr,
        };
        let child = left.children[key.step as usize];
        if let Some(offset) = self.search_panel.draw(
            ui,
            &connection.search,
            scope.clone(),
            *plan.fingerprint(),
            child,
        ) {
            self.read_output(
                connection,
                view,
                MixedOutputKey {
                    offset,
                    child,
                    revision: left.run.revision,
                    ..key
                },
            );
        }
        let mut merged_scope = scope;
        merged_scope.stderr = false;
        if let Some((stderr, offset)) = self.merge_panel.draw(
            ui,
            &connection.merge,
            merged_scope.clone(),
            *plan.fingerprint(),
            child,
        ) {
            self.read_output(
                connection,
                view,
                MixedOutputKey {
                    stderr,
                    offset,
                    child,
                    revision: left.run.revision,
                    ..key
                },
            );
        }
        self.archive_panel.draw(
            ui,
            &connection.archives,
            merged_scope,
            *plan.fingerprint(),
            child,
        );
    }
    fn builder(
        &mut self,
        ui: &mut egui::Ui,
        connection: &MixedConnection,
        view: &MixedView,
        hosts: Option<&HostGroupView>,
    ) {
        ui.add(
            egui::TextEdit::singleline(&mut self.name)
                .char_limit(128)
                .hint_text("计划名称"),
        );
        ui.horizontal_wrapped(|ui| {
            ui.label("主机并发");
            ui.add(egui::DragValue::new(&mut self.policy.concurrency).range(1..=50));
        });
        milliseconds(ui, "主机排队 ms", &mut self.policy.queue_timeout_ms);
        milliseconds(ui, "运行总期限 ms", &mut self.policy.total_timeout_ms);
        let mut enabled = self.policy.rollout.is_some();
        if ui
            .checkbox(&mut enabled, "按固定目标顺序分批 / Canary")
            .changed()
        {
            self.policy.rollout = enabled.then(MixedRolloutPolicy::default);
            self.create_confirm = false;
        }
        if let Some(policy) = &mut self.policy.rollout {
            ui.horizontal_wrapped(|ui| {
                ui.label("每批目标");
                ui.add(egui::DragValue::new(&mut policy.batch_size).range(1..=1000));
                ui.label("Canary 目标");
                ui.add(egui::DragValue::new(&mut policy.canary_count).range(0..=1000));
            });
            if policy.canary_count == 0 {
                policy.approve_after_canary = false;
            }
            ui.add_enabled(
                policy.canary_count != 0,
                egui::Checkbox::new(
                    &mut policy.approve_after_canary,
                    "Canary 全部成功后需要批准",
                ),
            );
            ui.checkbox(&mut policy.pause_between_batches, "每批结束后需要批准");
            milliseconds(ui, "批间延迟 ms", &mut policy.batch_delay_ms);
            let mut limit = policy.failure_limit.is_some();
            if ui.checkbox(&mut limit, "达到失败阈值后停止派发").changed() {
                policy.failure_limit = limit.then_some(1);
            }
            if let Some(value) = &mut policy.failure_limit {
                ui.add(egui::DragValue::new(value).range(1..=1000));
            }
            ui.label("按冻结 Host 标识排序分批，Canary 必须全成功；总期限包含批间和审批等待，批准后另须确认继续。");
        }
        ui.label("每个主机依次执行步骤；只有保存的成功回执允许进入下一步。");
        let mut remove = None;
        let mut shift = None;
        for (index, step) in self.steps.iter_mut().enumerate() {
            ui.push_id(step.key, |ui| {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "步骤 {} · {}",
                        index + 1,
                        action_name(&step.action)
                    ));
                    if ui
                        .add_enabled(index > 0, egui::Button::new("上移"))
                        .clicked()
                    {
                        shift = Some((index, index - 1));
                    }
                    if ui.button("下移").clicked() {
                        shift = Some((index, index + 1));
                    }
                    if ui.button("删除").clicked() {
                        remove = Some(index);
                    }
                });
                match &mut step.action {
                    MixedTaskAction::Command { .. } => {
                        if let Some(editor) = &mut step.editor {
                            editor.draw(ui, true, false);
                        } else {
                            ui.colored_label(cshell_ui::theme::WARNING, "缺少命令步骤草稿");
                        }
                    }
                    MixedTaskAction::Upload { definition } => {
                        crate::task_definition_editor::draw_publication_policy(
                            ui,
                            &mut definition.publication_policy,
                        );
                        let before = definition.clone();
                        ui.add(egui::TextEdit::singleline(&mut definition.name).char_limit(128));
                        ui.label("本机绝对源路径");
                        ui.add(
                            egui::TextEdit::singleline(&mut definition.local_source)
                                .char_limit(4096),
                        );
                        ui.label("远端完整目的路径（新文件）");
                        ui.add(
                            egui::TextEdit::singleline(&mut definition.remote_destination)
                                .char_limit(4000),
                        );
                        ui.collapsing("步骤期限", |ui| {
                            milliseconds(ui, "排队 ms", &mut definition.queue_timeout_ms);
                            milliseconds(ui, "连接 ms", &mut definition.connect_timeout_ms);
                            milliseconds(ui, "传输 ms", &mut definition.transfer_timeout_ms);
                        });
                        if before != *definition && view.hashing == Some(step.key) {
                            connection.cancel_hash();
                        }
                        if ui
                            .add_enabled(!view.busy, egui::Button::new("生成源大小与 SHA-256"))
                            .clicked()
                        {
                            let _queued = connection.hash(step.key, definition.clone());
                        }
                        if view.hashing == Some(step.key) && ui.button("取消摘要计算").clicked()
                        {
                            connection.cancel_hash();
                        }
                        if let Some(saved) = view
                            .manifests
                            .get(&step.key)
                            .filter(|saved| manifest_matches(saved, definition))
                        {
                            ui.label(format!(
                                "{} 字节 · SHA-256 {}",
                                saved.size,
                                hex(saved.sha256)
                            ));
                        } else {
                            ui.label("保存前先生成当前源文件摘要");
                        }
                    }
                    MixedTaskAction::Download { definition } => {
                        crate::task_definition_editor::draw_publication_policy(
                            ui,
                            &mut definition.publication_policy,
                        );
                        ui.add(egui::TextEdit::singleline(&mut definition.name).char_limit(128));
                        ui.label("远端绝对源路径");
                        ui.add(
                            egui::TextEdit::singleline(&mut definition.remote_source)
                                .char_limit(4000),
                        );
                        ui.label("本机既有目录");
                        ui.add(
                            egui::TextEdit::singleline(&mut definition.local_directory)
                                .char_limit(3500),
                        );
                        ui.label("文件名（主机标识自动作为前缀）");
                        ui.add(
                            egui::TextEdit::singleline(&mut definition.local_name).char_limit(128),
                        );
                        ui.horizontal_wrapped(|ui| {
                            ui.label("单文件上限字节");
                            ui.add(
                                egui::DragValue::new(&mut definition.maximum_bytes)
                                    .range(0..=MAX_DOWNLOAD_FILE_BYTES),
                            );
                        });
                        ui.collapsing("步骤期限", |ui| {
                            milliseconds(ui, "排队 ms", &mut definition.queue_timeout_ms);
                            milliseconds(ui, "连接 ms", &mut definition.connect_timeout_ms);
                            milliseconds(ui, "传输 ms", &mut definition.transfer_timeout_ms);
                        });
                    }
                }
            });
        }
        if let Some(index) = remove {
            if view.hashing == Some(self.steps[index].key) {
                connection.cancel_hash();
            }
            self.steps.remove(index);
        } else if let Some((left, right)) = shift
            && right < self.steps.len()
        {
            self.steps.swap(left, right);
        }
        ui.horizontal_wrapped(|ui| {
            for (kind, label) in [(0, "添加命令"), (1, "添加上传"), (2, "添加下载")] {
                if ui
                    .add_enabled(self.steps.len() < MAX_TASK_STEPS, egui::Button::new(label))
                    .clicked()
                    && let Some(step) = new_step(kind)
                {
                    self.steps.push(step);
                }
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
                egui::ComboBox::from_id_salt("mixed-host-group")
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
                    "请从主机面板采用选择"
                });
            }
        }
        let selection = self.selection();
        let revisions = snapshot.map(|snapshot| (snapshot.revision, snapshot.profile_revision));
        let preview_key = serde_json::to_vec(&(&selection, revisions)).unwrap_or_default();
        if self.preview_key != preview_key {
            self.preview_key = preview_key;
            self.preview.clear();
            self.preview_error.clear();
            match (snapshot, selection.as_ref()) {
                (Some(snapshot), Some(selection)) => {
                    match select_hosts(&snapshot.catalog, selection, MAX_HOST_TARGETS) {
                        Ok(hosts) => self.preview = hosts,
                        Err(error) => self.preview_error = error.to_string(),
                    }
                }
                _ => self.preview_error = "请加载主机库并选择目标".into(),
            }
        }
        let definition = self.definition(view);
        let actions: Vec<_> = self
            .steps
            .iter()
            .map(|step| {
                (
                    step.key,
                    &step.action,
                    step.editor.as_ref().map(|editor| editor.revision()),
                )
            })
            .collect();
        let key = serde_json::to_vec(&(
            &self.name,
            &self.policy,
            actions,
            &selection,
            revisions,
            definition.as_ref().ok(),
            hosts.is_some_and(|host| host.writable && !host.busy),
        ))
        .unwrap_or_default();
        if self.builder_key != key {
            self.create_confirm = false;
            self.builder_key = key;
        }
        if let Err(error) = &definition {
            ui.colored_label(cshell_ui::theme::WARNING, error);
        }
        if !self.preview_error.is_empty() {
            ui.colored_label(cshell_ui::theme::WARNING, &self.preview_error);
        }
        ui.label(format!(
            "匹配 {} 个目标 · 主机/Profile 版本 {:?}",
            self.preview.len(),
            revisions
        ));
        egui::ScrollArea::vertical()
            .id_salt("mixed-target-preview")
            .max_height(140.0)
            .show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body) + 6.0,
                self.preview.len(),
                |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for host in &self.preview[range] {
                        ui.add(
                            egui::Label::new(format!(
                                "{} · {}:{} · {}",
                                host.name, host.address, host.port, host.id
                            ))
                            .truncate(),
                        );
                    }
                },
            );
        ui.checkbox(
            &mut self.create_confirm,
            "确认步骤顺序、内容、文件路径、策略及匹配目标",
        );
        if ui
            .add_enabled(
                view.available
                    && view.writes_allowed
                    && !view.busy
                    && !view.cancel_busy
                    && self.create_confirm
                    && definition.is_ok()
                    && !self.preview.is_empty()
                    && self.preview_error.is_empty()
                    && hosts.is_some_and(|host| host.writable && !host.busy),
                egui::Button::new("保存冻结混合计划"),
            )
            .clicked()
            && let (Ok(definition), Some(selection), Some((hosts, profiles))) =
                (definition, selection, revisions)
        {
            let expected: MixedExpectedTargets = self
                .preview
                .iter()
                .map(|host| {
                    (
                        host.id,
                        (host.name.clone(), host.address.clone(), host.port),
                    )
                })
                .collect();
            if connection.create(definition, selection, expected, hosts, profiles) {
                self.create_confirm = false;
                self.status.clear();
            }
        }
    }
    fn review(
        &mut self,
        ui: &mut egui::Ui,
        connection: &MixedConnection,
        view: &MixedView,
        plan: &MixedTaskSnapshot,
        page: &MixedStatusPage,
    ) {
        ui.separator();
        ui.heading(&plan.definition().name);
        ui.label(plan.id().to_string());
        ui.label(format!(
            "{} 个冻结目标 · {} 个步骤 · 主机/Profile 版本 {} / {}",
            page.total_targets,
            page.steps.len(),
            page.host_revision,
            page.profile_revision
        ));
        ui.label(format!(
            "主机并发 {} · 排队 {} ms · 总期限 {} ms",
            page.policy.concurrency, page.policy.queue_timeout_ms, page.policy.total_timeout_ms
        ));
        ui.label(format!("快照 SHA-256 {}", hex(page.snapshot_sha256)));
        ui.collapsing("核对全部冻结目标与步骤", |ui| {
            for (index, action) in plan.definition().steps.iter().enumerate() {
                ui.separator();
                ui.label(format!(
                    "{}. {} · {}",
                    index + 1,
                    action_name(action),
                    action.name()
                ));
                match action {
                    MixedTaskAction::Command { definition } => {
                        ui.label(format!(
                            "方言 {:?} · 成功码 {:?} · 幂等 {}",
                            definition.shell,
                            definition.steps[0].success_codes,
                            definition.steps[0].idempotent
                        ));
                        let mut body = serde_json::to_string_pretty(&definition.steps[0].command)
                            .unwrap_or_default();
                        ui.add(
                            egui::TextEdit::multiline(&mut body)
                                .interactive(false)
                                .desired_rows(4),
                        );
                        if !definition.variables.is_empty() || !definition.environment.is_empty() {
                            ui.collapsing("变量与环境", |ui| {
                                for (name, value) in &definition.variables {
                                    ui.label(format!("变量 {name} = {value}"));
                                }
                                for (name, value) in &definition.environment {
                                    ui.label(format!("环境 {name} = {value}"));
                                }
                            });
                        }
                        ui.label(format!(
                            "排队/连接/命令/步骤总期限：{} / {} / {} / {} ms",
                            definition.policy.queue_timeout_ms,
                            definition.policy.connect_timeout_ms,
                            definition.policy.step_timeout_ms,
                            definition.policy.total_timeout_ms
                        ));
                    }
                    MixedTaskAction::Upload { definition } => {
                        ui.label(format!("本机源：{}", definition.local_source));
                        ui.label(format!("远端目的：{}", definition.remote_destination));
                        ui.label(format!(
                            "文件策略：{:?}（None 为不覆盖）",
                            definition.publication_policy
                        ));
                        ui.label(format!(
                            "{} 字节 · SHA-256 {}",
                            definition.size,
                            hex(definition.sha256)
                        ));
                        ui.label(format!(
                            "排队/连接/传输：{} / {} / {} ms",
                            definition.queue_timeout_ms,
                            definition.connect_timeout_ms,
                            definition.transfer_timeout_ms
                        ));
                    }
                    MixedTaskAction::Download { definition } => {
                        ui.label(format!("远端源：{}", definition.remote_source));
                        ui.label(format!(
                            "文件策略：{:?}（None 为不覆盖）",
                            definition.publication_policy
                        ));
                        ui.label(format!(
                            "本机目录：{} · 文件名：主机标识-{}",
                            definition.local_directory, definition.local_name
                        ));
                        ui.label(format!(
                            "单文件上限 {} 字节 · 源摘要按主机读取后固定",
                            definition.maximum_bytes
                        ));
                        ui.label(format!(
                            "排队/连接/传输：{} / {} / {} ms",
                            definition.queue_timeout_ms,
                            definition.connect_timeout_ms,
                            definition.transfer_timeout_ms
                        ));
                    }
                }
            }
            egui::ScrollArea::vertical()
                .id_salt("mixed-frozen-hosts")
                .max_height(170.0)
                .show_rows(
                    ui,
                    ui.text_style_height(&egui::TextStyle::Body) + 6.0,
                    plan.targets().targets().len(),
                    |ui, range| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        for host in &plan.targets().targets()[range] {
                            let text = format!(
                                "{} · {}:{} · {}",
                                host.name, host.address, host.port, host.id
                            );
                            ui.add(egui::Label::new(&text).truncate())
                                .on_hover_text(text);
                        }
                    },
                );
        });
        let summary = [
            "待执行",
            "运行中",
            "已完成",
            "失败",
            "已取消",
            "结果不明",
            "超时",
        ]
        .into_iter()
        .zip(page.counts)
        .map(|(label, count)| format!("{label} {count}"))
        .collect::<Vec<_>>()
        .join(" · ");
        ui.label(summary);
        let revision = view
            .rollout
            .as_ref()
            .map(|document| document.progress.revision);
        if revision != self.rollout_revision {
            self.rollout_revision = revision;
            self.gate_confirm = false;
            self.start_confirm = false;
        }
        if let Some(control) = &view.rollout {
            let progress = &control.progress;
            ui.label(format!(
                "调度 {:?} · 元数据版本 {} · 下一批 {} · 总期限 {:?} · 原因 {:?}",
                progress.stage,
                progress.revision,
                progress.next_batch + 1,
                progress.deadline_ms,
                progress.reason
            ));
            ui.label(format!(
                "每批 {} · Canary {} · 批间 {} ms · 失败阈值 {:?}",
                control.policy.batch_size,
                control.policy.canary_count,
                control.policy.batch_delay_ms,
                control.policy.failure_limit
            ));
            if let Some(gate) = &progress.gate {
                ui.label(format!(
                    "待批准 {:?} · {} · 下一批 {}",
                    gate.kind,
                    gate.id,
                    gate.next_batch + 1
                ));
                ui.checkbox(
                    &mut self.gate_confirm,
                    "确认解除所列关卡；不直接执行后续目标",
                );
                if ui
                    .add_enabled(
                        self.gate_confirm
                            && !view.busy
                            && !view.cancel_busy
                            && view.available
                            && view.writes_allowed
                            && !control.active,
                        egui::Button::new("批准此关卡"),
                    )
                    .clicked()
                {
                    let _queued = connection.approve(plan.clone(), control.clone());
                    self.deactivate();
                }
            }
        }
        if ui
            .add_enabled(
                !view.busy
                    && !view.cancel_busy
                    && view.available
                    && !page.active
                    && page.counts[0] == 0
                    && page.counts[1] == 0,
                egui::Button::new("抓取全部目标结果"),
            )
            .clicked()
        {
            let _queued = connection.results(plan.clone());
        }
        self.results.draw(ui, connection, view);
        let mut sources = Vec::new();
        if !page.active {
            for row in &page.targets {
                if row.run.state == MixedTaskState::Running {
                    continue;
                }
                for (index, step) in row.run.steps.iter().enumerate() {
                    if let MixedReceipt::Command { target, .. } = &step.receipt {
                        if target_terminal(target.state) && target_terminal(target.steps[0].state) {
                            for stderr in [false, true] {
                                sources.push(OutputAnalysisRequest {
                                    format: "cshell.output-analysis-request".into(),
                                    version: 1,
                                    kind: OutputAnalysisKind::Mixed,
                                    run_id: plan.id(),
                                    left: row.run.host_id,
                                    left_revision: row.run.revision,
                                    right: None,
                                    right_revision: None,
                                    step: index as u32,
                                    stderr,
                                });
                            }
                        }
                    }
                }
            }
        }
        self.workspace_panel
            .draw(ui, &connection.workspace, sources);
        self.retry_panel.draw(ui, connection, view, plan, page);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !view.busy && !view.cancel_busy,
                    egui::Button::new("重新载入保存状态"),
                )
                .clicked()
            {
                let _queued = connection.page(plan.clone(), page.after, true);
                self.deactivate();
            }
            if ui
                .add_enabled(!view.busy, egui::Button::new("重新核对计划"))
                .clicked()
            {
                self.inspect(connection, plan.id());
            }
            if ui
                .add_enabled(
                    !view.cancel_busy && (page.active || !page.pending.is_empty()),
                    egui::Button::new("取消混合任务"),
                )
                .clicked()
            {
                if connection.cancel(plan.id()) {
                    self.read_requested = true;
                    self.deactivate();
                } else {
                    self.status = "状态变化或取消正在处理，请查询保存状态".into();
                }
            }
        });
        if !page.pending.is_empty() {
            ui.checkbox(
                &mut self.start_confirm,
                format!(
                    "确认执行全部 {} 个待执行目标；不受当前页筛选影响",
                    page.pending.len()
                ),
            );
            if ui
                .add_enabled(
                    view.available
                        && view.writes_allowed
                        && !view.busy
                        && !view.cancel_busy
                        && !page.active
                        && self.start_confirm
                        && page.confirmation_bound(view.rollout.as_deref()).is_ok(),
                    egui::Button::new("启动 / 继续待执行目标"),
                )
                .clicked()
            {
                if let Ok(confirmation) = page.confirmation_bound(view.rollout.as_deref())
                    && connection.start_run(plan.clone(), confirmation, page.after)
                {
                    self.read_requested = true;
                    self.status.clear();
                } else {
                    self.status = "待执行集合已变化，请重新载入后核对".into();
                }
                self.start_confirm = false;
            }
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !view.busy && page.after.is_some(),
                    egui::Button::new("目标首页"),
                )
                .clicked()
            {
                let _queued = connection.page(plan.clone(), None, false);
            }
            if let Some(next) = page.next
                && ui
                    .add_enabled(!view.busy, egui::Button::new("下一页目标"))
                    .clicked()
            {
                let _queued = connection.page(plan.clone(), Some(next), false);
            }
        });
        ui.label(format!(
            "当前页 {} 个目标；每页独立查询，筛选只作用于当前页。",
            page.targets.len()
        ));
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .char_limit(256)
                    .hint_text("名称、地址或主机标识"),
            );
            egui::ComboBox::from_id_salt("mixed-state-filter")
                .truncate()
                .selected_text(self.state.map_or("全部状态", state_name))
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut self.state, None, "全部状态");
                    for state in [
                        MixedTaskState::Pending,
                        MixedTaskState::Running,
                        MixedTaskState::Succeeded,
                        MixedTaskState::Failed,
                        MixedTaskState::Cancelled,
                        MixedTaskState::Unknown,
                        MixedTaskState::TimedOut,
                    ] {
                        ui.selectable_value(&mut self.state, Some(state), state_name(state));
                    }
                });
        });
        let filter = self.filter.to_lowercase();
        let rows: Vec<_> = page
            .targets
            .iter()
            .filter(|target| {
                self.state.is_none_or(|state| state == target.run.state)
                    && (filter.is_empty()
                        || target.name.to_lowercase().contains(&filter)
                        || target.address.to_lowercase().contains(&filter)
                        || target.run.host_id.to_string().contains(&filter))
            })
            .collect();
        egui::ScrollArea::vertical()
            .id_salt("mixed-current-targets")
            .max_height(220.0)
            .show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body) + 10.0,
                rows.len(),
                |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for target in &rows[range] {
                        if ui
                            .add(
                                egui::Button::selectable(
                                    self.selected == Some(target.run.host_id),
                                    format!(
                                        "{} · {}:{} · {} · 完成步骤 {} / {} · v{}",
                                        target.name,
                                        target.address,
                                        target.port,
                                        state_name(target.run.state),
                                        target.run.next_step,
                                        target.run.steps.len(),
                                        target.run.revision
                                    ),
                                )
                                .truncate(),
                            )
                            .clicked()
                        {
                            if self.selected != Some(target.run.host_id) {
                                self.repair_confirm = false;
                                self.clear_output();
                            }
                            self.selected = Some(target.run.host_id);
                        }
                    }
                },
            );
        if ui
            .add_enabled(!view.busy, egui::Button::new("加载文件修复状态"))
            .clicked()
        {
            let _queued = connection.repair_status(plan.clone());
        }
        if let Some(document) = &view.repair
            && let Some(job) = &document.job
        {
            ui.label(format!(
                "文件修复 {:?} · {:?} · 主机 {} · 步骤 {} · 起始版本 {}",
                job.operation,
                job.status,
                job.host_id,
                job.step + 1,
                job.parent_revision
            ));
            if let Some(error) = job.error {
                ui.label(format!("修复原因：{error:?}"));
            }
            if let Some(probe) = job.probe {
                ui.label(format!("探测临时文件：{}", probe_content(probe.temporary)));
                ui.label(format!(
                    "探测目的文件：{}",
                    probe_content(probe.destination)
                ));
            }
            ui.label("操作结束不等于步骤成功；步骤结果以保存回执为准。");
        }
        if let Some(target) = self
            .selected
            .and_then(|id| page.targets.iter().find(|target| target.run.host_id == id))
        {
            ui.separator();
            ui.label(format!(
                "{} · {} · {}",
                target.name,
                target.run.host_id,
                state_name(target.run.state)
            ));
            if let Some(reason) = target.run.reason {
                ui.label(format!("原因：{reason:?}"));
            }
            for (index, step) in target.run.steps.iter().enumerate() {
                ui.separator();
                ui.label(format!(
                    "{}. {} · {}",
                    index + 1,
                    page.steps[index].name,
                    state_name(step.state)
                ));
                match &step.receipt {
                    MixedReceipt::Command {
                        meta,
                        target: command,
                    } => {
                        ui.label(format!(
                            "命令状态 {:?} · 退出码 {:?} · 原因 {:?}",
                            command.state, command.steps[0].exit_code, command.reason
                        ));
                        let readable = step.state != MixedTaskState::Pending
                            && command.steps[0].state != TargetState::Pending;
                        ui.horizontal_wrapped(|ui| {
                            for stderr in [false, true] {
                                if ui
                                    .add_enabled(
                                        !view.busy && readable,
                                        egui::Button::new(if stderr {
                                            "读取 stderr"
                                        } else {
                                            "读取 stdout"
                                        }),
                                    )
                                    .clicked()
                                {
                                    self.read_output(
                                        connection,
                                        view,
                                        MixedOutputKey {
                                            run: plan.id(),
                                            host: target.run.host_id,
                                            step: index as u32,
                                            child: target.children[index],
                                            revision: if page.active {
                                                0
                                            } else {
                                                target.run.revision
                                            },
                                            stderr,
                                            offset: 0,
                                        },
                                    );
                                }
                            }
                        });
                        ui.label(format!("命令账本版本 {}", meta.revision));
                    }
                    MixedReceipt::Upload { row } => {
                        crate::task_definition_editor::show_publication_receipt(
                            ui,
                            row.publication,
                        );
                        ui.label(format!(
                            "上传阶段 {:?} · 已记录 {} 字节 · 原因 {:?}",
                            row.phase, row.bytes, row.reason
                        ));
                        receipt_flags(
                            ui,
                            row.temporary_owned,
                            row.verified,
                            row.committed,
                            row.adopted,
                            row.temporary_cleaned,
                        );
                    }
                    MixedReceipt::Download { row } => {
                        crate::task_definition_editor::show_publication_receipt(
                            ui,
                            row.publication,
                        );
                        ui.label(format!(
                            "下载阶段 {:?} · 已记录 {} 字节 · 原因 {:?}",
                            row.phase, row.bytes, row.reason
                        ));
                        if let Some(content) = row.content {
                            ui.label(format!(
                                "固定源 {} 字节 · SHA-256 {}",
                                content.size,
                                hex(content.sha256)
                            ));
                        }
                        receipt_flags(
                            ui,
                            row.temporary_owned,
                            row.verified,
                            row.committed,
                            row.adopted,
                            row.temporary_cleaned,
                        );
                        if let MixedTaskAction::Download { definition } =
                            &plan.definition().steps[index]
                            && let Ok(path) = definition.destination(target.run.host_id)
                        {
                            ui.monospace(path.display().to_string());
                        }
                    }
                }
                if step.receipt.file_revision().is_some() {
                    self.file_repair_controls(ui, connection, view, plan, page, &target.run, index);
                }
            }
            if matches!(
                target.run.state,
                MixedTaskState::Failed
                    | MixedTaskState::Unknown
                    | MixedTaskState::TimedOut
                    | MixedTaskState::Cancelled
            ) {
                ui.label("此目标已停止；后续步骤保留为未开始，结果不明的操作不会自动重试。");
            }
        }
    }
    fn file_repair_controls(
        &mut self,
        ui: &mut egui::Ui,
        connection: &MixedConnection,
        view: &MixedView,
        plan: &MixedTaskSnapshot,
        page: &MixedStatusPage,
        target: &MixedTargetRun,
        step: usize,
    ) {
        let readonly = view.available && !view.busy && !view.cancel_busy && !page.active;
        if ui
            .add_enabled(
                readonly && target.file_repair_allowed(step, MixedFileOperation::Probe),
                egui::Button::new("只读探测此文件步骤"),
            )
            .clicked()
        {
            if connection.repair(
                plan.clone(),
                target.host_id,
                step,
                target.revision,
                MixedFileOperation::Probe,
                page.after,
            ) {
                self.read_requested = true;
                self.deactivate();
            }
        }
        if step != target.next_step
            || !matches!(
                target.state,
                MixedTaskState::Failed
                    | MixedTaskState::Unknown
                    | MixedTaskState::Cancelled
                    | MixedTaskState::TimedOut
            )
        {
            return;
        }
        let previous = self.repair_choice;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.repair_choice, MixedFileOperation::Resume, "续传");
            ui.radio_value(
                &mut self.repair_choice,
                MixedFileOperation::Cleanup,
                "清理临时文件",
            );
            ui.radio_value(
                &mut self.repair_choice,
                MixedFileOperation::Adopt,
                "认领目的文件",
            );
        });
        if previous != self.repair_choice {
            self.repair_confirm = false;
        }
        let allowed = target.file_repair_allowed(step, self.repair_choice)
            && (self.repair_choice != MixedFileOperation::Adopt
                || view
                    .repair
                    .as_ref()
                    .is_some_and(|document| document.matching_destination(plan, target, step)));
        let label = match self.repair_choice {
            MixedFileOperation::Resume => "确认核对原源摘要和完整前缀后续传",
            MixedFileOperation::Cleanup => "确认只删除此步骤拥有的临时文件，保留最终文件",
            MixedFileOperation::Adopt => "确认认领内容一致的目的文件；这不能证明原发布归属",
            _ => "确认文件操作",
        };
        ui.checkbox(&mut self.repair_confirm, label);
        if !allowed {
            ui.label("当前回执不满足操作条件；认领需要当前版本的已完成内容探测。");
        }
        if ui
            .add_enabled(
                readonly && view.writes_allowed && allowed && self.repair_confirm,
                egui::Button::new("执行此步骤修复"),
            )
            .clicked()
        {
            if connection.repair(
                plan.clone(),
                target.host_id,
                step,
                target.revision,
                self.repair_choice,
                page.after,
            ) {
                self.read_requested = true;
                self.status.clear();
            } else {
                self.status = "修复对象或保存版本已变化，请重新载入".into();
            }
            self.repair_confirm = false;
            self.start_confirm = false;
        }
        ui.label("修复成功后仍需核对待执行集合并显式继续；后续命令不会自动开始。");
    }
    fn inspect(&mut self, connection: &MixedConnection, id: TaskRunId) {
        if connection.inspect(id) {
            self.deactivate();
            self.status.clear();
            self.read_requested = false;
            self.refresh_at = None;
            self.clear_output();
        }
    }
    fn read_output(
        &mut self,
        connection: &MixedConnection,
        view: &MixedView,
        mut key: MixedOutputKey,
    ) {
        let Some(plan) = &view.plan else {
            return;
        };
        if plan.id() != key.run {
            return;
        }
        if let Some(page) = &view.page
            && let Some(target) = page
                .targets
                .iter()
                .find(|target| target.run.host_id == key.host)
        {
            key.revision = if page.active { 0 } else { target.run.revision };
        }
        if connection.output(plan.clone(), key) {
            self.output_key = Some(key);
            self.output_stamp = None;
            self.surface = None;
            self.status.clear();
        }
    }
    fn clear_output(&mut self) {
        self.merge_panel.reset();
        self.output_key = None;
        self.output_stamp = None;
        self.surface = None;
    }
    fn update_output(&mut self, view: &MixedView) {
        if let Some(output) = &view.output
            && self.output_key == Some(output.key)
            && self.output_stamp != Some(output.page.revision)
        {
            let mut surface = LogSurfaceModel::default();
            if surface.submit_page(output.page.clone()).is_ok() {
                surface.anchor(1, 0);
                self.surface = Some(surface);
                self.output_stamp = Some(output.page.revision);
            } else {
                self.status = "输出页无法绘制，请重新读取".into();
            }
        }
        if self.surface.is_some() && self.reflow.is_none() {
            self.reflow = TaskReflowWorker::start().ok();
        }
        self.reflow_output(self.columns);
    }
    fn reflow_output(&mut self, columns: u16) {
        self.columns = columns.max(1);
        if let Some(worker) = &self.reflow
            && let Some(result) = worker.take_result()
            && let Some(surface) = &mut self.surface
        {
            match result {
                Ok(layout) => {
                    surface.submit_reflow(layout);
                }
                Err(_) => surface.reflow_failed(),
            }
        }
        if let Some(surface) = &mut self.surface
            && let Some(request) = surface.request_reflow(self.columns)
            && self
                .reflow
                .as_ref()
                .is_none_or(|worker| !worker.submit(request))
        {
            surface.reflow_failed();
        }
    }
    pub fn frame(&mut self, columns: u16, rows: u16) -> Option<LogSurfaceFrame> {
        if self.merge_panel.active() {
            return self.merge_panel.frame(columns, rows);
        }
        self.reflow_output(columns);
        self.surface
            .as_ref()
            .and_then(|surface| surface.prepare_frame(rows))
    }
    pub fn scroll(&mut self, rows: i32, viewport: u16) {
        if self.merge_panel.active() {
            self.merge_panel.scroll(rows, viewport);
            return;
        }
        if let Some(surface) = &mut self.surface {
            surface.scroll_visual_rows(rows, viewport);
        }
    }
}
fn manifest_matches(left: &BatchUploadDefinition, right: &BatchUploadDefinition) -> bool {
    let mut left = left.clone();
    left.size = right.size;
    left.sha256 = right.sha256;
    left == *right
}
fn milliseconds(ui: &mut egui::Ui, label: &str, value: &mut u64) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(value).range(1..=604_800_000_u64));
    });
}
fn hex(value: [u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn probe_content(content: Option<UploadContent>) -> String {
    content.map_or("不存在".into(), |content| {
        format!("{} 字节 · SHA-256 {}", content.size, hex(content.sha256))
    })
}
fn action_name(action: &MixedTaskAction) -> &'static str {
    match action {
        MixedTaskAction::Command { .. } => "命令",
        MixedTaskAction::Upload { .. } => "上传",
        MixedTaskAction::Download { .. } => "下载",
    }
}
fn state_name(state: MixedTaskState) -> &'static str {
    match state {
        MixedTaskState::Pending => "未开始",
        MixedTaskState::Running => "执行中",
        MixedTaskState::Succeeded => "已完成",
        MixedTaskState::Failed => "失败",
        MixedTaskState::Cancelled => "已取消",
        MixedTaskState::Unknown => "结果不明",
        MixedTaskState::TimedOut => "超时",
    }
}
fn receipt_flags(
    ui: &mut egui::Ui,
    owned: bool,
    verified: bool,
    committed: bool,
    adopted: bool,
    cleaned: bool,
) {
    ui.label(format!("临时归属 {owned} · 已校验 {verified} · 已提交 {committed} · 显式认领 {adopted} · 已清理 {cleaned}"));
}
