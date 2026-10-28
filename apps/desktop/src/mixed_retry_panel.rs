use crate::mixed_connection::{MixedConnection, MixedView};
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use std::{collections::BTreeMap, sync::Arc};
#[derive(Default)]
pub(crate) struct MixedRetryPanel {
    scope: Option<(TaskRunId, [u8; 32])>,
    selected: BTreeMap<HostId, (u64, String, MixedTaskState)>,
    confirm: bool,
    unknown: bool,
    files: bool,
    page_stamp: Option<Arc<MixedStatusPage>>,
}
impl MixedRetryPanel {
    pub fn deactivate(&mut self) {
        self.confirm = false;
        self.unknown = false;
        self.files = false;
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        connection: &MixedConnection,
        view: &MixedView,
        plan: &MixedTaskSnapshot,
        page: &MixedStatusPage,
    ) {
        let scope = (plan.id(), *plan.fingerprint());
        if self.scope != Some(scope) {
            self.scope = Some(scope);
            self.selected.clear();
            self.deactivate();
        }
        if self
            .page_stamp
            .as_ref()
            .zip(view.page.as_ref())
            .is_none_or(|(previous, current)| !Arc::ptr_eq(previous, current))
        {
            self.deactivate();
            self.page_stamp = view.page.clone();
            self.selected.retain(|host, (revision, _, _)| {
                page.targets
                    .iter()
                    .find(|target| target.run.host_id == *host)
                    .is_none_or(|target| {
                        target.run.revision == *revision
                            && mixed_retry_block(plan, &target.run).is_none()
                    })
            });
        }
        if page.active || !view.writes_allowed || view.cancel_busy {
            self.deactivate();
        }
        if let Some(proof) = plan.retry_provenance() {
            ui.label(format!(
                "新运行来源 {} · {} 个来源目标 · 已确认 Unknown {} / 文件重放 {}",
                proof.parent_id,
                proof.sources.len(),
                proof.unknown_confirmed,
                proof.file_replay_confirmed
            ));
        }
        ui.collapsing("选定停止目标，创建新运行重试",|ui|{
            ui.label("新运行从第一步重新执行，生成新的 child 标识；原运行与文件回执保留，创建后须单独核对和启动。");
            ui.label("已有命令执行意图需要原定义的幂等声明；不明文件回执须先在原运行修复。文件重新尝试可能遇到已有目的文件冲突。");
            egui::ScrollArea::vertical().id_salt("mixed-retry-targets").max_height(180.0).show_rows(ui,ui.text_style_height(&egui::TextStyle::Body)+9.0,page.targets.len(),|ui,range|{
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                for target in &page.targets[range]{let blocked=mixed_retry_block(plan,&target.run);let mut selected=self.selected.contains_key(&target.run.host_id);
                    let label=format!("{} · {:?} · 版本 {}{}",target.name,target.run.state,target.run.revision,blocked.map_or(String::new(),|reason|format!(" · {}",block_name(reason))));
                    if ui.add_enabled(!page.active&&blocked.is_none()&&!view.busy,egui::Checkbox::new(&mut selected,label)).changed(){
                        if selected{self.selected.insert(target.run.host_id,(target.run.revision,target.name.clone(),target.run.state));}else{self.selected.remove(&target.run.host_id);}self.deactivate();
                    }
                }
            });
            ui.label(format!("跨页已选 {} 个目标；当前筛选不改变保存的选择",self.selected.len()));
            egui::ScrollArea::vertical().id_salt("mixed-retry-selection").max_height(100.0).show_rows(ui,ui.text_style_height(&egui::TextStyle::Body)+7.0,self.selected.len(),|ui,range|{
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                let rows:Vec<_>=self.selected.iter().collect();for(host,(revision,name,state))in &rows[range]{ui.label(format!("{name} · {host} · 版本 {revision} · {state:?}"));}
            });
            if ui.button("清除全部重试选择").clicked(){self.selected.clear();self.deactivate();}
            let has_unknown=self.selected.values().any(|(_,_,state)|*state==MixedTaskState::Unknown);
            if has_unknown{ui.checkbox(&mut self.unknown,"接受结果不明的命令重放；可能重复原远端动作");}
            if plan.retry_has_files(){ui.checkbox(&mut self.files,"接受从头尝试文件步骤：不续接旧临时文件、不覆盖既有目的文件；下载会重新读取远端源");}
            ui.checkbox(&mut self.confirm,"确认仅为所选保存版本创建新运行，从第一步重放");
            let ready=view.available&&view.writes_allowed&&!view.busy&&!view.cancel_busy&&!page.active&&!self.selected.is_empty()&&self.confirm&&(!has_unknown||self.unknown)&&(!plan.retry_has_files()||self.files);
            if ui.add_enabled(ready,egui::Button::new("保存新运行并核对（不启动）")).clicked(){
                let confirmation=MixedRetryConfirmation{format:"cshell.mixed-retry".into(),version:1,run_id:plan.id(),snapshot_sha256:*plan.fingerprint(),targets:self.selected.iter().map(|(host,(revision,_,_))|MixedTargetRevision{host_id:*host,revision:*revision}).collect(),confirm_unknown:self.unknown,confirm_file_replay:self.files};
                let _queued=connection.retry(plan.clone(),confirmation);self.deactivate();
            }
        });
    }
}
fn block_name(reason: MixedRetryBlock) -> &'static str {
    match reason {
        MixedRetryBlock::NotStopped => "未停止",
        MixedRetryBlock::Succeeded => "已成功",
        MixedRetryBlock::CommandNotIdempotent => "已派发命令未声明幂等",
        MixedRetryBlock::UncertainFile => "文件回执不明，请原运行修复",
        MixedRetryBlock::ActiveFile => "文件意图仍在处理",
    }
}
