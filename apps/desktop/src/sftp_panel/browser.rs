use super::*;
use super::visuals::{self,Icon};
use cshell_domain::SftpQueueItemId;
use std::collections::BTreeSet;
mod columns;
mod navigation;
mod transfer_table;
mod selection;
mod operations;
mod shared_local;
mod sync;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod layout_memory_tests;
#[cfg(test)]
mod visual_feedback_tests;

#[derive(Default)]
pub(super) struct BrowserState {
    sync:sync::Panel,
    navigation: navigation::Navigation,
    selection: [selection::Selection; 2],
    operation: Option<operations::Dialog>,
    staged: Vec<SftpQueueDraft>,
    submitted_drafts: Vec<SftpQueueDraft>,
    auto_submit: bool,
    tree:Option<cshell_application::SftpTreeStart>,
    tree_auto:bool,
    tree_sent:Option<(SessionId,cshell_application::SftpTreeStart)>,
    completed_trees:BTreeSet<SessionId>,
    local_history: Vec<String>,
    remote_history: Vec<String>,
    sort: [Option<columns::SortState>; 2],
    error: Option<String>,
    submitted_revision: Option<u64>,
    submitted_existing: BTreeSet<SftpQueueItemId>,
    submitted_generation: u64,
    local_loaded: Option<String>,
    remote_loaded: Option<String>,
    pub(super) local_rect: Option<egui::Rect>,
    pub(super) remote_rect: Option<egui::Rect>,
    table: transfer_table::TableState,
    #[cfg(test)] drop_feedback:[Option<visuals::DropFeedback>;2],
}

impl BrowserState { pub(super) fn set_layout_error(&mut self,error:String) {self.error=Some(error);} }

#[cfg(test)]
mod terminal_path_tests {
    use super::*;
    #[test]
    fn terminal_file_request_waits_for_exact_session_and_stages_download_only_once() {
        let ctx=egui::Context::default();let session=SessionId::new();let mut panel=SftpPanel::default();
        panel.queue_terminal_path(session,"/var/log/test.log".into(),true);
        let mut view=SftpView{session_id:Some(session),connection_phase:crate::sftp_connection::ConnectionPhase::Connected,generation:1,..Default::default()};
        assert!(matches!(panel.process_terminal_path(&ctx,&view),Some(SftpClientCommand::BrowseManaged(id,ref options)) if id==session&&options.path=="/var/log"));
        assert!(panel.process_terminal_path(&ctx,&view).is_none());assert!(panel.browser.staged.is_empty());
        view.remote_loaded_directory=Some("/var/log".into());view.generation=2;
        view.remote_requested_directory=Some("/var/log".into());
        view.local_directory=Some(std::env::temp_dir().to_string_lossy().into());
        view.entries=vec![cshell_ipc::SftpDirectoryEntry{name:"test.log".into(),path:"/var/log/test.log".into(),kind:1,..Default::default()}];
        panel.process_terminal_path(&ctx,&view);
        assert_eq!(panel.browser.staged.len(),1);assert!(panel.browser.auto_submit);
        assert_eq!(panel.browser.staged[0].remote_path,"/var/log/test.log");
        assert_eq!(panel.browser.staged[0].direction,SftpQueueDirection::Download);
        panel.process_terminal_path(&ctx,&view);assert_eq!(panel.browser.staged.len(),1);
        panel.queue_terminal_path(SessionId::new(),"/different".into(),true);
        panel.process_terminal_path(&ctx,&view);assert!(panel.terminal_path.is_none());assert!(panel.browser.error.as_deref().unwrap().contains("会话已变化"));
    }
}

#[derive(Clone)]
struct DragFiles(Vec<DragFile>);

#[derive(Clone)]
struct DragFile {
    session: SessionId,
    local: bool,
    path: String,
    name: String,
}

pub(super) fn bytes(value: u64) -> String {
    let value = value as f64;
    if value >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", value / (1024.0 * 1024.0 * 1024.0))
    } else if value >= 1024.0 * 1024.0 {
        format!("{:.1} MB", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.1} KB", value / 1024.0)
    } else {
        format!("{} B", value as u64)
    }
}

