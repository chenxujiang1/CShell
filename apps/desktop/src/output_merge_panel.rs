use crate::{output_merge_connection::OutputMergeConnection, task_panel::TaskReflowWorker};
use cshell_application::*;
use cshell_domain::TaskRunId;
use cshell_render::{LogSurfaceFrame, LogSurfaceModel};
use std::sync::Arc;
#[derive(Default)]
pub(crate) struct OutputMergePanel {
    expected: Option<OutputMergeRequest>,
    surface: Option<LogSurfaceModel>,
    metadata: Option<Arc<OutputMergePage>>,
    stamp: Option<u64>,
    worker: Option<TaskReflowWorker>,
    columns: u16,
}
impl OutputMergePanel {
    pub fn reset(&mut self) {
        self.expected = None;
        self.surface = None;
        self.metadata = None;
        self.stamp = None;
    }
    pub fn active(&self) -> bool {
        self.expected.is_some()
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &OutputMergeConnection,
        scope: OutputAnalysisRequest,
        hash: [u8; 32],
        child: TaskRunId,
    ) -> Option<(bool, u64)> {
        if self
            .expected
            .as_ref()
            .is_some_and(|request| request.scope != scope)
        {
            self.reset();
        }
        let view = connection.view();
        if let Some(expected) = &self.expected
            && let Some(page) = &view.page
            && page.metadata.request == *expected
            && self.stamp != Some(page.rendered.revision)
        {
            let mut surface = LogSurfaceModel::default();
            if surface.submit_page(page.rendered.clone()).is_ok() {
                surface.anchor(1, 0);
                self.surface = Some(surface);
                self.metadata = Some(page.metadata.clone());
                self.stamp = Some(page.rendered.revision);
            }
        }
        let cached = self.metadata.clone();
        if view.busy || self.active() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        let mut jump = None;
        ui.collapsing("stdout / stderr 按记录顺序合流",|ui|{
            ui.label("仅单主机单步骤的 daemon 记录顺序；时间为本机记录时间，保留流标签和原始字节偏移。");
            if !view.status.is_empty(){ui.label(&view.status);}
            let request=OutputMergeRequest{format:"cshell.output-merge-request".into(),version:1,scope:scope.clone(),after:0,limit:64,expected_journal_sha256:None};
            if ui.add_enabled(!view.busy,egui::Button::new("读取合流记录第一页")).clicked()&&connection.read(request.clone(),hash,child){self.expected=Some(request);self.surface=None;self.metadata=None;self.stamp=None;}
            if self.active()&&ui.button("返回单流字节页").clicked(){self.reset();jump=Some((false,0));}
            if let Some(expected)=&self.expected&&let Some(metadata)=&cached&&metadata.request==*expected{
                ui.label(format!("合流显示 stdout + stderr · 共 {} 条 · 封口 {} / 日志限额截断 {} / 不完整尾部 {} 字节",metadata.total_events,metadata.sealed,metadata.truncated,metadata.partial_tail_bytes));
                ui.label(format!("stdout 记录覆盖 {} / {} 字节 · stderr {} / {} 字节",metadata.indexed_stdout_bytes,metadata.stdout_bytes,metadata.indexed_stderr_bytes,metadata.stderr_bytes));
                ui.label("未封口或未覆盖的字节没有时序证据；旧运行没有日志时无法合流。Unknown 仍不明。");
                if ui.add_enabled(!view.busy&&metadata.next.is_some(),egui::Button::new("下一合流页")).clicked(){
                    if let Ok(Some(next))=expected.next(metadata)&&connection.read(next.clone(),hash,child){self.expected=Some(next);self.surface=None;self.metadata=None;self.stamp=None;}
                }
                egui::ScrollArea::vertical().id_salt("merge-events").max_height(100.0).show_rows(ui,ui.text_style_height(&egui::TextStyle::Body)+6.0,metadata.events.len(),|ui,range|{
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for event in &metadata.events[range]{if ui.button(format!("#{} {} · {} 字节 · 打开原流",event.sequence,if event.stderr{"stderr"}else{"stdout"},event.bytes)).clicked(){jump=Some((event.stderr,event.stream_offset/65536*65536));}}
                });
            }
        });
        if jump.is_some() {
            self.reset();
        }
        jump
    }
    fn reflow(&mut self, columns: u16) {
        self.columns = columns.max(1);
        if self.worker.is_none() && self.surface.is_some() {
            self.worker = TaskReflowWorker::start().ok();
        }
        if let Some(worker) = &self.worker
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
                .worker
                .as_ref()
                .is_none_or(|worker| !worker.submit(request))
        {
            surface.reflow_failed();
        }
    }
    pub fn frame(&mut self, columns: u16, rows: u16) -> Option<LogSurfaceFrame> {
        self.reflow(columns);
        self.surface
            .as_ref()
            .and_then(|surface| surface.prepare_frame(rows))
    }
    pub fn scroll(&mut self, rows: i32, viewport: u16) {
        if let Some(surface) = &mut self.surface {
            surface.scroll_visual_rows(rows, viewport);
        }
    }
}
