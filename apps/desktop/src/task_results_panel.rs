use crate::task_connection::{OutputKey, TaskConnection, TaskView};
use cshell_application::{
    TaskResultDocument, TaskResultExportFormat, TaskResultFilter, TaskStopReason, task_reason_name,
};
use cshell_domain::{HostId, TargetState, TaskRunId};
use std::{collections::BTreeSet, path::Path, sync::Arc};

#[derive(Default)]
pub(crate) struct TaskResultsPanel {
    filter: TaskResultFilter,
    stamp: Option<(TaskRunId, u64)>,
    group: Option<BTreeSet<HostId>>,
    export_path: String,
    export_format: TaskResultExportFormat,
    export_confirm: bool,
    right_host: Option<HostId>,
}
impl TaskResultsPanel {
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui, connection: &TaskConnection, view: &TaskView) {
        let Some(document) = &view.results else {
            return;
        };
        if self.stamp != Some((document.run_id, document.meta.revision)) {
            if self.stamp.map(|stamp| stamp.0) != Some(document.run_id) {
                self.filter = TaskResultFilter::default();
                self.right_host = None;
            }
            self.stamp = Some((document.run_id, document.meta.revision));
            self.group = None;
            self.export_confirm = false;
        }
        ui.label(format!(
            "结果快照版本 {} · {} 个目标 / {} 个步骤状态",
            document.meta.revision,
            document.summary.targets.total(),
            document.summary.steps.total()
        ));
        ui.horizontal_wrapped(|ui| {
            if ui
                .selectable_label(self.filter.state.is_none(), "全部状态")
                .clicked()
            {
                self.filter.state = None;
            }
            for state in states() {
                if ui
                    .selectable_label(
                        self.filter.state == Some(state),
                        format!(
                            "{} {}",
                            state_name(state),
                            document.summary.targets.get(state)
                        ),
                    )
                    .clicked()
                {
                    self.filter.state = Some(state);
                }
            }
        });
        if document.summary.targets.unknown > 0 {
            ui.colored_label(
                cshell_ui::theme::WARNING,
                "Unknown 仍为结果不明；输出文件与分页相同均不能证明远端成功",
            );
        }
        ui.collapsing("原因、退出码与结果分组", |ui| {
            egui::ComboBox::from_id_salt("task-result-reason")
                .truncate()
                .selected_text(self.filter.reason.map_or("全部原因", task_reason_name))
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut self.filter.reason, None, "全部原因");
                    for reason in reasons() {
                        ui.selectable_value(
                            &mut self.filter.reason,
                            Some(reason),
                            task_reason_name(reason),
                        );
                    }
                });
            egui::ComboBox::from_id_salt("task-result-exit")
                .truncate()
                .selected_text(
                    self.filter
                        .exit_code
                        .map_or("全部退出码".into(), |code| code.to_string()),
                )
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut self.filter.exit_code, None, "全部退出码");
                    for (code, count) in &document.summary.exit_codes {
                        ui.selectable_value(
                            &mut self.filter.exit_code,
                            Some(*code),
                            format!("{code} · {count} 步"),
                        );
                    }
                });
            egui::ComboBox::from_id_salt("task-result-step-filter")
                .truncate()
                .selected_text(
                    self.filter
                        .step
                        .map_or("全部步骤".into(), |step| format!("步骤 {}", step + 1)),
                )
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut self.filter.step, None, "全部步骤");
                    for (index, step) in document.steps.iter().enumerate() {
                        ui.selectable_value(
                            &mut self.filter.step,
                            Some(index),
                            format!("{} · {}", index + 1, step.name),
                        );
                    }
                });
            ui.label(format!(
                "{} 种实际结果组合（状态、每步退出码与原因；耗时不参与分组）",
                document.outcomes.len()
            ));
            if ui.button("清除分组筛选").clicked() {
                self.group = None;
            }
            let height = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
            egui::ScrollArea::vertical()
                .id_salt("task-outcomes")
                .max_height(150.0)
                .show_rows(ui, height, document.outcomes.len(), |ui, range| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for (index, group) in document.outcomes[range.clone()].iter().enumerate() {
                        let label = document
                            .target(group.representative)
                            .map_or("", |target| state_name(target.run.state));
                        if ui
                            .button(format!(
                                "组合 {} · {label} · {} 台",
                                range.start + index + 1,
                                group.hosts.len()
                            ))
                            .clicked()
                        {
                            self.group = Some(group.hosts.iter().copied().collect());
                        }
                    }
                });
            if ui.button("清除全部结果筛选").clicked() {
                self.filter = TaskResultFilter::default();
                self.group = None;
            }
        });
        ui.collapsing("导出结果快照", |ui| {
            ui.label("导出当前已载入版本的全部目标、步骤状态与退出码；包含主机信息，省略命令、凭据及输出正文");
            egui::ComboBox::from_id_salt("task-result-export-format").truncate().selected_text(match self.export_format { TaskResultExportFormat::Json => "JSON", TaskResultExportFormat::Csv => "CSV（文本保护）" }).show_ui(ui, |ui| { cshell_ui::form::menu(ui);
                if ui.selectable_value(&mut self.export_format, TaskResultExportFormat::Json, "JSON").changed() { self.export_confirm = false; }
                if ui.selectable_value(&mut self.export_format, TaskResultExportFormat::Csv, "CSV（文本保护）").changed() { self.export_confirm = false; }
            });
            if ui.add(egui::TextEdit::singleline(&mut self.export_path).hint_text("新文件的绝对路径").char_limit(8192)).changed() { self.export_confirm = false; }
            ui.checkbox(&mut self.export_confirm, "确认将此结果快照保存到所填新文件");
            if ui.add_enabled(!view.busy && self.export_confirm && Path::new(self.export_path.trim()).is_absolute(), egui::Button::new("保存全部目标结果")).clicked() {
                let _queued = connection.export(Arc::clone(document), self.export_format, self.export_path.trim().into());
                self.export_confirm = false;
            }
        });
    }
    pub(crate) fn matches(&self, target: &cshell_application::TaskTargetRun) -> bool {
        self.filter.matches(target)
            && self
                .group
                .as_ref()
                .is_none_or(|group| group.contains(&target.host_id))
    }
    pub(crate) fn comparison_controls(
        &mut self,
        ui: &mut egui::Ui,
        view: &TaskView,
        key: OutputKey,
    ) -> Option<HostId> {
        let document: &TaskResultDocument = view.results.as_deref()?;
        if document.run_id != key.run {
            return None;
        }
        if self.right_host.is_some_and(|id| {
            id == key.host
                || !document
                    .targets
                    .iter()
                    .any(|target| target.run.host_id == id)
        }) {
            self.right_host = None;
        }
        ui.collapsing("与另一目标比较此输出页", |ui| {
            ui.label("比较相同步骤、同一 stdout/stderr 流及同一字节偏移；每侧最多 64 KiB");
            egui::ComboBox::from_id_salt("task-result-compare-host")
                .truncate()
                .selected_text(
                    self.right_host
                        .and_then(|id| {
                            document
                                .targets
                                .iter()
                                .find(|target| target.run.host_id == id)
                        })
                        .map_or("选择右侧目标", |target| target.name.as_str()),
                )
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    let targets: Vec<_> = document
                        .targets
                        .iter()
                        .filter(|target| target.run.host_id != key.host)
                        .collect();
                    let height = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
                    egui::ScrollArea::vertical()
                        .id_salt("task-compare-hosts")
                        .max_height(200.0)
                        .show_rows(ui, height, targets.len(), |ui, range| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            for target in &targets[range] {
                                ui.selectable_value(
                                    &mut self.right_host,
                                    Some(target.run.host_id),
                                    format!("{} · {}", target.name, target.run.host_id),
                                );
                            }
                        });
                });
            let ready = cshell_application::task_terminal(document.meta.state)
                && !document.runner_active
                && !view.busy;
            if !cshell_application::task_terminal(document.meta.state) || document.runner_active {
                ui.label("等待任务执行和输出保存结束后再比较");
            } else if view.busy {
                ui.label("其他请求处理中，请稍候");
            }
            if ui
                .add_enabled(
                    ready && self.right_host.is_some(),
                    egui::Button::new("读取并比较这两页"),
                )
                .clicked()
            {
                self.right_host
            } else {
                None
            }
        })
        .body_returned
        .flatten()
    }
}
fn states() -> [TargetState; 8] {
    [
        TargetState::Pending,
        TargetState::Connecting,
        TargetState::Running,
        TargetState::Succeeded,
        TargetState::Failed,
        TargetState::TimedOut,
        TargetState::Cancelled,
        TargetState::Unknown,
    ]
}
fn reasons() -> [TaskStopReason; 12] {
    [
        TaskStopReason::UserCancelled,
        TaskStopReason::ConnectionFailure,
        TaskStopReason::ConnectionTimeout,
        TaskStopReason::QueueTimeout,
        TaskStopReason::StepTimeout,
        TaskStopReason::TotalTimeout,
        TaskStopReason::TransportLost,
        TaskStopReason::OutputFailure,
        TaskStopReason::ExecRejected,
        TaskStopReason::DaemonRestarted,
        TaskStopReason::FailureThreshold,
        TaskStopReason::CanaryFailed,
    ]
}
fn state_name(state: TargetState) -> &'static str {
    match state {
        TargetState::Pending => "待执行",
        TargetState::Connecting => "连接中",
        TargetState::Running => "运行中",
        TargetState::Succeeded => "成功",
        TargetState::Failed => "失败",
        TargetState::TimedOut => "超时",
        TargetState::Cancelled => "取消",
        TargetState::Unknown => "Unknown",
    }
}
