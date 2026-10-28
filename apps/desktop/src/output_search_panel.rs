use crate::output_search_connection::OutputSearchConnection;
use cshell_application::*;
use cshell_domain::TaskRunId;
use std::time::{Duration, Instant};
use zeroize::Zeroize;
#[derive(Default)]
pub(crate) struct OutputSearchPanel {
    pattern: String,
    hex: bool,
    poll: Option<Instant>,
}
impl Drop for OutputSearchPanel {
    fn drop(&mut self) {
        self.pattern.zeroize();
    }
}
impl OutputSearchPanel {
    pub fn deactivate(&mut self) {
        self.pattern.zeroize();
        self.poll = None;
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &OutputSearchConnection,
        scope: OutputAnalysisRequest,
        hash: [u8; 32],
        child: TaskRunId,
    ) -> Option<u64> {
        let mut jump = None;
        let view = connection.view();
        ui.collapsing("检索完整已保存字节流", |ui| {
            ui.label(
                "大小写敏感的字节匹配，支持重叠位置；每批最多 128 条。续查重新核对整条流的摘要。",
            );
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut self.hex, false, "UTF-8 文本");
                ui.radio_value(&mut self.hex, true, "十六进制字节");
            });
            ui.add(
                egui::TextEdit::singleline(&mut self.pattern)
                    .char_limit(3072)
                    .hint_text(if self.hex {
                        "例如 00 ff 41"
                    } else {
                        "按 UTF-8 原始字节匹配"
                    }),
            );
            let request = output_search_pattern(&self.pattern, self.hex)
                .ok()
                .map(|pattern| OutputSearchRequest {
                    format: "cshell.output-search-request".into(),
                    version: 1,
                    scope: scope.clone(),
                    pattern,
                    start: 0,
                    maximum_matches: 128,
                    expected_stream_sha256: None,
                });
            let ready = !view.busy
                && view
                    .document
                    .as_ref()
                    .is_none_or(|document| document.state != OutputAnalysisState::Running);
            if ui
                .add_enabled(
                    ready && request.is_some(),
                    egui::Button::new("从头检索此目标输出"),
                )
                .clicked()
            {
                if let Some(request) = &request {
                    let _queued = connection.launch(request.clone(), hash, child);
                }
            }
            if !view.status.is_empty() {
                ui.label(&view.status);
            }
            if let Some(document) = &view.document {
                ui.label(format!(
                    "检索作业 {} · {:?} · 已读取 {} 字节",
                    document.job_id, document.state, document.bytes
                ));
                let bound = &document.request.scope;
                ui.label(format!(
                    "运行 {} · 主机 {} · 版本 {} · 步骤 {} · {} · 匹配起始 {}",
                    bound.run_id,
                    bound.left,
                    bound.left_revision,
                    bound.step + 1,
                    if bound.stderr { "stderr" } else { "stdout" },
                    document.request.start
                ));
                if document.state == OutputAnalysisState::Running {
                    if ui
                        .add_enabled(!view.busy, egui::Button::new("取消后台检索"))
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
                    .add_enabled(!view.busy, egui::Button::new("查询检索状态"))
                    .clicked()
                {
                    let _queued = connection.get(document.clone(), false);
                }
                if let Some(error) = document.error {
                    ui.label(format!("检索未完成：{error:?}"));
                }
                if let Some(result) = &document.result {
                    ui.label(format!(
                        "完整已保存流 {} 字节 · SHA-256 {}",
                        result.stream.bytes,
                        hex(result.stream.sha256)
                    ));
                    ui.label(format!(
                        "本批 {} 条 · 匹配扫描至 {} · 下一匹配起始 {:?}",
                        result.matches.len(),
                        result.searched_through,
                        result.next
                    ));
                    let current = request
                        .as_ref()
                        .and_then(|request| request.descriptor().ok())
                        .is_some_and(|descriptor| descriptor.same_query(&document.request))
                        && document.binding.snapshot_sha256 == hash
                        && document.binding.child_id == child;
                    if !current {
                        ui.label("此结果属于所列旧查询或范围，需重新检索当前输入后再跳转。");
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("stored-output-search-matches")
                        .max_height(170.0)
                        .show_rows(
                            ui,
                            ui.text_style_height(&egui::TextStyle::Body) + 7.0,
                            result.matches.len(),
                            |ui, range| {
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                                for offset in &result.matches[range] {
                                    if ui
                                        .add_enabled(
                                            current,
                                            egui::Button::new(format!(
                                                "匹配偏移 {offset} · 读取所在字节页"
                                            )),
                                        )
                                        .clicked()
                                    {
                                        jump = Some(offset / 65536 * 65536);
                                    }
                                }
                            },
                        );
                    if ui
                        .add_enabled(
                            ready && current && result.next.is_some(),
                            egui::Button::new("续查下一批匹配"),
                        )
                        .clicked()
                    {
                        if let Some(request) = &request
                            && let Ok(Some(next)) = request.next(document)
                        {
                            let _queued = connection.launch(next, hash, child);
                        }
                    }
                    if result.next.is_none() {
                        ui.label("该起始位置之后的匹配已返回完毕。");
                    }
                }
                ui.label("检索只观察本地已保存输出；没有匹配不能证明命令成功，Unknown 仍不明。");
            }
        });
        jump
    }
}
fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