fn file_icon(ui: &mut egui::Ui, directory: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 24.0), egui::Sense::hover());
    let center = rect.center();
    let stroke = egui::Stroke::new(
        1.2,
        if directory {
            cshell_ui::theme::ACCENT
        } else {
            ui.visuals().weak_text_color()
        },
    );
    if directory {
        let points = [
            (-7.0, -4.0),
            (-2.0, -4.0),
            (0.0, -2.0),
            (7.0, -2.0),
            (7.0, 6.0),
            (-7.0, 6.0),
        ];
        ui.painter().add(egui::Shape::closed_line(
            points
                .into_iter()
                .map(|(x, y)| center + egui::vec2(x, y))
                .collect(),
            stroke,
        ));
    } else {
        ui.painter().rect_stroke(
            egui::Rect::from_center_size(center, egui::vec2(10.0, 14.0)),
            1.0,
            stroke,
            egui::StrokeKind::Inside,
        );
        for y in [-2.0, 2.0] {
            ui.painter().line_segment(
                [center + egui::vec2(-2.0, y), center + egui::vec2(2.0, y)],
                stroke,
            );
        }
    }
}

fn loaded_path(view: &SftpView, local: bool) -> Option<&str> {
    if local {
        view.local_directory.as_deref()
    } else {
        view.remote_loaded_directory.as_deref()
    }
}
fn parent_path(local: bool, path: &str) -> String {
    if local {
        std::path::Path::new(path)
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new(path))
            .to_string_lossy()
            .into_owned()
    } else {
        path.trim_end_matches('/')
            .rsplit_once('/')
            .map(|(p, _)| if p.is_empty() { "/" } else { p })
            .unwrap_or("/")
            .into()
    }
}
fn remember(history: &mut Vec<String>, path: &str) {
    history.retain(|old| old != path);
    history.insert(0, path.into());
    history.truncate(12);
}
fn directory_choices(
    local: bool,
    path: &str,
    history: &[String],
    entries: &[cshell_ipc::SftpDirectoryEntry],
) -> Vec<String> {
    let mut choices = Vec::new();
    if !path.is_empty() {
        let mut ancestor = path.to_owned();
        for _ in 0..32 {
            choices.push(ancestor.clone());
            let parent = parent_path(local, &ancestor);
            if parent == ancestor {
                break;
            }
            ancestor = parent;
        }
    }
    for item in history
        .iter()
        .cloned()
        .chain(
            entries
                .iter()
                .filter(|entry| entry.kind == 2)
                .map(|entry| entry.path.clone()),
        )
        .take(64)
    {
        if !choices.contains(&item) {
            choices.push(item);
        }
    }
    choices
}

