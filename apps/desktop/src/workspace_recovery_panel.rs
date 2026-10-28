use crate::{profile_connection::DesktopProfileView, workspace_connection::WorkspaceConnection};
use cshell_application::*;
use cshell_domain::*;
use std::{
    io::{Read, Write},
    sync::{Arc, mpsc},
};
enum FileJob {
    Import(String, u64),
    Export(String, WorkspaceDocument, u64),
}
struct FileResult {
    epoch: u64,
    result: Result<Option<WorkspaceDocument>, String>,
}
struct FileWorker {
    sender: Option<mpsc::SyncSender<FileJob>>,
    receiver: mpsc::Receiver<FileResult>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl FileWorker {
    fn start() -> std::io::Result<Self> {
        let (sender, jobs) = mpsc::sync_channel::<FileJob>(1);
        let (results, receiver) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("cshell-workspace-files".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let (epoch, result) = match job {
                        FileJob::Import(path, epoch) => (epoch, import(&path)),
                        FileJob::Export(path, document, epoch) => {
                            (epoch, export(&path, &document).map(|_| None))
                        }
                    };
                    if results.send(FileResult { epoch, result }).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            receiver,
            worker: Some(worker),
        })
    }
}
impl Drop for FileWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn import(path: &str) -> Result<Option<WorkspaceDocument>, String> {
    let file = std::fs::File::open(path).map_err(|_| "无法打开布局文件")?;
    if !file.metadata().map_err(|_| "无法检查布局文件")?.is_file() {
        return Err("布局输入必须是普通文件".into());
    }
    let mut bytes = Vec::new();
    file.take((MAX_WORKSPACE_BYTES + 4) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取布局文件")?;
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes.drain(..3);
    }
    let document = decode_workspace(&bytes).map_err(|e| e.to_string())?;
    validate_restorable_workspace(&document).map_err(|e| e.to_string())?;
    Ok(Some(document))
}
fn export(path: &str, document: &WorkspaceDocument) -> Result<(), String> {
    let bytes = encode_workspace(document).map_err(|e| e.to_string())?;
    let path = std::path::Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| "无法暂存布局文件")?;
    file.write_all(&bytes).map_err(|_| "布局文件写入失败")?;
    file.as_file().sync_all().map_err(|_| "布局文件同步失败")?;
    file.persist_noclobber(path)
        .map_err(|_| "目标已存在或布局导出未确认")?;
    Ok(())
}
#[derive(Default)]
pub(super) struct WorkspaceRecoveryPanel {
    pub open: bool,
    worker: Option<FileWorker>,
    file_busy: bool,
    file_path: String,
    json: String,
    epoch: u64,
    file_epoch: u64,
    plan: Option<Arc<WorkspaceRestorePlan>>,
    confirm: bool,
    error: Option<String>,
    generation: Option<u64>,
}
impl WorkspaceRecoveryPanel {
    pub fn poll(&mut self, connection: Option<&WorkspaceConnection>) -> bool {
        let generation = connection.map(|c| c.view().generation);
        let mut changed = self.generation != generation;
        self.generation = generation;
        if let Some(worker) = &self.worker
            && let Ok(result) = worker.receiver.try_recv()
        {
            self.file_busy = false;
            changed = true;
            if result.epoch == self.file_epoch {
                match result.result {
                    Ok(Some(document)) => {
                        self.json = serde_json::to_string_pretty(&document).unwrap_or_default();
                        self.epoch = self.epoch.wrapping_add(1);
                        self.plan = None;
                        self.confirm = false;
                        self.error = None;
                    }
                    Ok(None) => self.error = Some("布局文件已导出".into()),
                    Err(error) => self.error = Some(error),
                }
            }
        }
        changed
    }
    pub fn draw(
        &mut self,
        context: &egui::Context,
        connection: Option<&WorkspaceConnection>,
        current: &WorkspaceDocument,
        sessions: &std::collections::BTreeMap<TabId, SessionId>,
        profiles: Option<&DesktopProfileView>,
    ) {
        if !self.open {
            return;
        }
        context.request_repaint_after(std::time::Duration::from_millis(150));
        if self.worker.is_none() {
            match FileWorker::start() {
                Ok(worker) => self.worker = Some(worker),
                Err(_) => self.error = Some("布局文件后台线程无法启动".into()),
            }
        }
        let view = connection
            .map(WorkspaceConnection::view)
            .unwrap_or_default();
        let ready = !self.file_busy
            && view.ready
            && view.paused
            && !view.busy
            && !view.blocked
            && view.restored.is_none();
        let revision = view.snapshot.as_ref().map_or(0, |s| s.revision);
        if self
            .plan
            .as_ref()
            .is_some_and(|p| p.before != *current || p.storage_revision != revision)
        {
            self.plan = None;
            self.confirm = false;
        }
        let mut open = self.open;
        cshell_ui::form::dialog(&context, "工作区恢复 / 布局导入导出").open(&mut open).default_size([820.0,700.0]).show(context,|ui|{
            ui.weak("自动保存暂停期间先读取、预览再确认。只恢复布局与 Profile 引用，不运行命令。相同标签和 Profile 可保留已有会话绑定；其他视图退出，不发送进程终止。");
            ui.label(format!("存储版本 {revision} · {}",view.status));if let Some(error)=self.error.as_ref().or(view.error.as_ref()){ui.colored_label(cshell_ui::theme::WARNING,error);}if view.busy||self.file_busy{ui.spinner();}
            if ui.add_enabled(connection.is_some()&&!view.busy,egui::Button::new("暂停自动保存并明确重新读取")).clicked()&&let Some(c)=connection{c.pause_reload();self.plan=None;self.confirm=false;}
            ui.add_enabled_ui(!view.busy&&!self.file_busy,|ui|{
                ui.horizontal_wrapped(|ui|{
                    if ui.button("以本地当前布局为目标（保留草稿并恢复保存）").clicked(){self.json=serde_json::to_string_pretty(current).unwrap_or_default();self.epoch=self.epoch.wrapping_add(1);self.plan=None;self.confirm=false;}
                    if ui.add_enabled(view.snapshot.is_some(),egui::Button::new("以最新存储布局为目标")).clicked()&&let Some(snapshot)=&view.snapshot{self.json=serde_json::to_string_pretty(&snapshot.document).unwrap_or_default();self.epoch=self.epoch.wrapping_add(1);self.plan=None;self.confirm=false;}
                });
                ui.horizontal_wrapped(|ui|{ui.label("布局文件路径");if ui.add(egui::TextEdit::singleline(&mut self.file_path).char_limit(4096)).changed(){self.file_epoch=self.file_epoch.wrapping_add(1);}});
                let path_valid=!self.file_path.trim().is_empty()&&self.file_path.len()<=4096&&!self.file_path.chars().any(char::is_control);
                ui.horizontal_wrapped(|ui|{
                    if ui.add_enabled(path_valid,egui::Button::new("从文件载入目标布局")).clicked(){self.file_epoch=self.file_epoch.wrapping_add(1);self.file_busy=self.worker.as_ref().and_then(|w|w.sender.as_ref()).is_some_and(|s|s.try_send(FileJob::Import(self.file_path.clone(),self.file_epoch)).is_ok());}
                    if ui.add_enabled(path_valid,egui::Button::new("导出当前本地布局到新文件")).clicked(){self.file_epoch=self.file_epoch.wrapping_add(1);self.file_busy=self.worker.as_ref().and_then(|w|w.sender.as_ref()).is_some_and(|s|s.try_send(FileJob::Export(self.file_path.clone(),current.clone(),self.file_epoch)).is_ok());}
                });
                if ui.add(egui::TextEdit::multiline(&mut self.json).code_editor().desired_rows(12).char_limit(MAX_WORKSPACE_BYTES)).changed(){self.epoch=self.epoch.wrapping_add(1);self.plan=None;self.confirm=false;}
                if ui.add_enabled(ready,egui::Button::new("生成完整恢复预览")).clicked(){match decode_workspace(self.json.as_bytes()).and_then(|target|WorkspaceRestorePlan::new(revision,current.clone(),target)){Ok(plan)=>{self.plan=Some(Arc::new(plan));self.error=None;self.confirm=false;},Err(error)=>self.error=Some(error.to_string())}}
            });
            if let Some(plan)=&self.plan{
                ui.label(format!("目标 {} 窗口 / {} 分屏组 / {} 标签 · 可保留已绑定会话 {} · 退出视图 {} · 新占位标签 {}",plan.target.windows.len(),plan.target.tab_groups.len(),plan.target.bindings.len(),plan.retain_tabs.iter().filter(|id|sessions.contains_key(id)).count(),plan.detach_tabs.len(),plan.placeholder_tabs.len()));
                egui::ScrollArea::vertical().max_height(160.0).show(ui,|ui|{for binding in &plan.target.bindings{let profile=profiles.and_then(|p|p.catalog.as_ref()).and_then(|c|c.profiles.iter().find(|p|Some(p.id)==binding.profile_id));let state=if plan.retain_tabs.contains(&binding.tab_id)&&sessions.contains_key(&binding.tab_id){"保留当前会话绑定"}else{"占位：需显式打开"};ui.label(format!("{} · {} · {}",binding.title,state,profile.map_or("Profile 不存在 / 无 Profile",|p|p.name.as_str())));}});
                ui.checkbox(&mut self.confirm,"确认上述布局、保留绑定与退出视图；成功后恢复自动保存");
                if ui.add_enabled(ready&&self.confirm,egui::Button::new("提交布局恢复并恢复自动保存")).clicked()&&let Some(c)=connection{if c.restore(plan.clone(),self.epoch){self.confirm=false;}}
            }
            ui.weak("关闭面板不会恢复自动保存。需要重新读取、预览目标并确认；不明提交结果不能重放。窗口创建失败会保留已保存布局，可再次打开本面板。");
        });
        self.open = open;
    }
}
