use crate::output_archive_connection::OutputArchiveConnection;
use cshell_application::*;
use cshell_domain::TaskRunId;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
#[derive(Default)]
pub(crate) struct OutputArchivePanel {
    scope: Option<OutputAnalysisRequest>,
    path: String,
    confirmed: bool,
    stamp: Option<Arc<OutputArchiveDocument>>,
    poll: Option<Instant>,
}
impl OutputArchivePanel {
    pub fn deactivate(&mut self) {
        self.confirmed = false;
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &OutputArchiveConnection,
        scope: OutputAnalysisRequest,
        hash: [u8; 32],
        child: TaskRunId,
    ) {
        let view = connection.view();
        if self.scope.as_ref() != Some(&scope) {
            self.scope = Some(scope.clone());
            self.confirmed = false;
        }
        if self
            .stamp
            .as_ref()
            .zip(view.document.as_ref())
            .is_none_or(|(before, now)| !Arc::ptr_eq(before, now))
        {
            self.stamp = view.document.clone();
            self.confirmed = false;
        }
        ui.collapsing("捕获并下载原始输出归档",|ui|{
            ui.label("归档单主机单步骤的 stdout、stderr 和可用时序日志；保留原始字节及状态，可能包含敏感输出。");
            if !view.status.is_empty(){ui.label(&view.status);}
            if ui.add_enabled(!view.busy&&view.document.as_ref().is_none_or(|document|document.state!=OutputAnalysisState::Running),egui::Button::new("捕获此停止命令的输出")).clicked(){let _queued=connection.launch(scope.clone(),hash,child);self.confirmed=false;}
            if view.busy{ui.ctx().request_repaint_after(Duration::from_millis(250));}
            if let Some(document)=&view.document{
                ui.label(format!("作业 {} · {:?} · 源摘要已读 {} / 归档已写 {} 字节",document.job_id,document.state,document.source_bytes,document.bytes));
                ui.label(format!("捕获运行 {} · 主机 {} · 版本 {} · 步骤 {} · 原始命令状态 {:?}",document.scope.run_id,document.scope.left,document.scope.left_revision,document.scope.step+1,document.binding.step_state));
                if document.state==OutputAnalysisState::Running{
                    if ui.add_enabled(!view.busy,egui::Button::new("取消 daemon 捕获作业")).clicked(){let _queued=connection.get(document.clone(),true);}
                    if !view.busy&&self.poll.is_none_or(|at|at.elapsed()>=Duration::from_secs(1))&&connection.get(document.clone(),false){self.poll=Some(Instant::now());}ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
                if ui.add_enabled(!view.busy,egui::Button::new("查询捕获状态")).clicked(){let _queued=connection.get(document.clone(),false);}
                if let Some(error)=document.error{ui.label(format!("捕获未完成：{error:?}"));}
                if let Some(artifact)=&document.artifact{
                    ui.label(format!("归档 {} · {} 字节 · SHA-256 {}",artifact.manifest.archive_id,artifact.bytes,hex(artifact.sha256)));
                    for entry in &artifact.manifest.entries{ui.label(format!("{:?} · {} 字节 · {}",entry.kind,entry.bytes,hex(entry.sha256)));}
                    ui.label("下载所列捕获，包含全部原始输出；本机保存路径不发送给 daemon。使用新文件的绝对路径。");
                    if ui.add(egui::TextEdit::singleline(&mut self.path).char_limit(8192).hint_text("例如 D:\\exports\\command.cshout")).changed(){self.confirmed=false;}
                    ui.checkbox(&mut self.confirmed,"确认将所列原始输出归档保存到本机新文件");
                    if ui.add_enabled(!view.busy&&self.confirmed&&Path::new(self.path.trim()).is_absolute(),egui::Button::new("下载、校验并保存归档")).clicked(){let _queued=connection.export(document.clone(),self.path.trim().into());self.confirmed=false;}
                }
            }
            if let Some(control)=&view.transfer{ui.label(format!("本机已暂存 {} 字节",control.bytes()));if let Some(path)=&view.export_path{ui.label(format!("此次下载目标：{}",path.display()));}
                if ui.add_enabled(view.busy&&control.can_cancel(),egui::Button::new("取消本机下载（发布前）")).clicked(){let _cancelled=connection.cancel_export();}
                if control.cancelled(){ui.label("取消已请求；等待当前读取或同步结束。");}}
            ui.label("校验归档完整性不代表命令成功或全部远端输出到达；Unknown 仍不明。缓存过期或淘汰后需显式重新捕获。");
        });
    }
}
fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