impl SftpPanel {
    #[cfg(test)]
    pub(super) fn preview_directory_sync(&mut self,session:SessionId,id:SessionId){self.browser.sync=sync::Panel::preview(session,id);}
    pub(super) fn open_directory_sync(&mut self){self.browser.sync.open();}
    pub(crate) fn process_terminal_path(&mut self,ctx:&egui::Context,view:&SftpView)->Option<SftpClientCommand> {
        let mut request=self.terminal_path.take()?;
        if view.session_id!=Some(request.session) {
            self.browser.error=Some("SFTP 会话已变化，终端文件操作已取消，请重新选择。".into());return None;
        }
        if view.connection_phase!=crate::sftp_connection::ConnectionPhase::Connected {
            self.terminal_path=Some(request);return None;
        }
        let path=request.path.trim_end_matches('/');
        let (parent,name)=if path.is_empty(){("/","")}else{path.rsplit_once('/').map(|(p,n)|(if p.is_empty(){"/"}else{p},n)).unwrap_or(("/",path))};
        if request.phase==0 {
            request.phase=1;request.generation=view.generation;
            let command=self.navigate(request.session,false,parent.into());
            self.terminal_path=Some(request);return Some(command);
        }
        if view.remote_requested_directory.as_deref().is_some_and(|p|p!=parent) {
            self.browser.error=Some("SFTP 已切换目录，终端路径操作已取消。".into());return None;
        }
        if view.remote_loaded_directory.is_none()||view.remote_requested_directory.as_deref()!=Some(parent)||view.generation<=request.generation||view.workspace_busy {
            if view.error.is_some()&&view.generation>request.generation {self.browser.error=Some("无法定位终端路径，请检查 SFTP 错误信息。".into());}
            else {self.terminal_path=Some(request);}
            return None;
        }
        if name.is_empty() {return None;}
        if let Some(entry)=view.entries.iter().find(|e|e.name==name) {
            self.browser.selection[1].context(&entry.path);
            if request.download {
                if view.local_directory.is_none() {self.terminal_path=Some(request);return None;}
                self.stage_file(request.session,view,&DragFile{session:request.session,local:false,path:entry.path.clone(),name:entry.name.clone()},true);
                ctx.request_repaint();
            }
        } else if let Some(cursor)=&view.remote_cursor {
            request.generation=view.generation;
            let command=SftpClientCommand::BrowseNext(request.session,false,cursor.clone());
            self.terminal_path=Some(request);return Some(command);
        } else {self.browser.error=Some(format!("远端目录中未找到：{}",request.path));}
        None
    }
    pub fn request_failed(&mut self, transfer: bool) {
        if transfer {
            self.request_rejected();
        } else {
            self.browser.error = Some("后台正忙，请稍后重试当前操作。".into());
            if let Some(dialog) = &mut self.browser.operation { dialog.rejected(); }
        }
    }
    fn submit_dropped(
        &mut self,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        if self.browser.tree_auto && self.browser.operation.is_none() && command.is_none() && !view.workspace_busy {
            if let Some(options)=self.browser.tree.take(){let id=SessionId::new();self.browser.tree_sent=Some((id,options.clone()));self.browser.tree_auto=false;*command=Some(SftpClientCommand::Tree(session,SftpOperation::TreeStart,id,Some(options)));return;}
        }
        if self.browser.operation.is_some()
            || !self.browser.auto_submit
            || self.browser.staged.is_empty()
            || self.browser.submitted_revision.is_some()
            || command.is_some()
            || view.queue_busy
            || view.workspace_busy
            || !view.queue_writable
        {
            return;
        }
        let Some(queue) = view
            .queue
            .as_ref()
            .filter(|queue| queue.migrated_to.is_none())
        else {
            return;
        };
        self.browser.submitted_revision = Some(queue.revision);
        self.browser.submitted_generation = view.generation;
        self.browser.submitted_existing = queue.items.iter().map(|item| item.id).collect();
        self.browser.submitted_drafts = std::mem::take(&mut self.browser.staged);
        self.browser.auto_submit = false;
        *command = Some(SftpClientCommand::TransferBatch(
            session,
            self.browser.submitted_drafts.clone(),
            queue.revision,
            !view.queue_hold,
        ));
    }
    pub fn request_rejected(&mut self) {
        self.browser.sync.request_rejected();
        if let Some((_,options))=self.browser.tree_sent.take(){self.browser.tree=Some(options);self.browser.tree_auto=false;}
        self.advanced.editor_request_rejected();
        self.browser
            .staged
            .splice(0..0, std::mem::take(&mut self.browser.submitted_drafts));
        self.browser.auto_submit = false;
        self.browser.submitted_revision = None;
        self.browser.submitted_existing.clear();
        self.browser.error =
            Some("请求未提交：任务状态已变化或后台正忙，请刷新任务后重试。".into());
    }
    pub(super) fn draw_browser(
        &mut self,
        ui: &mut egui::Ui,
        context: &egui::Context,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        if self.browser.tree_sent.as_ref().is_some_and(|(id,_)|view.trees.contains_key(id)){self.browser.tree_sent=None;}
        for tree in view.trees.values().filter(|r|r.session==session&&r.state==cshell_application::TerminalUploadState::Succeeded) {
            if command.is_none() && !self.browser.completed_trees.contains(&tree.id) {
                let local=tree.direction==SftpQueueDirection::Download;
                if loaded_path(view,local)==Some(tree.destination.as_str()){*command=Some(self.navigate(session,local,tree.destination.clone()));}
                self.browser.completed_trees.insert(tree.id);
            }
        }
        if self.browser.local_loaded != view.local_directory {
            self.browser.local_loaded.clone_from(&view.local_directory);
            if let Some(path) = &view.local_directory {
                self.local_directory.clone_from(path);
                remember(&mut self.browser.local_history, path);
                self.browser.navigation.history[0].loaded(path);
            }
        }
        if self.browser.remote_loaded != view.remote_loaded_directory {
            self.browser
                .remote_loaded
                .clone_from(&view.remote_loaded_directory);
            if let Some(path) = &view.remote_loaded_directory {
                self.remote_directory.clone_from(path);
                remember(&mut self.browser.remote_history, path);
                self.browser.navigation.history[1].loaded(path);
            }
        }
        if let Some(revision) = self.browser.submitted_revision
            && !view.queue_busy
            && view.queue_writable
            && (view.generation != self.browser.submitted_generation
                || view
                    .queue
                    .as_ref()
                    .is_some_and(|queue| queue.revision > revision))
        {
            if view.queue.as_ref().is_some_and(|queue| {
                queue.revision > revision
                    && self.browser.submitted_drafts.iter().all(|draft| {
                        queue.items.iter().any(|item| {
                            !self.browser.submitted_existing.contains(&item.id)
                                && item.draft == *draft
                        })
                    })
            }) {
                self.browser.submitted_drafts.clear();
                self.browser.error = None;
            }
            if !self.browser.submitted_drafts.is_empty() {
                self.browser
                    .staged
                    .splice(0..0, std::mem::take(&mut self.browser.submitted_drafts));
                self.browser.auto_submit = false;
                self.browser.error = Some("文件尚未加入，可点击重试。".into());
            }
            self.browser.submitted_revision = None;
            self.browser.submitted_existing.clear();
        }
        for (side, local) in [(0, true), (1, false)] {
            self.browser.selection[side].sync(loaded_path(view, local),
                if local { &view.local_entries } else { &view.entries });
            let selected = if local { &mut self.local_file } else { &mut self.remote_file };
            if !self.browser.selection[side].paths.contains(selected) {
                *selected = self.browser.selection[side].paths.first().cloned().unwrap_or_default();
            }
        }
        if self.browser.operation.is_none() && !self.browser.sync.blocks() {
            self.navigation_shortcuts(ui, session, view, command);
            self.file_shortcuts(ui, session, view, command);
        }
        self.advance_file_operation(session, view, command);
        #[cfg(test)] self.layout.column_handles.clear();
        let verification=view.connection_profile.and_then(|p|self.verification_overrides.get(&p).copied()).unwrap_or_else(||crate::zmodem_settings::load().sftp_verification);
        self.browser.sync.show(ui,session,view,verification,command);
        let whole=ui.available_rect_before_wrap();
        let dock_height=self.layout.transfer_height(whole.height());
        let dock=egui::Panel::bottom("sftp-transfer-dock")
            .exact_size(dock_height)
            .frame(egui::Frame::NONE.fill(cshell_ui::theme::PANEL)
                .stroke(egui::Stroke::new(1.0,cshell_ui::theme::LINE)).inner_margin(8))
            .show(ui,|ui|{ui.push_id(("queue-session",session),|ui|self.transfer_dock(ui,session,view,command));});
        let panes=ui.available_rect_before_wrap();
        let [left,right,divider]=self.layout.pane_rects(panes);
        for (local,rect) in [(true,left),(false,right)] {
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)),|ui| {
                self.file_pane(ui,session,view,local,panes.height(),command);
            });
        }
        self.layout.split_handle(ui,panes,divider);
        self.layout.transfer_handle(ui,whole,dock.response.rect.top());
        let native = context.input(|input| input.raw.dropped_files.clone());
        if self.browser.operation.is_none() && !self.browser.sync.blocks() && !native.is_empty() {
            context.request_repaint();
            if self.browser.remote_rect.is_some_and(|rect| {
                context
                    .pointer_latest_pos()
                    .is_some_and(|pos| rect.contains(pos) && context.layer_id_at(pos)==Some(ui.layer_id()))
            }) {
                if native.iter().any(|f|f.path().is_dir()) {
                    self.stage_tree(session,view,true,native.iter().take(129).map(|f|f.path().to_string_lossy().into_owned()).collect());
                } else {
                for file in native.into_iter().take(129) {
                    let path = file.path();
                    if !path.is_file() {
                        self.browser.error =
                            Some("请拖入普通文件；文件夹请先打开后选择其中的文件。".into());
                        continue;
                    }
                    let Some(path_text) = path.to_str() else {
                        self.browser.error = Some("此文件路径无法读取。".into());
                        continue;
                    };
                    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                        continue;
                    };
                    self.stage_file(
                        session,
                        view,
                        &DragFile {
                            session,
                            local: true,
                            path: path_text.into(),
                            name: name.into(),
                        },
                        false,
                    );
                }
                }
            } else {
                self.browser.error = Some("请将本地文件拖到右侧远端列表上传。".into());
            }
        }
        self.submit_dropped(session, view, command);
        if view.queue.as_ref().is_some_and(|queue| {
            queue
                .items
                .iter()
                .any(|item| item.state == SftpQueueState::Running)
        }) {
            context.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    fn file_pane(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        local: bool,
        height: f32,
        command: &mut Option<SftpClientCommand>,
    ) {
        let width = ui.available_width();
        let selected=&self.browser.selection[usize::from(!local)].paths;
        let entries=if local {&view.local_entries}else{&view.entries};
        let connected=view.connection_profile.is_none() || view.connection_phase==crate::sftp_connection::ConnectionPhase::Connected;
        let can_transfer=connected && loaded_path(view,local).is_some() && loaded_path(view,!local).is_some()
            && !selected.is_empty() && selected.len()<=128
            && entries.iter().filter(|entry|matches!(entry.kind,1|2)&&selected.contains(&entry.path)).count()==selected.len();
        ui.push_id(
            if local {
                "sftp-local-pane"
            } else {
                "sftp-remote-pane"
            },
            |ui| {
                let response = egui::Frame::NONE
                    .stroke(egui::Stroke::new(1.0,
                        if self.browser.navigation.active == usize::from(!local) {
                            cshell_ui::theme::ACCENT
                        } else { cshell_ui::theme::LINE }))
                    .fill(cshell_ui::theme::PANEL)
                    .inner_margin(8)
                    .show(ui, |ui| {
                        ui.set_min_width((width - 18.0).max(1.0));
                        ui.set_max_width((width - 18.0).max(1.0));
                        ui.set_min_height((height - 18.0).max(100.0));
                        let tabs_height=if !local {self.draw_remote_tabs(ui)}else{0.0};
                        let online=view.connection_profile.is_none() || view.connection_phase==crate::sftp_connection::ConnectionPhase::Connected;
                        if !local && !online {
                            ui.add_space(16.0);
                            ui.horizontal(|ui|{if view.connection_phase.busy(){ui.spinner();}ui.colored_label(cshell_ui::theme::WARNING,egui::RichText::new(&view.status).strong());});
                            ui.label("使用工具栏重新连接。当前主机以外的标签和 SSH 终端不受影响。");
                            return;
                        }
                        ui.horizontal(|ui| {
                            ui.strong(if local {
                                "本地文件"
                            } else {
                                "远端文件"
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if visuals::tool(ui,if local {Icon::Upload}else{Icon::Download},if local {"上传"}else{"下载"},can_transfer,Some(visuals::direction_color(!local)),false)
                                        .on_hover_text(if local {"将选中文件上传到右侧当前目录 · 本地 → 远端"}else{"将选中文件下载到左侧当前目录 · 远端 → 本地"}).clicked() {
                                        self.transfer_selection(session,view,local);
                                    }
                                    if !local {
                                        let can_edit=!view.workspace_busy && view.entries.iter().any(|e|e.path==self.remote_file && e.kind==1);
                                        if visuals::tool(ui,Icon::Edit,"编辑",can_edit,None,width<440.0).on_hover_text("编辑选中文件 · F4；保存上传 · Ctrl+S").clicked(){
                                            let path=self.remote_file.clone();*command=self.open_remote_editor(session,view,&path);
                                        }
                                    }
                                    if visuals::tool(ui,Icon::Refresh,"刷新",true,None,true)
                                        .on_hover_text("刷新当前目录 · F5")
                                        .clicked()
                                    {
                                        *command = Some(
                                            self.navigate(
                                                session,
                                                local,
                                                loaded_path(view, local)
                                                    .unwrap_or(if local { "" } else { "/" })
                                                    .into(),
                                            ),
                                        );
                                    }
                                    if visuals::tool(ui,Icon::Up,"↑",true,None,true).on_hover_text("上级目录 · Alt+Up").clicked()
                                    {
                                        if let Some(path) = loaded_path(view, local) {
                                            *command = Some(self.navigate(
                                                session,
                                                local,
                                                parent_path(local, path),
                                            ));
                                        }
                                    }
                                },
                            );
                        });
                        if local {self.navigation_bar(ui,session,view,local,command);} else {ui.push_id(session,|ui|self.navigation_bar(ui,session,view,local,command));}
                        let side = usize::from(!local);
                        let mut sort = self.browser.sort[side].unwrap_or_else(|| columns::SortState::initial(local));
                        ui.horizontal(|ui| {
                            if local { ui.weak(if width < 440.0 { "表头排序" } else { "点击表头切换升序 / 降序" }); }
                            else { ui.checkbox(&mut sort.folders_first, "文件夹优先"); }
                            ui.weak(format!("已选 {} 项", self.browser.selection[side].paths.len())).on_hover_text("多选和排序作用于当前页");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                visuals::menu(ui,"操作",260.0,|ui|self.file_menu(ui,session,view,local,command));
                            });
                        });
                        let mut column_widths = if self.layout.value.columns[side].is_empty() {
                            columns::widths(local,(width-18.0).max(1.0))
                        } else {self.layout.value.columns[side].clone()};
                        let table_width = column_widths.iter().sum::<f32>() + 8.0 * (column_widths.len() - 1) as f32;
                        egui::ScrollArea::horizontal()
                            .id_salt(("file-columns",(!local).then_some(session)))
                            .max_width((width - 18.0).max(1.0))
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.set_width(table_width);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    for (index, &column) in columns::columns(local).iter().enumerate() {
                                         let width=column_widths[index];
                                        let header = ui.add_sized([width, 24.0], egui::Button::new(sort.header(column))
                                            .selected(sort.column == column).frame(false).truncate())
                                            .on_hover_text(if column == columns::Column::Modified { "按当前页修改时间排序；时间使用本机时区" }
                                                else if column == columns::Column::Owner { "按服务器提供的 UID 排序；悬停查看 UID / GID" }
                                                else { "点击按本列排序，再次点击反转顺序" })
                                            ;
                                        columns::clip_accessibility(ui, &header);
                                        if header.clicked() { sort.select(column); }
                                        self.layout.column_handle(ui,local,index,header.rect,&mut column_widths);
                                    }
                                });
                                ui.separator();
                                let entries = sort.sorted(if local { &view.local_entries } else { &view.entries });
                        let list_height = (height - 185.0 - tabs_height).max(24.0);
                        egui::ScrollArea::vertical()
                            .id_salt(("files",(!local).then_some(session)))
                            .min_scrolled_height(24.0)
                            .auto_shrink([false, false])
                            .max_height(list_height)
                            .show(ui, |ui| {
                                if let Some(path) = loaded_path(view, local) {
                                    for (name, target, hint) in [
                                        (".", path.to_owned(), "当前目录，双击刷新"),
                                        ("..", parent_path(local, path), "上一级目录，双击进入"),
                                    ] {
                                        let entry = cshell_ipc::SftpDirectoryEntry { name: name.into(), path: target.clone(), kind: 2, ..Default::default() };
                                        let response = columns::row(ui, &entry, local, &column_widths, false);
                                        if response.double_clicked() || response.triple_clicked() {
                                            *command = Some(self.navigate(session, local, target));
                                        }
                                        response.on_hover_text(hint);
                                    }
                                }
                                for entry in &entries {
                                    let selected = self.browser.selection[side].paths.contains(&entry.path);
                                    let clicked = columns::row(ui, entry, local, &column_widths, selected);
                                    if matches!(entry.kind,1|2) && clicked.drag_started() {
                                        let files = self.drag_selection(session, view, local, entry);
                                        if files.is_empty() {
                                            self.browser.error = Some("请选择 1–128 个普通文件后拖动；目录请先打开。".into());
                                        } else { clicked.dnd_set_drag_payload(DragFiles(files)); }
                                    }
                                    if self.browser.selection[side].scroll_requested && self.browser.selection[side].cursor.as_ref()==Some(&entry.path) {clicked.scroll_to_me(Some(egui::Align::Center));self.browser.selection[side].scroll_requested=false;}
                                    clicked.clone().on_hover_text(&entry.path);
                                    if clicked.clicked() {
                                        self.browser.selection[side].click(&entry.path, &entries, ui.input(|input| input.modifiers));
                                        self.browser.navigation.active = side;
                                        if local { self.local_file.clone_from(&entry.path); }
                                        else { self.remote_file.clone_from(&entry.path); }
                                    }
                                    if clicked.secondary_clicked() {
                                        self.browser.selection[side].context(&entry.path);
                                        self.browser.navigation.active = side;
                                        if local { self.local_file.clone_from(&entry.path); }
                                        else { self.remote_file.clone_from(&entry.path); }
                                    }
                                    clicked.context_menu(|ui| self.file_menu(ui, session, view, local, command));
                                    if (clicked.double_clicked() || clicked.triple_clicked()) && entry.kind == 2 {
                                        *command = Some(self.navigate(session, local, entry.path.clone()));
                                    }
                                }
                                let loaded = if local {
                                    view.local_directory.is_some()
                                } else {
                                    view.remote_loaded_directory.is_some()
                                };
                                if !loaded
                                    && ui
                                        .button(if local {
                                            "载入本地文件"
                                        } else {
                                            "载入远端文件"
                                        })
                                        .clicked()
                                {
                                    *command = Some(self.navigate(
                                        session,
                                        local,
                                        if local { String::new() } else { "/".into() },
                                    ));
                                }
                                let cursor = if local {
                                    &view.local_cursor
                                } else {
                                    &view.remote_cursor
                                };
                                if let Some(cursor) = cursor
                                    && ui.button("下一页").clicked()
                                {
                                    *command = Some(SftpClientCommand::BrowseNext(
                                        session,
                                        local,
                                        cursor.clone(),
                                    ));
                                }
                                let blank = ui.allocate_response(egui::vec2(ui.available_width(), (list_height - ui.min_rect().height()).max(24.0)), egui::Sense::click());
                                if blank.secondary_clicked() { self.browser.navigation.active = side; }
                                blank.context_menu(|ui| self.file_menu(ui, session, view, local, command));
                            });
                            });
                        self.browser.sort[side] = Some(sort);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(if local {"接收下载 · 远端 → 本地"}else{"接收上传 · 本地 → 远端"})
                                .color(visuals::direction_color(local))
                                .small(),
                            )
                            .truncate(),
                        ).on_hover_text("Ctrl/Shift 多选 · 右键操作 · F2 重命名；拖到此侧的当前目录即开始传输");
                    });
                if ui.input(|input| input.pointer.any_pressed())
                    && ui.ctx().pointer_latest_pos().is_some_and(|pos| response.response.rect.contains(pos)
                        && ui.ctx().layer_id_at(pos) == Some(ui.layer_id()))
                { self.browser.navigation.active = usize::from(!local); }
                if local {
                    self.browser.local_rect = Some(response.response.rect);
                } else {
                    self.browser.remote_rect = Some(response.response.rect);
                }
                let payload=response.response.dnd_hover_payload::<DragFiles>();
                let native=ui.input(|input|input.raw.hovered_files.len());
                let target_visible=response.response.contains_pointer() && self.browser.operation.is_none()
                    && ui.ctx().pointer_latest_pos().is_some_and(|pos|ui.ctx().layer_id_at(pos)==Some(ui.layer_id()));
                let feedback=if target_visible {
                    if let Some(files)=&payload {
                        Some(visuals::DropFeedback::new(local,loaded_path(view,local),files.0.first().is_some_and(|f|f.local),
                            files.0.iter().all(|f|f.session==session),files.0.len()))
                    } else if native>0 {Some(visuals::DropFeedback::new(local,loaded_path(view,local),true,true,native))}else{None}
                } else {None};
                if let Some(feedback)=&feedback {feedback.paint(ui,response.response.rect);}
                #[cfg(test)] {self.browser.drop_feedback[usize::from(!local)]=feedback.clone();}
                if let Some(files)=response.response.dnd_release_payload::<DragFiles>() {
                    if feedback.as_ref().is_some_and(|f|f.allowed) {
                        let source=if local{&view.entries}else{&view.local_entries};
                        if files.0.iter().any(|f|source.iter().any(|e|e.path==f.path&&e.kind==2)) {
                            self.stage_tree(session,view,!local,files.0.iter().map(|f|f.path.clone()).collect());
                        }else{for file in &files.0 {self.stage_file(session,view,file,local);}}
                    } else if let Some(feedback)=feedback {self.browser.error=Some(feedback.caption);}
                    ui.ctx().request_repaint();
                }
            },
        );
    }

    fn stage_tree(&mut self,session:SessionId,view:&SftpView,local:bool,paths:Vec<String>){
        if view.session_id!=Some(session)||paths.is_empty()||paths.len()>128 {self.browser.error=Some("请选择当前会话的 1–128 个文件或文件夹".into());return;}
        let Some(destination)=loaded_path(view,!local) else{self.browser.error=Some("请先载入目标目录".into());return;};
        if self.browser.tree.is_some()||self.browser.tree_sent.is_some(){self.browser.error=Some("上一批目录任务正在加入，请稍后再拖入".into());return;}
        let verification=view.connection_profile.and_then(|p|self.verification_overrides.get(&p).copied()).unwrap_or_else(||crate::zmodem_settings::load().sftp_verification);
        self.browser.tree=Some(cshell_application::SftpTreeStart{direction:if local{SftpQueueDirection::Upload}else{SftpQueueDirection::Download},paths,destination:destination.into(),verification});self.browser.tree_auto=true;self.browser.error=None;
    }
    fn stage_file(
        &mut self,
        session: SessionId,
        view: &SftpView,
        file: &DragFile,
        local_target: bool,
    ) {
        if file.session != session || view.session_id != Some(session) {
            self.browser.error = Some("连接会话已变化，请重新拖动文件。".into());
            return;
        }
        if file.local == local_target {
            return;
        }
        let source=if file.local{&view.local_entries}else{&view.entries};
        if source.iter().any(|e|e.path==file.path&&e.kind==2){self.stage_tree(session,view,file.local,vec![file.path.clone()]);return;}
        if self.browser.staged.len() >= 128 {
            self.browser.error = Some("一次最多选择 128 个文件。".into());
            return;
        }
        // The filename may never escape the loaded destination directory.
        if file.name.is_empty()
            || file.name.contains(['/', '\\'])
            || file.name == "."
            || file.name == ".."
        {
            self.browser.error = Some("文件名无效。".into());
            return;
        }
        let verification=view.connection_profile.and_then(|profile|self.verification_overrides.get(&profile).copied())
            .unwrap_or_else(||crate::zmodem_settings::load().sftp_verification);
        let draft = if local_target {
            view.local_directory
                .as_ref()
                .ok_or("请先载入左侧本地目标目录。")
                .and_then(|directory| {
                    self.advanced.draft(
                        SftpQueueDirection::Download,
                        std::path::Path::new(directory)
                            .join(&file.name)
                            .to_string_lossy()
                            .into_owned(),
                        file.path.clone(),
                        verification,
                    )
                })
        } else {
            view.remote_loaded_directory
                .as_ref()
                .ok_or("请先载入右侧远端目标目录。")
                .and_then(|directory| {
                    self.advanced.draft(
                        SftpQueueDirection::Upload,
                        file.path.clone(),
                        format!("{}/{}", directory.trim_end_matches('/'), file.name),
                        verification,
                    )
                })
        };
        match draft {
            Ok(draft) => {
                if !self
                    .browser
                    .staged
                    .iter()
                    .chain(&self.browser.submitted_drafts)
                    .any(|old| old == &draft)
                {
                    self.browser.staged.push(draft);
                    self.browser.auto_submit = true;
                }
                self.browser.error = None;
            }
            Err(error) => self.browser.error = Some(error.into()),
        }
    }

}
