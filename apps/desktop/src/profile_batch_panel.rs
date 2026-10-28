use crate::{
    daemon_connection::DesktopConnectionConfig, profile_batch_connection::Connection,
    profile_connection::DesktopProfileView,
};
use cshell_application::*;
use cshell_domain::*;
use cshell_ipc::{ProfileBatchOperation as Op, ProfileBatchRequest as Request};
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Default)]
pub(super) struct ProfileBatchPanel {
    pub open: bool,
    connection: Option<Connection>,
    error: Option<String>,
    selected: BTreeSet<ProfileId>,
    patch: ProfileBatchPatch,
    filter: String,
    tag_text: String,
    tag_mode: u8,
    epoch: u64,
    confirm: bool,
    inspect: Option<ProfileId>,
    inspect_epoch: u64,
    seen_applied: Option<u64>,
    seen_preview: Option<[u8; 32]>,
    stamp: Option<(u64, u64)>,
}
impl ProfileBatchPanel {
    pub fn generation(&self) -> Option<u64> {
        self.connection.as_ref().map(|c| c.view().generation)
    }
    pub fn draw(
        &mut self,
        context: &egui::Context,
        config: Option<&DesktopConnectionConfig>,
        profiles: Option<&DesktopProfileView>,
        active: Option<ProfileId>,
    ) -> bool {
        if !self.open {
            return false;
        }
        let mut refresh = false;
        if self.connection.is_none()
            && self.error.is_none()
            && let Some(config) = config
        {
            match Connection::start(config.clone()) {
                Ok(c) => {
                    c.request(
                        Request {
                            operation: Op::Reload as i32,
                            ..Default::default()
                        },
                        0,
                        None,
                        true,
                    );
                    self.connection = Some(c);
                }
                Err(_) => self.error = Some("批量编辑后台线程无法启动".into()),
            }
        }
        context.request_repaint_after(std::time::Duration::from_millis(150));
        let view = self
            .connection
            .as_ref()
            .map(Connection::view)
            .unwrap_or_default();
        let token = view.preview.as_ref().map(|p| p.token);
        if self.seen_preview != token {
            self.seen_preview = token;
            self.confirm = false;
        }
        if view.applied != self.seen_applied {
            self.seen_applied = view.applied;
            if view.applied.is_some() {
                refresh = true;
                self.confirm = false;
            }
        }
        let catalog = profiles.and_then(|v| v.catalog.as_ref());
        let ready = !view.busy
            && view.writable
            && catalog.is_some_and(|c| Some(c.revision) == view.authority);
        if let Some(catalog) = catalog {
            let stamp = (catalog.revision, self.epoch);
            if self.stamp != Some(stamp) {
                self.stamp = Some(stamp);
                self.confirm = false;
            }
        }
        let mut open = self.open;
        let mut edited = false;
        cshell_ui::form::dialog(&context, "Profile 批量编辑与继承来源").open(&mut open).default_size([880.0,740.0]).show(context,|ui|{
            ui.weak("只修改选中 Profile 的元数据和终端覆盖；不启动或重连进程，不改连接与凭据。主题显示按目录刷新生效。先逐项预览，再一次提交。");
            if let Some(error)=self.error.as_ref().or(view.error.as_ref()){ui.colored_label(cshell_ui::theme::WARNING,error);}if view.busy{ui.spinner();}
            if ui.add_enabled(!view.busy,egui::Button::new("明确重新读取目录 / 恢复批量编辑")).clicked(){self.error=None;self.confirm=false;self.epoch=self.epoch.wrapping_add(1);if let Some(c)=&self.connection{c.request(Request{operation:Op::Reload as i32,..Default::default()},self.epoch,None,true);}refresh=true;}
            let Some(catalog)=catalog else{ui.label("等待 Profile 目录");return;};
            ui.horizontal_wrapped(|ui|{ui.label("筛选");ui.text_edit_singleline(&mut self.filter);ui.label(format!("已选 {} / 128",self.selected.len()));if ui.button("清除选中").clicked(){self.selected.clear();edited=true;}if ui.add_enabled(active.is_some(),egui::Button::new("加入当前 Profile")).clicked()&&let Some(id)=active&&self.selected.len()<128{edited|=self.selected.insert(id);}});
            let filter=self.filter.to_lowercase();let visible=catalog.profiles.iter().filter(|p|p.name.to_lowercase().contains(&filter)||p.tags.iter().any(|t|t.to_lowercase().contains(&filter))).take(256).collect::<Vec<_>>();
            ui.horizontal_wrapped(|ui|{if ui.button("选中当前列表（最多 128）").clicked(){for profile in &visible{if self.selected.len()>=128{break;}edited|=self.selected.insert(profile.id);}}ui.weak("列表最多显示前 256 个筛选结果；已选择项不会随筛选自动变化。");});
            egui::ScrollArea::vertical().max_height(150.0).show(ui,|ui|{for profile in visible{ui.horizontal_wrapped(|ui|{let mut selected=self.selected.contains(&profile.id);if ui.add_enabled(selected||self.selected.len()<128,egui::Checkbox::new(&mut selected,&profile.name)).changed(){if selected{self.selected.insert(profile.id);}else{self.selected.remove(&profile.id);}edited=true;}if ui.button("检查继承来源").clicked(){self.inspect=Some(profile.id);self.inspect_epoch=self.inspect_epoch.wrapping_add(1);if let Some(c)=&self.connection{c.request(Request{operation:Op::Inspect as i32,expected_revision:catalog.revision,profile_id:profile.id.as_uuid().as_bytes().to_vec(),..Default::default()},self.inspect_epoch,Some(Arc::new(catalog.export_snapshot())),false);}}});}});
            ui.separator();ui.horizontal_wrapped(|ui|{ui.label("名称前缀");edited|=ui.add(egui::TextEdit::singleline(&mut self.patch.name_prefix).char_limit(64)).changed();ui.label("后缀");edited|=ui.add(egui::TextEdit::singleline(&mut self.patch.name_suffix).char_limit(64)).changed();});
            let old_folder=self.patch.folder.clone();cshell_ui::form::combo(ui, "文件夹").truncate().selected_text(match &self.patch.folder{FieldEdit::Keep=>"保持原值".into(),FieldEdit::Clear=>"移至根目录".into(),FieldEdit::Set(id)=>catalog.folders.iter().find(|f|f.id==*id).map_or_else(||"目录已变化".into(),|f|f.name.clone())}).show_ui(ui,|ui|{ cshell_ui::form::menu(ui);ui.selectable_value(&mut self.patch.folder,FieldEdit::Keep,"保持原值");ui.selectable_value(&mut self.patch.folder,FieldEdit::Clear,"移至根目录");for folder in &catalog.folders{ui.selectable_value(&mut self.patch.folder,FieldEdit::Set(folder.id),&folder.name);}});edited|=old_folder!=self.patch.folder;
            let old_mode=self.tag_mode;ui.horizontal_wrapped(|ui|{for(mode,label)in[(0,"保留标签"),(1,"替换"),(2,"追加"),(3,"移除")]{ui.selectable_value(&mut self.tag_mode,mode,label);}});edited|=self.tag_mode!=old_mode;
            if self.tag_mode!=0{ui.weak("标签每行一个；追加/移除按精确大小写匹配。替换为空表示清空。");edited|=ui.add(egui::TextEdit::multiline(&mut self.tag_text).desired_rows(2).char_limit(4096)).changed();}
            let tags=self.tag_text.lines().map(str::trim).filter(|s|!s.is_empty()).map(str::to_owned).collect::<BTreeSet<_>>();self.patch.tags=match self.tag_mode{1=>TagEdit::Replace(tags),2=>TagEdit::Append(tags),3=>TagEdit::Remove(tags),_=>TagEdit::Keep};
            let old_favorite=self.patch.favorite;ui.horizontal_wrapped(|ui|{ui.label("收藏");ui.selectable_value(&mut self.patch.favorite,None,"保留");ui.selectable_value(&mut self.patch.favorite,Some(true),"设为收藏");ui.selectable_value(&mut self.patch.favorite,Some(false),"取消收藏");});edited|=self.patch.favorite!=old_favorite;
            edited|=text_edit(ui,"Terminal type",&mut self.patch.terminal.terminal_type);edited|=text_edit(ui,"Theme 引用",&mut self.patch.terminal.theme);
            let old_logging=self.patch.terminal.logging.clone();ui.horizontal_wrapped(|ui|{ui.label("Logging 覆盖");for(value,label)in[(FieldEdit::Keep,"保留"),(FieldEdit::Clear,"清除并继承"),(FieldEdit::Set(true),"启用"),(FieldEdit::Set(false),"关闭")]{ui.selectable_value(&mut self.patch.terminal.logging,value,label);}});edited|=old_logging!=self.patch.terminal.logging;
            if edited{self.epoch=self.epoch.wrapping_add(1);self.confirm=false;}
            let draft=ProfileBatchDraft{format:"cshell.profile-batch".into(),version:1,revision:catalog.revision,profiles:self.selected.clone(),patch:self.patch.clone()};
            if ui.add_enabled(ready&&!self.selected.is_empty(),egui::Button::new("生成逐项批量预览")).clicked()&&let Some(c)=&self.connection{match draft.fingerprint().and_then(|_|serde_json::to_vec(&draft).map_err(|_|"批量编码失败")){Ok(document_json)=>{c.request(Request{operation:Op::Preview as i32,expected_revision:catalog.revision,document_json,..Default::default()},self.epoch,Some(Arc::new(catalog.export_snapshot())),false);self.error=None;},Err(error)=>self.error=Some(error.into())}}
            if let Some(preview)=&view.preview{
                let current=view.preview_epoch==self.epoch&&preview.plan.revision==catalog.revision&&draft.fingerprint().ok()==Some(preview.plan.request_sha256);
                if !current{ui.weak("选中项、修改内容或目录版本已变化，请重新预览。");}else{
                    egui::ScrollArea::vertical().max_height(180.0).show(ui,|ui|{for row in &preview.plan.rows{ui.collapsing(format!("{} → {}",row.before.name,row.after.name),|ui|{ui.label(format!("文件夹 {:?} → {:?} · 收藏 {} → {}",row.before.folder_id,row.after.folder_id,row.before.favorite,row.after.favorite));ui.label(format!("标签 {:?} → {:?}",row.before.tags,row.after.tags));settings(ui,"之前",&row.settings_before,&catalog.folders);settings(ui,"之后",&row.settings_after,&catalog.folders);});}});
                    ui.weak("核对以上修改后，点击应用；变更期间不允许提交过期预览。");let now=SystemTime::now().duration_since(UNIX_EPOCH).map_or(u64::MAX,|t|t.as_secs());
                    if ui.add_enabled(ready&&now<preview.expires_unix,egui::Button::new(format!("应用到 {} 个会话",preview.plan.rows.len()))).clicked()&&let Some(c)=&self.connection&&let Ok(sha)=preview.plan.fingerprint(){c.request(Request{operation:Op::Apply as i32,expected_revision:preview.plan.revision,preview_token:preview.token.to_vec(),review_sha256:sha.to_vec(),confirmed:true,..Default::default()},self.epoch,None,false);self.confirm=false;}
                }
            }
            if let Some(report)=&view.settings&&view.settings_epoch==self.inspect_epoch&&self.inspect==Some(report.profile.id){if report.revision==catalog.revision{ui.separator();ui.heading(format!("{}：最终值与来源",report.profile.name));settings(ui,"当前",&report.settings,&catalog.folders);}else{ui.weak("继承来源检查版本已过期，请重新检查。");}}
        });
        self.open = open;
        refresh
    }
}
fn text_edit(ui: &mut egui::Ui, label: &str, value: &mut FieldEdit<String>) -> bool {
    let before = value.clone();
    let mut mode = match value {
        FieldEdit::Keep => 0,
        FieldEdit::Set(_) => 1,
        FieldEdit::Clear => 2,
    };
    let previous = mode;
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.selectable_value(&mut mode, 0, "保留");
        ui.selectable_value(&mut mode, 1, "设置");
        ui.selectable_value(&mut mode, 2, "清除并继承");
    });
    if mode != previous {
        *value = match mode {
            1 => FieldEdit::Set(String::new()),
            2 => FieldEdit::Clear,
            _ => FieldEdit::Keep,
        };
    }
    if let FieldEdit::Set(text) = value {
        ui.add(egui::TextEdit::singleline(text).char_limit(128));
    }
    *value != before
}
fn origin(source: SettingSource, folders: &[ProfileFolder]) -> String {
    match source {
        SettingSource::Global => "全局默认".into(),
        SettingSource::Profile(_) => "Profile 显式覆盖".into(),
        SettingSource::Tag(id) => format!("标签策略 {id}"),
        SettingSource::Host(id) => format!("Host {id}"),
        SettingSource::Temporary => "临时覆盖".into(),
        SettingSource::Folder(id) => folders
            .iter()
            .find(|f| f.id == id)
            .map_or_else(|| format!("文件夹 {id}"), |f| format!("文件夹 {}", f.name)),
    }
}
fn settings(
    ui: &mut egui::Ui,
    label: &str,
    value: &ResolvedTerminalSettings,
    folders: &[ProfileFolder],
) {
    ui.label(format!(
        "{label} · TERM={} [{}] · Theme={} [{}] · Logging={} [{}]",
        value.terminal_type.value,
        origin(value.terminal_type.source, folders),
        value.theme.value,
        origin(value.theme.source, folders),
        value.logging.value,
        origin(value.logging.source, folders)
    ));
}
