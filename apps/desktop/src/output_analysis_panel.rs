use crate::output_analysis_connection::OutputAnalysisConnection;
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use std::time::{Duration, Instant};
pub(crate) struct AnalysisHost {
    pub host: HostId,
    pub name: String,
    pub revision: u64,
    pub child: TaskRunId,
}
#[derive(Default)]
pub(crate) struct OutputAnalysisPanel {
    right: Option<HostId>,
    scope: Option<(OutputAnalysisKind, TaskRunId, HostId, u32, bool)>,
    poll: Option<Instant>,
}
impl OutputAnalysisPanel {
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &OutputAnalysisConnection,
        request: OutputAnalysisRequest,
        hash: [u8; 32],
        hosts: Vec<AnalysisHost>,
    ) -> Option<(HostId, u64)> {
        let mut jump = None;
        let scope = (
            request.kind,
            request.run_id,
            request.left,
            request.step,
            request.stderr,
        );
        if self.scope != Some(scope) {
            self.scope = Some(scope);
            self.right = None;
        }
        if self.right.is_some_and(|id| {
            !hosts
                .iter()
                .any(|host| host.host == id && host.host != request.left)
        }) {
            self.right = None;
        }
        let view = connection.view();
        ui.collapsing("完整已保存输出摘要与比较", |ui| {
            ui.label(
                "读取全部已保存字节，后台计算 SHA-256；不规范化文本，Unknown 的远端结果仍不明。",
            );
            ui.label("比较目标来自当前载入的已停止命令目标；混合任务使用当前目标页。");
            if !view.status.is_empty() {
                ui.label(&view.status);
            }
            let ready = !view.busy
                && view
                    .document
                    .as_ref()
                    .is_none_or(|document| document.state != OutputAnalysisState::Running);
            let left = hosts
                .iter()
                .find(|host| host.host == request.left && host.revision == request.left_revision);
            if ui
                .add_enabled(
                    ready && left.is_some(),
                    egui::Button::new("计算此目标完整摘要"),
                )
                .clicked()
            {
                if let Some(left) = left {
                    let _queued = connection.launch(request.clone(), hash, left.child, None);
                }
            }
            egui::ComboBox::from_id_salt("full-output-right-host")
                .truncate()
                .selected_text(
                    self.right
                        .and_then(|id| hosts.iter().find(|host| host.host == id))
                        .map_or("选择比较目标", |host| host.name.as_str()),
                )
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    egui::ScrollArea::vertical().max_height(170.0).show_rows(
                        ui,
                        ui.text_style_height(&egui::TextStyle::Body) + 6.0,
                        hosts.len(),
                        |ui, range| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            for host in &hosts[range] {
                                if host.host != request.left {
                                    ui.selectable_value(
                                        &mut self.right,
                                        Some(host.host),
                                        format!("{} · {}", host.name, host.host),
                                    );
                                }
                            }
                        },
                    );
                });
            let right = self
                .right
                .and_then(|id| hosts.iter().find(|host| host.host == id));
            if ui
                .add_enabled(
                    ready && left.is_some() && right.is_some(),
                    egui::Button::new("比较两个目标完整字节流"),
                )
                .clicked()
            {
                if let (Some(left), Some(right)) = (left, right) {
                    let mut query = request.clone();
                    query.right = Some(right.host);
                    query.right_revision = Some(right.revision);
                    let _queued = connection.launch(query, hash, left.child, Some(right.child));
                }
            }
            if let Some(document) = &view.document {
                ui.separator();
                ui.label(format!(
                    "后台作业 {} · {:?}",
                    document.job_id, document.state
                ));
                ui.label(format!(
                    "运行 {} · 左 {} / 右 {:?} · 步骤 {} · {}",
                    document.request.run_id,
                    document.request.left,
                    document.request.right,
                    document.request.step + 1,
                    if document.request.stderr {
                        "stderr"
                    } else {
                        "stdout"
                    }
                ));
                ui.label(format!(
                    "已读取左 {} / 右 {:?} 字节",
                    document.left_bytes, document.right_bytes
                ));
                if document.state == OutputAnalysisState::Running {
                    if ui
                        .add_enabled(!view.busy, egui::Button::new("取消后台输出分析"))
                        .clicked()
                    {
                        let _queued = connection.get(document.clone(), true);
                    }
                    if !view.busy
                        && self
                            .poll
                            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
                        && connection.get(document.clone(), false)
                    {
                        self.poll = Some(Instant::now());
                    }
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
                if ui
                    .add_enabled(!view.busy, egui::Button::new("查询作业状态"))
                    .clicked()
                {
                    let _queued = connection.get(document.clone(), false);
                }
                if let Some(error) = document.error {
                    ui.label(format!("分析未完成：{error:?}"));
                }
                if let Some(result) = &document.result {
                    ui.label(format!(
                        "左 {} 字节 · SHA-256 {}",
                        result.left.bytes,
                        hex(result.left.sha256)
                    ));
                    if let Some(right) = &result.right {
                        ui.label(format!(
                            "右 {} 字节 · SHA-256 {}",
                            right.bytes,
                            hex(right.sha256)
                        ));
                        ui.label(format!(
                            "完整已保存字节流相同 {:?} · 不同字节 {:?} · 首个差异偏移 {:?}",
                            result.equal, result.differing_bytes, result.first_difference
                        ));
                    }
                    if let Some(offset) = result.first_difference
                        && document.request.kind == request.kind
                        && document.request.run_id == request.run_id
                        && document.request.left == request.left
                        && document.request.left_revision == request.left_revision
                        && document.request.step == request.step
                        && document.request.stderr == request.stderr
                    {
                        let location = if offset < result.left.bytes {
                            Some((
                                document.request.left,
                                document.request.left_revision,
                                document.left.child_id,
                            ))
                        } else {
                            document
                                .request
                                .right
                                .zip(document.request.right_revision)
                                .zip(document.right.as_ref())
                                .map(|((host, revision), binding)| {
                                    (host, revision, binding.child_id)
                                })
                        };
                        if let Some((host, revision, child)) = location
                            && hosts.iter().any(|target| {
                                target.host == host
                                    && target.revision == revision
                                    && target.child == child
                            })
                            && ui.button("读取首个差异所在字节页").clicked()
                        {
                            jump = Some((host, offset / 65536 * 65536));
                        }
                    }
                }
                ui.label("作业只观察所列版本的已保存输出，不表示命令成功或远端输出完整到达。");
            }
        });
        jump
    }
}
fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
