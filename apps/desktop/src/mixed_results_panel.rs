//! Displays the captured dataset independently of the mutable live status page.
use crate::mixed_connection::{MixedConnection, MixedView};
use cshell_application::*;
use cshell_domain::HostId;
use std::{collections::BTreeSet, path::Path, sync::Arc};
#[derive(Default)]
pub(crate) struct MixedResultsPanel {
    document: Option<Arc<MixedResultDocument>>,
    group: Option<BTreeSet<HostId>>,
    state: Option<MixedTaskState>,
    path: String,
    format: TaskResultExportFormat,
    confirmed: bool,
}
impl MixedResultsPanel {
    pub fn deactivate(&mut self) {
        self.confirmed = false;
    }
    pub fn draw(&mut self, ui: &mut egui::Ui, connection: &MixedConnection, view: &MixedView) {
        let Some(document) = &view.results else {
            self.document = None;
            self.confirmed = false;
            return;
        };
        if self
            .document
            .as_ref()
            .is_none_or(|saved| !Arc::ptr_eq(saved, document))
        {
            self.document = Some(document.clone());
            self.group = None;
            self.state = None;
            self.confirmed = false;
        }
        ui.collapsing("已抓取的全部混合结果",|ui|{
            let header=&document.header;let summary=&header.summary;
            ui.label(format!("{} 个目标 · 抓取时间 {} ms · 后续修复不改变此结果",header.target_count,header.captured_at_ms));
            ui.horizontal_wrapped(|ui|{
                ui.selectable_value(&mut self.state,None,"全部状态");
                for state in [MixedTaskState::Succeeded,MixedTaskState::Failed,MixedTaskState::Cancelled,MixedTaskState::Unknown,MixedTaskState::TimedOut]{
                    ui.selectable_value(&mut self.state,Some(state),format!("{state:?} {}",summary.targets[state.code() as usize]));
                }
            });
            ui.label(format!("上传/下载步骤 {} / {} · 文件进度 {} 字节 · 已校验 {} · 已提交 {} · 已认领 {} · 提交后待清理 {}",summary.uploads,summary.downloads,summary.file_bytes,summary.verified,summary.committed,summary.adopted,summary.cleanup_pending));
            if summary.targets[MixedTaskState::Unknown.code() as usize]>0{ui.label("Unknown 为结果不明；文件内容匹配与进度不能证明原运行已提交。");}
            ui.collapsing("退出码、原因与回执组合",|ui|{
                for (code,count) in &summary.command_exit_codes{ui.label(format!("实际退出码 {code}：{count} 步"));}
                for (reason,count) in &summary.reasons{ui.label(format!("{reason}：{count}"));}
                ui.label("组合依据状态、退出码和文件回执；未比较输出正文。");
                if ui.button("清除组合筛选").clicked(){self.group=None;}
                egui::ScrollArea::vertical().id_salt("mixed-result-groups").max_height(140.0)
                    .show_rows(ui,ui.text_style_height(&egui::TextStyle::Body)+6.0,header.groups.len(),|ui,range|{
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        for index in range{let group=&header.groups[index];if ui.button(format!("组合 {} · {} 台",index+1,group.hosts.len())).clicked(){self.group=Some(group.hosts.iter().copied().collect());}}
                    });
            });
            let targets:Vec<_>=document.targets.iter().filter(|target|self.state.is_none_or(|state|state==target.run.state)&&self.group.as_ref().is_none_or(|ids|ids.contains(&target.run.host_id))).collect();
            ui.label(format!("抓取结果中显示 {} 台",targets.len()));
            egui::ScrollArea::vertical().id_salt("mixed-captured-targets").max_height(190.0)
                .show_rows(ui,ui.text_style_height(&egui::TextStyle::Body)+8.0,targets.len(),|ui,range|{
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    for target in &targets[range]{
                        let text=format!("{} · {}:{} · {:?} · 版本 {} · {:?}",target.name,target.address,target.port,target.run.state,target.run.revision,target.run.reason);
                        let details=target.run.steps.iter().enumerate().map(|(index,step)|match &step.receipt{
                            MixedReceipt::Command{target,..}=>format!("{}. {:?} · 退出码 {:?}",index+1,step.state,target.steps[0].exit_code),
                            MixedReceipt::Upload{row}=>format!("{}. {:?} · {} 字节 · 提交 {} / 认领 {} / 清理 {}",index+1,step.state,row.bytes,row.committed,row.adopted,row.temporary_cleaned),
                            MixedReceipt::Download{row}=>format!("{}. {:?} · {} 字节 · 提交 {} / 认领 {} / 清理 {}",index+1,step.state,row.bytes,row.committed,row.adopted,row.temporary_cleaned),
                        }).collect::<Vec<_>>().join("\n");
                        ui.add(egui::Label::new(&text).truncate()).on_hover_text(format!("{text}\n{details}"));
                    }
                });
            ui.collapsing("保存抓取结果",|ui|{
                ui.label("保存此抓取的全部目标，包含主机信息和原生回执；筛选不缩减导出内容。");
                ui.label("省略命令、文件路径、凭据及输出正文；使用新文件的绝对路径。");
                egui::ComboBox::from_id_salt("mixed-export-format").truncate().selected_text(match self.format{TaskResultExportFormat::Json=>"JSON",TaskResultExportFormat::Csv=>"CSV"}).show_ui(ui,|ui|{ cshell_ui::form::menu(ui);
                    if ui.selectable_value(&mut self.format,TaskResultExportFormat::Json,"JSON").changed(){self.confirmed=false;}
                    if ui.selectable_value(&mut self.format,TaskResultExportFormat::Csv,"CSV（文本保护）").changed(){self.confirmed=false;}
                });
                if ui.add(egui::TextEdit::singleline(&mut self.path).char_limit(8192).hint_text("新文件的绝对路径")).changed(){self.confirmed=false;}
                ui.checkbox(&mut self.confirmed,"确认将此抓取结果保存到所填新文件");
                if ui.add_enabled(!view.busy&&self.confirmed&&Path::new(self.path.trim()).is_absolute(),egui::Button::new("保存全部抓取结果")).clicked(){
                    let _queued=connection.export(document.clone(),self.format,self.path.trim().into());self.confirmed=false;
                }
            });
        });
    }
}
