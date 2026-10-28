use crate::output_workspace_connection::{Operation, OutputWorkspaceConnection};
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use std::{collections::BTreeMap, path::PathBuf};
pub(crate) struct OutputWorkspacePanel {
    run: Option<TaskRunId>,
    selected: BTreeMap<(HostId, u32, bool), OutputAnalysisRequest>,
    action: OutputWorkspaceAction,
    query: zeroize::Zeroizing<String>,
    confirmed: bool,
    retention_confirmed: bool,
    local_confirmed: bool,
    bundle_path: String,
    source_path: String,
    directory: String,
    source_page: usize,
    document_stamp: Option<(TaskRunId, Option<TaskRunId>)>,
}
impl Default for OutputWorkspacePanel {
    fn default() -> Self {
        Self {
            run: None,
            selected: BTreeMap::new(),
            action: OutputWorkspaceAction::Index,
            query: zeroize::Zeroizing::new(String::new()),
            confirmed: false,
            retention_confirmed: false,
            local_confirmed: false,
            bundle_path: String::new(),
            source_path: String::new(),
            directory: String::new(),
            source_page: 0,
            document_stamp: None,
        }
    }
}
impl OutputWorkspacePanel {
    pub fn deactivate(&mut self) {
        self.confirmed = false;
        self.retention_confirmed = false;
        self.local_confirmed = false;
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &OutputWorkspaceConnection,
        candidates: Vec<OutputAnalysisRequest>,
    ) {
        let run = candidates.first().map(|s| s.run_id);
        if run != self.run {
            self.run = run;
            self.selected.clear();
            self.deactivate();
            self.source_page = 0;
        }
        for source in &candidates {
            let key = (source.left, source.step, source.stderr);
            if self.selected.get(&key).is_some_and(|old| old != source) {
                self.selected.remove(&key);
                self.deactivate();
            }
        }
        let view = connection.view();
        let stamp = view
            .document
            .as_ref()
            .map(|d| (d.job_id, d.retention.as_ref().map(|p| p.id)));
        if self.document_stamp != stamp {
            self.deactivate();
            self.document_stamp = stamp;
        }
        ui.collapsing("聚合索引 / 搜索 / 文本 diff / 归档与保留", |ui| {
            egui::ComboBox::from_id_salt("output-workspace-action").truncate().selected_text(format!("{:?}", self.action)).show_ui(ui, |ui| { cshell_ui::form::menu(ui);
                for (action, label) in [(OutputWorkspaceAction::Index, "索引与摘要分组"), (OutputWorkspaceAction::Search, "跨主机字面搜索"),
                    (OutputWorkspaceAction::TextDiff, "两份 UTF-8 文本对齐"), (OutputWorkspaceAction::Bundle, "多输出归档包"),
                    (OutputWorkspaceAction::RetentionPreview, "输出保留策略预览")] {
                    if ui.selectable_value(&mut self.action, action, label).changed() { self.deactivate(); }
                }
            });
            if self.action == OutputWorkspaceAction::Search {
                if ui.add(egui::TextEdit::multiline(&mut *self.query).desired_rows(2).char_limit(4096).hint_text("字面查询，允许换行；保存请求时含查询正文")).changed() { self.confirmed = false; }
            }
            ui.label(format!("已选 {} / {} 个可用输出；混合列表换页保留已选项。", self.selected.len(), candidates.len()));
            ui.horizontal_wrapped(|ui| {
                if ui.button("清空选择").clicked() { self.selected.clear(); self.deactivate(); }
                if ui.button("选当前页可用项").clicked() {
                    for source in candidates.iter().skip(self.source_page * 64).take(64) {
                        if self.selected.len() < MAX_WORKSPACE_SOURCES && (!matches!(self.action, OutputWorkspaceAction::Bundle | OutputWorkspaceAction::RetentionPreview) || !source.stderr) {
                            self.selected.insert((source.left, source.step, source.stderr), source.clone());
                        }
                    }
                    self.deactivate();
                }
                if ui.add_enabled(self.source_page > 0, egui::Button::new("上页")).clicked() { self.source_page -= 1; }
                if ui.add_enabled((self.source_page + 1) * 64 < candidates.len(), egui::Button::new("下页")).clicked() { self.source_page += 1; }
            });
            egui::ScrollArea::vertical().id_salt("workspace-sources").max_height(200.0).show(ui, |ui| {
                for source in candidates.iter().skip(self.source_page * 64).take(64) {
                    let key = (source.left, source.step, source.stderr); let mut checked = self.selected.contains_key(&key);
                    let label = format!("{} · 步骤 {} · {} · v{}", source.left, source.step + 1, if source.stderr { "stderr" } else { "stdout" }, source.left_revision);
                    if ui.checkbox(&mut checked, label).changed() {
                        if checked && self.selected.len() < MAX_WORKSPACE_SOURCES { self.selected.insert(key, source.clone()); }
                        else { self.selected.remove(&key); }
                        self.deactivate();
                    }
                }
            });
            ui.label("归档／保留选 stdout 范围即包含该命令双流及可用 journal；文本 diff 限两份 UTF-8 输出，不归一化换行。");
            let request = OutputWorkspaceRequest { format: "cshell.output-workspace-request".into(), version: 1, action: self.action,
                sources: self.selected.values().cloned().collect(), query: (self.action == OutputWorkspaceAction::Search).then(|| self.query.to_string()), retention: None };
            let valid = request.validate().is_ok();
            ui.checkbox(&mut self.confirmed, "已核对选定输出及版本，开始此后台作业");
            if ui.add_enabled(valid && self.confirmed && !view.busy, egui::Button::new("开始")).clicked() {
                connection.enqueue(Operation::Launch(request, false)); self.confirmed = false;
            }
            if let Some(document) = &view.document {
                ui.label(format!("作业 {} · {:?} · {} / {} 项 · {} 字节 · {} 结果", document.job_id, document.state, document.completed_sources, document.sources.len(), document.bytes, document.rows));
                ui.label(format!("来源运行：{}", document.sources[0].run_id));
                if document.action == OutputWorkspaceAction::TextDiff && document.sources.len() == 2 {
                    ui.label(format!("左：{} / 步骤 {} / stderr={}；右：{} / 步骤 {} / stderr={}", document.sources[0].left, document.sources[0].step + 1, document.sources[0].stderr,
                        document.sources[1].left, document.sources[1].step + 1, document.sources[1].stderr));
                }
                if document.truncated { ui.colored_label(cshell_ui::theme::WARNING, "索引或匹配列表达到上限；完整摘要仍覆盖原流，请核对总行数和已索引数。"); }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!view.busy, egui::Button::new("刷新作业")).clicked() { connection.enqueue(Operation::Get(document.clone(), false)); }
                    if ui.add_enabled(!view.busy && document.state == OutputAnalysisState::Running, egui::Button::new("取消作业")).clicked() { connection.enqueue(Operation::Get(document.clone(), true)); }
                    if ui.add_enabled(!view.busy && document.state == OutputAnalysisState::Completed && document.rows > 0, egui::Button::new("结果第一页")).clicked() { connection.enqueue(Operation::Page(document.clone(), 0)); }
                });
                for group in &document.groups { ui.label(format!("{} 字节 · {} 项内容一致 · SHA {:?}", group.digest.bytes, group.sources.len(), group.digest.sha256)); }
                if let Some(plan) = &document.retention {
                    ui.label(format!("保留计划 {}；删除选定命令的原始双流/journal，运行元数据和实际回执保留。先导出需要保留的归档。", plan.id));
                    ui.checkbox(&mut self.retention_confirmed, "确认删除预览绑定的原始输出；失败或取消时可能已部分提交");
                    if ui.add_enabled(!view.busy && !view.write_uncertain && document.action == OutputWorkspaceAction::RetentionPreview && self.retention_confirmed, egui::Button::new("应用保留计划")).clicked() {
                        let request = OutputWorkspaceRequest { format: "cshell.output-workspace-request".into(), version: 1, action: OutputWorkspaceAction::RetentionApply,
                            sources: plan.sources.clone(), query: None, retention: Some(plan.clone()) };
                        connection.enqueue(Operation::Launch(request, true)); self.retention_confirmed = false;
                    }
                    if ui.add_enabled(!view.busy, egui::Button::new("读取持久删除回执")).clicked() { connection.enqueue(Operation::Receipt(plan.clone())); }
                }
                if document.artifact.is_some() {
                    ui.add(egui::TextEdit::singleline(&mut self.bundle_path).hint_text("归档包保存到新的绝对文件路径"));
                    if ui.add_enabled(!view.busy && !self.bundle_path.is_empty(), egui::Button::new("下载并校验整包")).clicked() { connection.enqueue(Operation::Export(document.clone(), PathBuf::from(&self.bundle_path))); }
                }
            }
            if let Some(plan) = &view.retention_plan {
                if ui.add_enabled(!view.busy, egui::Button::new("核对最近提交的删除回执")).clicked() { connection.enqueue(Operation::Receipt(plan.clone())); }
            }
            if let Some(page) = &view.page {
                ui.label(format!("结果 {}..{} / {}", page.offset, page.next, page.total));
                egui::ScrollArea::vertical().id_salt("workspace-result-page").max_height(300.0).show(ui, |ui| {
                    for row in &page.rows {
                        match row {
                            OutputWorkspaceRow::Index { entry } => {
                                ui.label(format!("输出 {}：{} 字节 / {} 行；索引 {} 行", entry.source, entry.digest.bytes, entry.lines, entry.indexed_lines));
                                if let Some(document) = &view.document && let Some(scope) = document.sources.get(entry.source) {
                                    if ui.add_enabled(!view.busy, egui::Button::new("读取持久行索引")).clicked() {
                                        connection.enqueue(Operation::Index(scope.clone(), entry.clone(), 0));
                                    }
                                }
                            }
                            OutputWorkspaceRow::Diff { row } => { ui.monospace(format!("{:?} L{:?} R{:?}", row.kind, row.left_line, row.right_line)); ui.monospace(crate::sync_input::compose::visible_body(&row.text).as_str()); }
                            _ => { ui.label(format!("{row:?}")); }
                        }
                    }
                });
                if let Some(document) = &view.document {
                    if ui.add_enabled(!view.busy && page.next < page.total, egui::Button::new("结果下一页")).clicked() { connection.enqueue(Operation::Page(document.clone(), page.next)); }
                }
            }
            if let Some(receipt) = &view.receipt { ui.label(format!("持久回执 {}：{:?}", receipt.plan.id, receipt.states)); }
            ui.separator(); ui.label("本机归档检查与提取（不执行内容）");
            if let Some(page) = &view.index_page {
                ui.label(format!("行索引 {}..{} / {}（总 {} 行）", page.offset, page.next, page.entry.indexed_lines, page.entry.lines));
                for (i, offset) in page.positions.iter().enumerate() { ui.monospace(format!("行 {} → 字节 {}", page.offset + i + 1, offset)); }
                if ui.add_enabled(!view.busy && page.next < page.entry.indexed_lines, egui::Button::new("行索引下一页")).clicked() {
                    connection.enqueue(Operation::Index(page.scope.clone(), page.entry.clone(), page.next));
                }
            }
            ui.add(egui::TextEdit::singleline(&mut self.source_path).hint_text("现有 CSHOUT01 / CSBND001 文件"));
            ui.add(egui::TextEdit::singleline(&mut self.directory).hint_text("新的绝对提取目录"));
            if ui.add_enabled(!view.busy && !self.source_path.is_empty(), egui::Button::new("校验容器")).clicked() { connection.enqueue(Operation::Inspect(PathBuf::from(&self.source_path))); }
            ui.checkbox(&mut self.local_confirmed, "确认提取到新目录；发布失败时可能已有部分文件");
            if ui.add_enabled(!view.busy && self.local_confirmed && !self.directory.is_empty(), egui::Button::new("校验并提取")).clicked() {
                connection.enqueue(Operation::Extract(PathBuf::from(&self.source_path), PathBuf::from(&self.directory))); self.local_confirmed = false;
            }
            if view.busy && ui.button("取消本机下载／提取").clicked() { connection.cancel_transfer(); }
            ui.label(&view.status);
        });
    }
}
