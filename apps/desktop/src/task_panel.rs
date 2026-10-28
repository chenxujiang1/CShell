use crate::{
    host_group_connection::HostGroupView,
    task_connection::{OutputKey, TaskConnection, TaskView},
};
use cshell_application::{
    HostSelector, MAX_HOST_SELECTOR_BYTES, OutputAnalysisKind, OutputAnalysisRequest,
    TaskResultDocument, TaskSnapshot, target_terminal, task_terminal,
};
use cshell_domain::*;
use cshell_render::{
    LogDecorations, LogReflowLayout, LogReflowRequest, LogSurfaceError, LogSurfaceFrame,
    LogSurfaceModel,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
struct ReflowState {
    request: Option<LogReflowRequest>,
    result: Option<Result<LogReflowLayout, LogSurfaceError>>,
    stopped: bool,
}
// One pending request and one completed layout; resizing replaces pending work.
pub(crate) struct TaskReflowWorker {
    state: Arc<(Mutex<ReflowState>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl TaskReflowWorker {
    pub(crate) fn start() -> std::io::Result<Self> {
        let state = Arc::new((Mutex::new(ReflowState::default()), Condvar::new()));
        let shared = state.clone();
        let worker = std::thread::Builder::new()
            .name("cshell-task-reflow".into())
            .spawn(move || {
                loop {
                    let request = {
                        let (mutex, ready) = &*shared;
                        let mut state = mutex
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while !state.stopped && state.request.is_none() {
                            state = ready
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        if state.stopped {
                            break;
                        }
                        state.request.take()
                    };
                    if let Some(request) = request {
                        let result = request.execute();
                        shared
                            .0
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .result = Some(result);
                    }
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }
    pub(crate) fn submit(&self, request: LogReflowRequest) -> bool {
        let mut state = self
            .state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopped {
            return false;
        }
        state.request = Some(request);
        self.state.1.notify_one();
        true
    }
    pub(crate) fn take_result(&self) -> Option<Result<LogReflowLayout, LogSurfaceError>> {
        self.state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .result
            .take()
    }
}
impl Drop for TaskReflowWorker {
    fn drop(&mut self) {
        {
            let mut state = self
                .state
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopped = true;
            state.request = None;
        }
        self.state.1.notify_one();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TaskPanelMode {
    Commands,
    Upload,
    Download,
    Mixed,
}
pub(crate) struct TaskPanel {
    pub(crate) open: bool,
    mode: TaskPanelMode,
    upload_panel: crate::upload_panel::UploadPanel,
    download_panel: crate::download_panel::DownloadPanel,
    mixed_panel: crate::mixed_panel::MixedPanel,
    editor: crate::task_definition_editor::TaskDefinitionEditor,
    template_open: bool,
    selection: Option<HostSelection>,
    query: String,
    group: Option<HostGroupId>,
    selection_mode: u8,
    stamp: Option<(TaskRunId, u64)>,
    start_confirm: bool,
    gate_confirm: bool,
    retry_confirm: bool,
    unknown_confirm: bool,
    retry_hosts: BTreeSet<HostId>,
    target_filter: String,
    results_panel: crate::task_results_panel::TaskResultsPanel,
    analysis_panel: crate::output_analysis_panel::OutputAnalysisPanel,
    search_panel: crate::output_search_panel::OutputSearchPanel,
    merge_panel: crate::output_merge_panel::OutputMergePanel,
    archive_panel: crate::output_archive_panel::OutputArchivePanel,
    workspace_panel: crate::output_workspace_panel::OutputWorkspacePanel,
    review_step: usize,
    output_key: Option<OutputKey>,
    comparison_key: Option<(OutputKey, HostId, u64)>,
    output_stamp: Option<u64>,
    output_surface: Option<LogSurfaceModel>,
    pub(crate) output_decorations: LogDecorations,
    output_reflow: Option<TaskReflowWorker>,
    output_columns: u16,
    refresh_at: Option<Instant>,
    status: String,
}
impl Default for TaskPanel {
    fn default() -> Self {
        Self {
            open: false,
            mode: TaskPanelMode::Commands,
            upload_panel: crate::upload_panel::UploadPanel::default(),
            download_panel: crate::download_panel::DownloadPanel::default(),
            mixed_panel: crate::mixed_panel::MixedPanel::default(),
            editor: Default::default(),
            template_open: false,
            selection: None,
            query: "all".into(),
            group: None,
            selection_mode: 0,
            stamp: None,
            start_confirm: false,
            gate_confirm: false,
            retry_confirm: false,
            unknown_confirm: false,
            retry_hosts: BTreeSet::new(),
            target_filter: String::new(),
            results_panel: crate::task_results_panel::TaskResultsPanel::default(),
            analysis_panel: Default::default(),
            search_panel: Default::default(),
            merge_panel: Default::default(),
            archive_panel: Default::default(),
            workspace_panel: Default::default(),
            review_step: 0,
            output_key: None,
            comparison_key: None,
            output_stamp: None,
            output_surface: None,
            output_decorations: LogDecorations::default(),
            output_reflow: None,
            output_columns: 120,
            refresh_at: None,
            status: String::new(),
        }
    }
}
impl TaskPanel {
    pub(crate) fn open_commands(&mut self) {
        self.switch_mode(TaskPanelMode::Commands);
    }
    pub(crate) fn open_upload(&mut self) {
        self.switch_mode(TaskPanelMode::Upload);
    }
    pub(crate) fn open_download(&mut self) {
        self.switch_mode(TaskPanelMode::Download);
    }
    pub(crate) fn open_mixed(&mut self) {
        self.switch_mode(TaskPanelMode::Mixed);
    }
    fn switch_mode(&mut self, mode: TaskPanelMode) {
        if !self.open || self.mode != mode {
            self.clear_confirmations();
            self.upload_panel.deactivate();
            self.download_panel.deactivate();
            self.mixed_panel.deactivate();
            self.output_decorations = LogDecorations::default();
        }
        self.open = true;
        self.mode = mode;
    }
    pub(crate) fn close(&mut self) {
        self.open = false;
        self.clear_confirmations();
        self.upload_panel.deactivate();
        self.download_panel.deactivate();
        self.mixed_panel.deactivate();
    }
    pub(crate) fn use_upload_selection(&mut self, selection: HostSelection) {
        self.upload_panel.use_selection(selection);
        self.open_upload();
    }
    pub(crate) fn use_download_selection(&mut self, selection: HostSelection) {
        self.download_panel.use_selection(selection);
        self.open_download();
    }
    pub(crate) fn use_mixed_selection(&mut self, selection: HostSelection) {
        self.mixed_panel.use_selection(selection);
        self.open_mixed();
    }
    pub(crate) fn use_selection(&mut self, selection: HostSelection) {
        self.open_commands();
        self.selection = Some(selection);
        self.selection_mode = 2;
        self.open = true;
    }
    pub(crate) fn use_template(
        &mut self,
        mut definition: TaskDefinition,
        selection: HostSelection,
    ) {
        self.clear_confirmations();
        self.clear_output();
        definition.id = TaskId::new();
        self.editor =
            crate::task_definition_editor::TaskDefinitionEditor::from_definition(&definition);
        self.template_open = true;
        self.use_selection(selection);
        self.status = "已载入快捷任务草稿，保存计划后核对冻结目标".into();
    }
    fn definition(&self) -> Result<TaskDefinition, String> {
        let mut definition = self.editor.definition(false)?;
        definition.id = TaskId::new();
        Ok(definition)
    }
    fn current_selection(&self) -> Option<HostSelection> {
        match self.selection_mode {
            0 => Some(HostSelection::Selector {
                expression: self.query.clone(),
            }),
            1 => self.group.map(|id| HostSelection::Group { id }),
            _ => self.selection.clone(),
        }
    }
    fn clear_confirmations(&mut self) {
        self.workspace_panel.deactivate();
        self.archive_panel.deactivate();
        self.search_panel.deactivate();
        self.start_confirm = false;
        self.gate_confirm = false;
        self.retry_confirm = false;
        self.unknown_confirm = false;
    }
    pub(crate) fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &TaskConnection,
        view: &TaskView,
        hosts: Option<&HostGroupView>,
    ) -> egui::Rect {
        if self.mode == TaskPanelMode::Mixed {
            return self.mixed_panel.draw(
                ui,
                &connection.mixed,
                &connection.mixed.view(),
                hosts,
                &mut self.open,
            );
        }
        if self.mode == TaskPanelMode::Download {
            return self.download_panel.draw(
                ui,
                &connection.downloads,
                &connection.downloads.view(),
                hosts,
                &mut self.open,
            );
        }
        if self.mode == TaskPanelMode::Upload {
            return self.upload_panel.draw(
                ui,
                &connection.uploads,
                &connection.uploads.view(),
                hosts,
                &mut self.open,
            );
        }
        let stamp = view
            .detail
            .as_ref()
            .map(|detail| (detail.id, detail.meta.revision));
        if self.stamp != stamp {
            self.clear_confirmations();
            if self.stamp.map(|stamp| stamp.0) != stamp.map(|stamp| stamp.0) {
                self.retry_hosts.clear();
                self.clear_output();
                self.review_step = 0;
            }
            self.stamp = stamp;
        }
        self.update_output(view);
        egui::Panel::left("task-controls")
            .default_size(380.0)
            .size_range(300.0..=520.0)
            .resizable(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("task-controls-scroll")
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.heading("批量任务");
                            if ui.button("返回终端").clicked() {
                                self.open = false;
                                self.clear_confirmations();
                            }
                        });
                        ui.label(&view.status);
                        if !self.status.is_empty() {
                            ui.colored_label(cshell_ui::theme::WARNING, &self.status);
                        }
                        egui::CollapsingHeader::new("创建任务")
                            .open(self.template_open.then_some(true))
                            .show(ui, |ui| self.draw_builder(ui, connection, view, hosts));
                        self.template_open = false;
                        ui.separator();
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
                                    egui::Button::new("下一页"),
                                )
                                .clicked()
                            {
                                let _queued = connection.list(view.rows.last().map(|row| row.id));
                            }
                        });
                        if view.after.is_some() {
                            ui.label("按任务标识分页；重新加载返回第一页");
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("task-list")
                            .max_height(170.0)
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
                                                    "{} · {} · {}",
                                                    row.name,
                                                    task_state(row.meta.state),
                                                    row.id
                                                )),
                                            )
                                            .clicked()
                                        {
                                            let _queued = connection.inspect(row.id);
                                        }
                                    }
                                },
                            );
                        if let Some(detail) = &view.detail {
                            ui.separator();
                            ui.heading(&detail.name);
                            ui.label(format!(
                                "{} · 版本 {} · {}",
                                task_state(detail.meta.state),
                                detail.meta.revision,
                                detail.id
                            ));
                            self.results_panel.draw(ui, connection, view);
                            if let Some(reason) = detail.meta.stop_reason {
                                ui.label(format!("停止原因：{reason:?}"));
                            }
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add_enabled(
                                        !view.busy,
                                        egui::Button::new("重新加载计划与状态"),
                                    )
                                    .clicked()
                                {
                                    let _queued = connection.inspect(detail.id);
                                }
                                if ui
                                    .add_enabled(
                                        !task_terminal(detail.meta.state),
                                        egui::Button::new("取消任务"),
                                    )
                                    .clicked()
                                    && !connection.cancel(detail.id)
                                {
                                    self.status = "取消队列已满，请查看状态".into();
                                }
                            });
                            if let Some(plan) = &view.plan {
                                let mut sources = Vec::new();
                                if task_terminal(detail.meta.state) && !detail.runner_active {
                                    for target in &detail.targets {
                                        for (index, step) in target.steps.iter().enumerate() {
                                            if target_terminal(target.state) && target_terminal(step.state) {
                                                for stderr in [false, true] {
                                                    sources.push(cshell_application::OutputAnalysisRequest { format: "cshell.output-analysis-request".into(), version: 1,
                                                        kind: cshell_application::OutputAnalysisKind::Task, run_id: detail.id, left: target.host_id,
                                                        left_revision: detail.meta.revision, right: None, right_revision: None, step: index as u32, stderr });
                                                }
                                            }
                                        }
                                    }
                                }
                                self.workspace_panel.draw(ui, &connection.workspace, sources);
                                egui::CollapsingHeader::new("核对冻结目标与命令")
                                    .default_open(true)
                                    .show(ui, |ui| self.draw_review(ui, plan));
                                let writable = view.writable && !view.busy;
                                if matches!(
                                    detail.meta.state,
                                    TaskState::Queued | TaskState::Paused
                                ) && !detail.runner_active
                                {
                                    ui.checkbox(
                                        &mut self.start_confirm,
                                        "确认执行显示的冻结计划（恢复时重新托管）",
                                    );
                                    if ui
                                        .add_enabled(
                                            writable && self.start_confirm,
                                            egui::Button::new("启动 / 恢复托管"),
                                        )
                                        .clicked()
                                    {
                                        let _queued =
                                            connection.start_run(detail.id, detail.meta.revision);
                                        self.clear_confirmations();
                                    }
                                }
                                if let Some(gate) = &detail.meta.gate {
                                    ui.label(format!(
                                        "等待批准：{:?} · 下一批 {}",
                                        gate.kind,
                                        gate.next_batch + 1
                                    ));
                                    ui.checkbox(&mut self.gate_confirm, "确认放行当前门禁后的目标");
                                    if !detail.runner_active {
                                        ui.label(
                                            "daemon 尚未托管此任务，请先恢复托管，再刷新当前门禁",
                                        );
                                    }
                                    if ui
                                        .add_enabled(
                                            writable && detail.runner_active && self.gate_confirm,
                                            egui::Button::new("批准继续"),
                                        )
                                        .clicked()
                                    {
                                        let _queued = connection.approve(
                                            detail.id,
                                            detail.meta.revision,
                                            gate.id,
                                        );
                                        self.clear_confirmations();
                                    }
                                }
                                if task_terminal(detail.meta.state) {
                                    ui.checkbox(
                                        &mut self.retry_confirm,
                                        "确认为勾选目标创建新的重试计划",
                                    );
                                    let unknown = detail.targets.iter().any(|target| {
                                        self.retry_hosts.contains(&target.host_id)
                                            && target.state == TargetState::Unknown
                                    });
                                    if unknown {
                                        ui.colored_label(
                                            cshell_ui::theme::WARNING,
                                            "Unknown 可能已在远端执行；重试会重新执行全部步骤",
                                        );
                                        ui.checkbox(
                                            &mut self.unknown_confirm,
                                            "确认重复执行 Unknown 目标",
                                        );
                                    }
                                    let safe = !unknown
                                        || self.unknown_confirm
                                            && plan
                                                .definition()
                                                .steps
                                                .iter()
                                                .all(|step| step.idempotent);
                                    if unknown
                                        && !plan
                                            .definition()
                                            .steps
                                            .iter()
                                            .all(|step| step.idempotent)
                                    {
                                        ui.label("此任务含未声明幂等的步骤，无法重试 Unknown 目标");
                                    }
                                    if ui
                                        .add_enabled(
                                            writable
                                                && !detail.runner_active
                                                && self.retry_confirm
                                                && safe
                                                && !self.retry_hosts.is_empty(),
                                            egui::Button::new("保存重试计划，随后核对启动"),
                                        )
                                        .clicked()
                                    {
                                        let _queued = connection.retry(
                                            detail.id,
                                            detail.meta.revision,
                                            self.retry_hosts.clone(),
                                            self.unknown_confirm,
                                        );
                                        self.clear_confirmations();
                                    }
                                }
                            }
                            ui.separator();
                            ui.label("目标与步骤结果（点击 stdout / stderr 查看输出页）");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.target_filter)
                                    .hint_text("按主机名称、地址或标识筛选")
                                    .char_limit(128),
                            );
                            let targets: Vec<_> = detail
                                .targets
                                .iter()
                                .filter(|target| {
                                    self.results_panel.matches(target)
                                        && (self.target_filter.is_empty()
                                            || target
                                                .host_id
                                                .to_string()
                                                .contains(&self.target_filter)
                                            || view
                                                .results
                                                .as_ref()
                                                .and_then(|document| {
                                                    document.target(target.host_id)
                                                })
                                                .is_some_and(|host| {
                                                    host.name.contains(&self.target_filter)
                                                        || host
                                                            .address
                                                            .contains(&self.target_filter)
                                                }))
                                })
                                .collect();
                            ui.label(format!(
                                "显示 {} / {} 个目标",
                                targets.len(),
                                detail.targets.len()
                            ));
                            egui::ScrollArea::vertical()
                                .id_salt("task-targets")
                                .max_height(360.0)
                                .show_rows(
                                    ui,
                                    ui.text_style_height(&egui::TextStyle::Body) + 10.0,
                                    targets.len(),
                                    |ui, range| {
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                                        for target in &targets[range] {
                                            ui.push_id(target.host_id.to_string(), |ui| {
                                                let name = view
                                                    .results
                                                    .as_ref()
                                                    .and_then(|document| {
                                                        document.target(target.host_id)
                                                    })
                                                    .map_or_else(
                                                        || target.host_id.to_string(),
                                                        |host| {
                                                            format!(
                                                                "{} · {}:{}",
                                                                host.name, host.address, host.port
                                                            )
                                                        },
                                                    );
                                                ui.horizontal_wrapped(|ui| {
                                                    let mut selected =
                                                        self.retry_hosts.contains(&target.host_id);
                                                    if ui
                                                        .add_enabled(
                                                            task_terminal(detail.meta.state)
                                                                && target_terminal(target.state)
                                                                && target.state
                                                                    != TargetState::Succeeded,
                                                            egui::Checkbox::new(
                                                                &mut selected,
                                                                "重试",
                                                            ),
                                                        )
                                                        .changed()
                                                    {
                                                        if selected {
                                                            self.retry_hosts.insert(target.host_id);
                                                        } else {
                                                            self.retry_hosts
                                                                .remove(&target.host_id);
                                                        }
                                                        self.retry_confirm = false;
                                                        self.unknown_confirm = false;
                                                    }
                                                    if ui
                                                        .add(
                                                            egui::Button::selectable(
                                                                self.output_key.is_some_and(
                                                                    |key| {
                                                                        key.host == target.host_id
                                                                    },
                                                                ),
                                                                format!(
                                                                    "{name} · {}",
                                                                    target_state(target.state)
                                                                ),
                                                            )
                                                            .truncate(),
                                                        )
                                                        .clicked()
                                                    {
                                                        self.output_key = Some(OutputKey {
                                                            run: detail.id,
                                                            host: target.host_id,
                                                            step: 0,
                                                            stderr: false,
                                                            offset: 0,
                                                        });
                                                        self.comparison_key = None;
                                                        self.clear_output_page();
                                                    }
                                                });
                                            });
                                        }
                                    },
                                );
                            if let Some(key) = self.output_key
                                && let Some(target) = detail
                                    .targets
                                    .iter()
                                    .find(|target| target.host_id == key.host)
                            {
                                for (index, step) in target.steps.iter().enumerate() {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(format!(
                                            "步骤 {} · {} · 退出码 {:?} · {:?}",
                                            index + 1,
                                            target_state(step.state),
                                            step.exit_code,
                                            step.reason
                                        ));
                                        for (stderr, label) in [(false, "stdout"), (true, "stderr")]
                                        {
                                            if ui
                                                .add_enabled(!view.busy, egui::Button::new(label))
                                                .clicked()
                                            {
                                                let key = OutputKey {
                                                    step: index as u32,
                                                    stderr,
                                                    offset: 0,
                                                    ..key
                                                };
                                                self.read_output(connection, key);
                                            }
                                        }
                                    });
                                }
                            }
                            if (!task_terminal(detail.meta.state) || detail.runner_active)
                                && view.writable
                                && !view.busy
                                && self.refresh_at.is_none_or(|at| Instant::now() >= at)
                            {
                                let _queued = connection.refresh(detail.id);
                                self.refresh_at = Some(Instant::now() + Duration::from_secs(2));
                            }
                        }
                    });
            });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT)).show(ui, |ui| {
            if let Some(key) = self.output_key {
                ui.label(format!("{} · 步骤 {} · {}", key.host, key.step + 1, if key.stderr { "stderr" } else { "stdout" }));
                ui.label("字节检查视图：控制字符和非 ASCII 字节以 \\xNN 显示；每页最多 64 KiB");
                if let (Some(document),Some(plan))=(&view.results,&view.plan)
                    && document.run_id==key.run&&plan.id()==key.run&&task_terminal(document.meta.state)&&!document.runner_active{
                    let hosts=document.targets.iter().filter(|target|target_terminal(target.run.state)&&target.run.steps.get(key.step as usize).is_some_and(|step|target_terminal(step.state)))
                        .map(|target|crate::output_analysis_panel::AnalysisHost{host:target.run.host_id,name:target.name.clone(),revision:document.meta.revision,child:key.run}).collect();
                    if let Some((host,offset))=self.analysis_panel.draw(ui,&connection.analysis,OutputAnalysisRequest{format:"cshell.output-analysis-request".into(),version:1,kind:OutputAnalysisKind::Task,run_id:key.run,
                        left:key.host,left_revision:document.meta.revision,right:None,right_revision:None,step:key.step,stderr:key.stderr},*plan.fingerprint(),hosts){
                        self.read_output(connection,OutputKey{host,offset,..key});
                    }
                    self.search_output(ui,connection,document,plan,key);
                }
                if let Some(right) = self.results_panel.comparison_controls(ui, view, key)
                    && let Some(document) = &view.results
                    && connection.compare(key, right, document.meta.revision)
                {
                    self.comparison_key = Some((key, right, document.meta.revision));
                    self.clear_output_page();
                }
                if let Some((expected, right, revision)) = self.comparison_key
                    && let Some(page) = &view.comparison
                    && page.comparison.run_id == expected.run && page.comparison.revision == revision
                    && page.comparison.left_host == expected.host && page.comparison.right_host == right
                    && page.comparison.step == expected.step && page.comparison.stderr == expected.stderr && page.comparison.offset == expected.offset
                {
                    let comparison = &page.comparison;
                    ui.label(format!("L {} / R {} · 此页 {} · {} 个不同字节 · 首个差异 {:?}", comparison.left_host, comparison.right_host,
                        if comparison.page_equal { "相同" } else { "不同" }, comparison.differing_bytes, comparison.first_difference));
                    ui.label(format!("采样长度：左 {} 字节 / 右 {} 字节；覆盖已采样左流 {} / 已采样右流 {}。= 为相同片段，! 为不同片段", comparison.left_length,
                        comparison.right_length, comparison.covers_left_stream, comparison.covers_right_stream));
                    if ui.button("返回单目标输出页").clicked() { self.comparison_key = None; self.clear_output_page(); }
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!view.busy, egui::Button::new("读取 / 刷新此页")).clicked() { self.read_output(connection, key); }
                    if ui.add_enabled(!view.busy && key.offset > 0, egui::Button::new("上一页")).clicked() {
                        self.read_output(connection, OutputKey { offset: key.offset.saturating_sub(64 * 1024), ..key });
                    }
                    if let Some(page) = &view.output && page.key == key {
                        ui.label(format!("{}..{} / {} 字节", key.offset, page.next, page.length));
                        if ui.add_enabled(!view.busy && page.next < page.length, egui::Button::new("下一页")).clicked() {
                            self.read_output(connection, OutputKey { offset: page.next, ..key });
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.button("页内向上").clicked() { self.scroll(-20, 40); }
                    if ui.button("页内向下").clicked() { self.scroll(20, 40); }
                });
            } else { ui.label("选择目标与步骤读取 stdout 或 stderr。任务由 daemon 托管，关闭此界面不取消执行。"); }
            ui.allocate_rect(ui.available_rect_before_wrap(), egui::Sense::hover()).rect
        }).inner
    }
    fn search_output(
        &mut self,
        ui: &mut egui::Ui,
        connection: &TaskConnection,
        document: &TaskResultDocument,
        plan: &TaskSnapshot,
        key: OutputKey,
    ) {
        if !document.targets.iter().any(|target| {
            target.run.host_id == key.host
                && target_terminal(target.run.state)
                && target
                    .run
                    .steps
                    .get(key.step as usize)
                    .is_some_and(|step| target_terminal(step.state))
        }) {
            return;
        }
        let scope = OutputAnalysisRequest {
            format: "cshell.output-analysis-request".into(),
            version: 1,
            kind: OutputAnalysisKind::Task,
            run_id: key.run,
            left: key.host,
            left_revision: document.meta.revision,
            right: None,
            right_revision: None,
            step: key.step,
            stderr: key.stderr,
        };
        if let Some(offset) = self.search_panel.draw(
            ui,
            &connection.search,
            scope.clone(),
            *plan.fingerprint(),
            key.run,
        ) {
            self.read_output(connection, OutputKey { offset, ..key });
        }
        let mut merged_scope = scope;
        merged_scope.stderr = false;
        if let Some((stderr, offset)) = self.merge_panel.draw(
            ui,
            &connection.merge,
            merged_scope.clone(),
            *plan.fingerprint(),
            key.run,
        ) {
            self.read_output(
                connection,
                OutputKey {
                    stderr,
                    offset,
                    ..key
                },
            );
        }
        self.archive_panel.draw(
            ui,
            &connection.archives,
            merged_scope,
            *plan.fingerprint(),
            key.run,
        );
    }
    fn draw_builder(
        &mut self,
        ui: &mut egui::Ui,
        connection: &TaskConnection,
        view: &TaskView,
        hosts: Option<&HostGroupView>,
    ) {
        self.editor.draw(ui, false, false);
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.selection_mode, 0, "查询");
            ui.radio_value(&mut self.selection_mode, 1, "主机组");
            ui.radio_value(&mut self.selection_mode, 2, "主机面板选择");
        });
        match self.selection_mode {
            0 => {
                ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("all / tag:web AND env:prod")
                        .char_limit(MAX_HOST_SELECTOR_BYTES),
                );
                if let Err(error) = HostSelector::parse(&self.query) {
                    ui.colored_label(cshell_ui::theme::WARNING, error.to_string());
                }
            }
            1 => {
                egui::ComboBox::from_id_salt("task-host-group")
                    .truncate()
                    .selected_text(self.group.map_or("选择组".into(), |id| id.to_string()))
                    .show_ui(ui, |ui| {
                        cshell_ui::form::menu(ui);
                        if let Some(snapshot) = hosts.and_then(|view| view.snapshot.as_ref()) {
                            for group in &snapshot.catalog.groups {
                                ui.selectable_value(&mut self.group, Some(group.id), &group.name);
                            }
                        }
                    });
            }
            _ => match &self.selection {
                Some(HostSelection::Selector { expression }) => {
                    ui.label(format!("采用查询：{expression}"));
                }
                Some(HostSelection::Group { id }) => {
                    ui.label(format!("采用主机组：{id}"));
                }
                Some(HostSelection::Explicit { hosts }) => {
                    ui.label(format!(
                        "采用固定选择：{} 台主机；保存后核对详细目标",
                        hosts.len()
                    ));
                }
                None => {
                    ui.label("请从主机面板工具栏采用选择");
                }
            },
        }

        let snapshot = hosts.and_then(|hosts| hosts.snapshot.as_ref());
        if let Some(snapshot) = snapshot {
            ui.label(format!(
                "主机版本 {} / Profile 版本 {}",
                snapshot.revision, snapshot.profile_revision
            ));
        }
        if ui
            .add_enabled(
                !view.busy
                    && hosts.is_some_and(|hosts| hosts.writable && !hosts.busy)
                    && snapshot.is_some(),
                egui::Button::new("保存计划并查看冻结目标"),
            )
            .clicked()
        {
            match (self.definition(), self.current_selection(), snapshot) {
                (Ok(definition), Some(selection), Some(snapshot)) => {
                    if connection.create(
                        definition,
                        selection,
                        snapshot.revision,
                        snapshot.profile_revision,
                    ) {
                        self.status.clear();
                        self.clear_confirmations();
                    } else {
                        self.status = "任务请求队列忙，请稍后重试".into();
                    }
                }
                (Err(error), _, _) => self.status = error,
                _ => self.status = "请先选择目标并加载主机库".into(),
            }
        }
    }
    fn draw_review(&mut self, ui: &mut egui::Ui, plan: &TaskSnapshot) {
        ui.label(format!(
            "远端 Shell：{:?} · 并发 {} · Canary {} · 批大小 {:?}",
            plan.definition().shell,
            plan.definition().policy.concurrency,
            plan.definition().policy.canary_count,
            plan.definition().policy.batch_size
        ));
        ui.label(format!(
            "Canary 后批准 {} · 批间批准 {} · 批间隔 {} ms · 失败阈值 {:?}",
            plan.definition().policy.approve_after_canary,
            plan.definition().policy.pause_between_batches,
            plan.definition().policy.batch_delay_ms,
            plan.definition().policy.failure_limit
        ));
        let policy = &plan.definition().policy;
        ui.label(format!(
            "超时 ms：排队 {} / 连接 {} / 单步 {} / 总计 {}",
            policy.queue_timeout_ms,
            policy.connect_timeout_ms,
            policy.step_timeout_ms,
            policy.total_timeout_ms
        ));
        if let Some(parent) = plan.retry_of() {
            ui.label(format!("重试来源：{parent}"));
        }
        ui.label(format!(
            "冻结目标 {} 台 · 主机版本 {} / Profile 版本 {}",
            plan.targets().targets().len(),
            plan.targets().host_revision(),
            plan.targets().profile_revision()
        ));
        let targets = plan.targets().targets();
        egui::ScrollArea::vertical()
            .id_salt("task-frozen-targets")
            .max_height(160.0)
            .show_rows(
                ui,
                ui.text_style_height(&egui::TextStyle::Body) + 6.0,
                targets.len(),
                |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for host in &targets[range] {
                        ui.label(format!(
                            "{} · {}:{} · Profile {}",
                            host.name, host.address, host.port, host.profile_id
                        ));
                    }
                },
            );
        egui::ComboBox::from_id_salt("task-review-step")
            .truncate()
            .selected_text(format!("步骤 {}", self.review_step + 1))
            .show_ui(ui, |ui| {
                cshell_ui::form::menu(ui);
                for (index, step) in plan.definition().steps.iter().enumerate() {
                    ui.selectable_value(&mut self.review_step, index, &step.name);
                }
            });
        if let Some(command) = plan.commands().get(self.review_step) {
            let mut command = command.as_str();
            ui.label("将发送的完整命令");
            ui.add(
                egui::TextEdit::multiline(&mut command)
                    .desired_rows(5)
                    .code_editor(),
            );
            let step = &plan.definition().steps[self.review_step];
            ui.label(format!(
                "成功码 {:?} · 幂等 {}",
                step.success_codes, step.idempotent
            ));
        }
    }
    fn clear_output_page(&mut self) {
        self.output_stamp = None;
        self.output_surface = None;
    }
    fn clear_output(&mut self) {
        self.merge_panel.reset();
        self.output_key = None;
        self.comparison_key = None;
        self.clear_output_page();
    }
    fn read_output(&mut self, connection: &TaskConnection, key: OutputKey) {
        if connection.output(key) {
            self.merge_panel.reset();
            self.output_key = Some(key);
            self.comparison_key = None;
            self.clear_output_page();
        }
    }
    pub(crate) fn update_output(&mut self, view: &TaskView) -> bool {
        if self.mode != TaskPanelMode::Commands {
            return false;
        }
        let previous = self.output_stamp;
        let mut changed = false;
        if self.comparison_key.is_none()
            && let Some(output) = &view.output
            && Some(output.key) == self.output_key
            && self.output_stamp != Some(output.page.revision)
        {
            let mut surface = LogSurfaceModel::default();
            match surface.submit_page(output.page.clone()) {
                Ok(_) => {
                    surface.anchor(1, 0);
                    self.output_surface = Some(surface);
                    self.output_stamp = Some(output.page.revision);
                }
                Err(_) => self.status = "输出页无法绘制，请重新读取".into(),
            }
        }
        if let Some((key, right, revision)) = self.comparison_key
            && let Some(output) = &view.comparison
            && output.comparison.run_id == key.run
            && output.comparison.revision == revision
            && output.comparison.left_host == key.host
            && output.comparison.right_host == right
            && output.comparison.step == key.step
            && output.comparison.stderr == key.stderr
            && output.comparison.offset == key.offset
            && self.output_stamp != Some(output.page.revision)
        {
            let mut surface = LogSurfaceModel::default();
            if surface.submit_page(output.page.clone()).is_ok() {
                surface.anchor(1, 0);
                self.output_surface = Some(surface);
                self.output_stamp = Some(output.page.revision);
            } else {
                self.status = "比较页无法绘制，请重新读取".into();
            }
        }
        if self.output_reflow.is_none() && self.output_surface.is_some() {
            self.output_reflow = TaskReflowWorker::start().ok();
        }
        if let Some(worker) = &self.output_reflow {
            if let Some(result) = worker.take_result() {
                changed = true;
                if let Some(surface) = &mut self.output_surface {
                    match result {
                        Ok(layout) => {
                            surface.submit_reflow(layout);
                        }
                        Err(_) => surface.reflow_failed(),
                    }
                }
            }
        }
        self.reflow(self.output_columns);
        changed || previous != self.output_stamp
    }
    fn reflow(&mut self, columns: u16) {
        self.output_columns = columns.max(1);
        if let Some(surface) = &mut self.output_surface
            && let Some(request) = surface.request_reflow(self.output_columns)
            && self
                .output_reflow
                .as_ref()
                .is_none_or(|worker| !worker.submit(request))
        {
            surface.reflow_failed();
        }
    }
    pub(crate) fn frame(&mut self, columns: u16, rows: u16) -> Option<LogSurfaceFrame> {
        if self.mode == TaskPanelMode::Mixed {
            return self.mixed_panel.frame(columns, rows);
        }
        if self.mode != TaskPanelMode::Commands {
            return None;
        }
        if self.merge_panel.active() {
            return self.merge_panel.frame(columns, rows);
        }
        self.reflow(columns);
        self.output_surface
            .as_ref()
            .and_then(|surface| surface.prepare_frame(rows))
    }
    pub(crate) fn scroll(&mut self, rows: i32, viewport_rows: u16) {
        if self.mode == TaskPanelMode::Mixed {
            self.mixed_panel.scroll(rows, viewport_rows);
            return;
        }
        if self.mode != TaskPanelMode::Commands {
            return;
        }
        if self.merge_panel.active() {
            self.merge_panel.scroll(rows, viewport_rows);
            return;
        }
        if let Some(surface) = &mut self.output_surface {
            surface.scroll_visual_rows(rows, viewport_rows);
        }
    }
}

fn task_state(state: TaskState) -> &'static str {
    match state {
        TaskState::Draft => "草稿",
        TaskState::Validating => "校验中",
        TaskState::AwaitingConfirmation => "待确认",
        TaskState::Queued => "待启动",
        TaskState::Running => "运行中",
        TaskState::Paused => "待批准",
        TaskState::Cancelling => "取消中",
        TaskState::Completed => "已完成",
        TaskState::PartialFailed => "部分失败",
        TaskState::Failed => "失败",
        TaskState::Cancelled => "已取消",
    }
}
fn target_state(state: TargetState) -> &'static str {
    match state {
        TargetState::Pending => "待执行",
        TargetState::Connecting => "连接中",
        TargetState::Running => "运行中",
        TargetState::Succeeded => "成功",
        TargetState::Failed => "失败",
        TargetState::TimedOut => "超时",
        TargetState::Cancelled => "取消",
        TargetState::Unknown => "Unknown（结果不明）",
    }
}
