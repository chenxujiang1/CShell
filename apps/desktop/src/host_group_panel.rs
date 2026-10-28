use crate::host_group_connection::{HostGroupConnection, HostGroupView, HostRevision};
use cshell_application::{
    HostCatalogChange, HostSelector, MAX_HOST_SELECTOR_BYTES, apply_host_change,
};
use cshell_domain::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(crate) struct HostGroupPanel {
    pub(crate) open: bool,
    filter: String,
    selected: BTreeSet<HostId>,
    selection_mode: u8,
    query: String,
    query_group: Option<HostGroupId>,
    host_id: Option<HostId>,
    name: String,
    address: String,
    port: u16,
    profile: Option<ProfileId>,
    tags: String,
    labels: String,
    platform: HostPlatformHint,
    host_base: Option<HostRevision>,
    group_id: Option<HostGroupId>,
    group_name: String,
    parent: Option<HostGroupId>,
    dynamic: bool,
    group_selector: String,
    members: BTreeSet<HostId>,
    group_base: Option<HostRevision>,
    delete_host: bool,
    delete_group: bool,
    status: String,
}
impl HostGroupPanel {
    pub(crate) fn selection(&self) -> Option<HostSelection> {
        match self.selection_mode {
            0 => Some(HostSelection::Selector {
                expression: self.query.clone(),
            }),
            1 => self.query_group.map(|id| HostSelection::Group { id }),
            2 => Some(HostSelection::Explicit {
                hosts: self.selected.clone(),
            }),
            _ => None,
        }
    }
    fn load_host(&mut self, host: &HostRecord, revision: HostRevision) {
        self.host_id = Some(host.id);
        self.name.clone_from(&host.name);
        self.address.clone_from(&host.address);
        self.port = host.port;
        self.profile = Some(host.profile_id);
        self.tags = host.tags.iter().cloned().collect::<Vec<_>>().join("\n");
        self.labels = host
            .labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.platform = host.platform_hint;
        self.host_base = Some(revision);
        self.delete_host = false;
    }
    fn record(&self) -> Result<HostRecord, &'static str> {
        let profile_id = self.profile.ok_or("请选择一个 SSH Profile")?;
        let mut labels = BTreeMap::new();
        for line in self.labels.lines().filter(|line| !line.is_empty()) {
            let (key, value) = line.split_once('=').ok_or("属性需为 key=value，每行一项")?;
            if labels.insert(key.into(), value.into()).is_some() {
                return Err("属性键重复");
            }
        }
        Ok(HostRecord {
            id: self.host_id.unwrap_or_default(),
            name: self.name.clone(),
            address: self.address.clone(),
            port: self.port,
            profile_id,
            tags: self
                .tags
                .lines()
                .filter(|tag| !tag.is_empty())
                .map(str::to_owned)
                .collect(),
            labels,
            platform_hint: self.platform,
        })
    }
    fn load_group(&mut self, group: &HostGroupRecord, revision: HostRevision) {
        self.group_id = Some(group.id);
        self.group_name.clone_from(&group.name);
        self.parent = group.parent_id;
        self.group_base = Some(revision);
        self.delete_group = false;
        match &group.membership {
            HostGroupMembership::Static { hosts } => {
                self.dynamic = false;
                self.members.clone_from(hosts);
                self.group_selector.clear();
            }
            HostGroupMembership::Dynamic { selector } => {
                self.dynamic = true;
                self.group_selector.clone_from(selector);
                self.members.clear();
            }
        }
    }
    pub(crate) fn draw(
        &mut self,
        context: &egui::Context,
        connection: &HostGroupConnection,
        view: &HostGroupView,
    ) {
        if !self.open {
            return;
        }
        let mut open = true;
        let mut reload = false;
        let mut preview = false;
        let mut freeze = false;
        let mut change = None;
        let mut revision = None;
        let mut load_host = None;
        let mut load_group = None;
        let empty = HostCatalog::default();
        let catalog = view
            .snapshot
            .as_ref()
            .map_or(&empty, |snapshot| &snapshot.catalog);
        let current = view
            .snapshot
            .as_ref()
            .map(|snapshot| (snapshot.revision, snapshot.profile_revision));
        if self.host_base.is_none() {
            self.host_base = current;
        }
        if self.group_base.is_none() {
            self.group_base = current;
        }
        cshell_ui::form::dialog(&context, "主机与主机组").open(&mut open).default_width(760.0).vscroll(true).collapsible(false).show(context,|ui| {
            ui.label("主机引用已保存 SSH Profile 的认证/跳板配置；目标地址、标签独立保存。平台仅为提示。");ui.label(&view.status);
            reload=ui.add_enabled(!view.busy,egui::Button::new("重新加载主机与 Profile 版本")).clicked();if !self.status.is_empty() {ui.colored_label(cshell_ui::theme::WARNING,&self.status);}
            ui.collapsing("主机列表与编辑",|ui| {
                edit(ui,&mut self.filter,"名称/地址/标签筛选",128);ui.horizontal_wrapped(|ui| {ui.label(format!("{} 台主机，已选 {} 台",catalog.hosts.len(),self.selected.len()));
                    if ui.button("新主机").clicked() {self.host_id=None;self.name.clear();self.address.clear();self.port=22;self.profile=None;self.tags.clear();self.labels.clear();self.platform=HostPlatformHint::Unknown;self.host_base=current;self.delete_host=false;}
                    if ui.button("清空所选").clicked() {self.selected.clear();}});
                let hosts:Vec<_>=catalog.hosts.iter().filter(|host|self.filter.is_empty() || host.name.contains(&self.filter) || host.address.contains(&self.filter) || host.tags.iter().any(|tag|tag.contains(&self.filter))).collect();
                egui::ScrollArea::vertical().id_salt("inventory-hosts").max_height(180.0).show_rows(ui,row_height(ui),hosts.len(),|ui,range| {
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);for host in &hosts[range] {ui.push_id(host.id.to_string(),|ui| {ui.horizontal_wrapped(|ui| {
                    let mut selected=self.selected.contains(&host.id);let label=format!("{} · {}:{}",host.name,host.address,host.port);let width=(ui.available_width()-68.0).max(40.0);if ui.add_sized([width,row_height(ui)],egui::Checkbox::new(&mut selected,&label)).on_hover_text(&label).changed() {if selected {self.selected.insert(host.id);} else {self.selected.remove(&host.id);}}
                    if ui.button("编辑").clicked() {load_host=Some(*host);}
                });});}});
                edit(ui,&mut self.name,"主机名称",128);edit(ui,&mut self.address,"目标地址",255);ui.horizontal_wrapped(|ui| {ui.label("端口");ui.add(egui::DragValue::new(&mut self.port).range(1..=u16::MAX));});
                egui::ComboBox::from_id_salt("host-profile").truncate().selected_text(self.profile.and_then(|id|view.snapshot.as_ref()?.profiles.iter().find(|profile|profile.profile_id==id)).map_or("选择 SSH Profile",|profile|profile.name.as_str())).show_ui(ui,|ui| { cshell_ui::form::menu(ui);if let Some(snapshot)=&view.snapshot {for profile in &snapshot.profiles {ui.selectable_value(&mut self.profile,Some(profile.profile_id),format!("{} · {}:{} · {}",profile.name,profile.address,profile.port,profile.profile_id));}}});
                if ui.button("采用所选 Profile 的默认名称、地址和标签").clicked() && let Some(profile)=self.profile.and_then(|id|view.snapshot.as_ref()?.profiles.iter().find(|profile|profile.profile_id==id)) {self.name.clone_from(&profile.name);self.address.clone_from(&profile.address);self.port=profile.port;self.tags=profile.tags.iter().cloned().collect::<Vec<_>>().join("\n");}
                ui.label("标签（每行一项，精确匹配）");ui.add(egui::TextEdit::multiline(&mut self.tags).desired_rows(2).char_limit(4160));ui.label("属性（每行 key=value；例如 env=prod）");ui.add(egui::TextEdit::multiline(&mut self.labels).desired_rows(2).char_limit(21_000));
                egui::ComboBox::from_id_salt("host-platform").truncate().selected_text(format!("平台提示：{:?}",self.platform)).show_ui(ui,|ui| { cshell_ui::form::menu(ui);for platform in [HostPlatformHint::Unknown,HostPlatformHint::Posix,HostPlatformHint::Linux,HostPlatformHint::Macos,HostPlatformHint::Windows] {ui.selectable_value(&mut self.platform,platform,format!("{platform:?}"));}});
                let writable=view.writable && !view.busy && current.is_some() && self.host_base==current;if self.host_base!=current {ui.colored_label(cshell_ui::theme::WARNING,"编辑依据已过期，请重新选择编辑项或新建主机");}
                if ui.add_enabled(writable,egui::Button::new("保存主机")).clicked() {match self.record() {Ok(host)=> {change=Some(HostCatalogChange::UpsertHost(host));revision=self.host_base;},Err(error)=>self.status=error.into()}}
                if let Some(id)=self.host_id {ui.checkbox(&mut self.delete_host,"确认删除这台主机");if ui.add_enabled(writable && self.delete_host,egui::Button::new("删除主机")).clicked() {change=Some(HostCatalogChange::RemoveHost(id));revision=self.host_base;}}
            });
            ui.collapsing("主机组管理",|ui| {
                if ui.button("新主机组").clicked() {self.group_id=None;self.group_name.clear();self.parent=None;self.dynamic=false;self.members.clear();self.group_selector="all".into();self.group_base=current;self.delete_group=false;}
                egui::ScrollArea::vertical().id_salt("inventory-groups").max_height(120.0).show(ui,|ui| {for group in &catalog.groups {if ui.selectable_label(self.group_id==Some(group.id),format!("{} · {}",group.name,group.id)).clicked() {load_group=Some(group);}}});
                edit(ui,&mut self.group_name,"组名称",128);group_picker(ui,"host-group-parent",&mut self.parent,catalog,"顶层");ui.label("父组用于分类，成员由本组清单或查询确定。");ui.checkbox(&mut self.dynamic,"动态组（selector）");
                if self.dynamic {edit(ui,&mut self.group_selector,"tag:web AND env:prod",MAX_HOST_SELECTOR_BYTES);if let Err(error)=HostSelector::parse(&self.group_selector) {ui.colored_label(cshell_ui::theme::WARNING,error.to_string());}}
                else {ui.label(format!("静态成员：{} 台",self.members.len()));egui::ScrollArea::vertical().id_salt("static-members").max_height(160.0).show_rows(ui,row_height(ui),catalog.hosts.len(),|ui,range| {
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);for host in &catalog.hosts[range] {let mut member=self.members.contains(&host.id);if ui.checkbox(&mut member,&host.name).changed() {if member {self.members.insert(host.id);} else {self.members.remove(&host.id);}}}});}
                let writable=view.writable && !view.busy && current.is_some() && self.group_base==current;if self.group_base!=current {ui.colored_label(cshell_ui::theme::WARNING,"组编辑依据已过期，请重新选择编辑项或新建组");}
                if ui.add_enabled(writable,egui::Button::new("保存主机组")).clicked() {change=Some(HostCatalogChange::UpsertGroup(HostGroupRecord {id:self.group_id.unwrap_or_default(),name:self.group_name.clone(),parent_id:self.parent,membership:if self.dynamic {HostGroupMembership::Dynamic {selector:self.group_selector.clone()}} else {HostGroupMembership::Static {hosts:self.members.clone()}}}));revision=self.group_base;}
                if let Some(id)=self.group_id {ui.checkbox(&mut self.delete_group,"确认删除此组");if ui.add_enabled(writable && self.delete_group,egui::Button::new("删除主机组")).clicked() {change=Some(HostCatalogChange::RemoveGroup(id));revision=self.group_base;}}
            });
            ui.separator();ui.heading("预览与固定目标");ui.label("AND/OR/NOT、&&/||/!、括号；字段 tag/env/label/platform/name/address/host/profile/group。值精确匹配，含空白时使用双引号。");
            ui.horizontal_wrapped(|ui| {ui.radio_value(&mut self.selection_mode,0,"查询");ui.radio_value(&mut self.selection_mode,1,"主机组");ui.radio_value(&mut self.selection_mode,2,"所选主机");});
            match self.selection_mode {0=>edit(ui,&mut self.query,"all 或 tag:web AND env:prod",MAX_HOST_SELECTOR_BYTES),1=>group_picker(ui,"host-query-group",&mut self.query_group,catalog,"选择组"),_=>{ui.label(format!("已选 {} 台",self.selected.len()));}}
            let selection=self.selection();preview=ui.add_enabled(view.writable && !view.busy && selection.is_some(),egui::Button::new("预览目标清单…")).clicked();
            if let Some(result)=&view.preview && Some(&result.selection)==selection.as_ref() {ui.label(format!("预览：{} 台，主机版本 {} / Profile 版本 {}",result.hosts.len(),result.host_revision,result.profile_revision));
                egui::ScrollArea::vertical().id_salt("host-preview").max_height(180.0).show_rows(ui,row_height(ui),result.hosts.len(),|ui,range| {
 ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);for host in &result.hosts[range] {ui.label(format!("{} · {}:{} · {}",host.name,host.address,host.port,host.id));}});
                freeze=ui.add_enabled(view.writable && !view.busy && !result.hosts.is_empty() && current==Some((result.host_revision,result.profile_revision)),egui::Button::new("确认固定这些目标")).clicked();}
            if let Some(targets)=&view.frozen {ui.label(format!("快照 {}，{} 台主机；用于后续任务定义",targets.id(),targets.targets().len()));}
        });
        self.open = open;
        if let Some(host) = load_host
            && let Some(base) = current
        {
            self.load_host(host, base);
        }
        if let Some(group) = load_group
            && let Some(base) = current
        {
            self.load_group(group, base);
        }
        if reload {
            let _queued = connection.reload();
        }
        if let Some(change) = change
            && let Some(base) = revision
        {
            let ids = match &change {
                HostCatalogChange::UpsertHost(host) => Some((Some(host.id), None)),
                HostCatalogChange::UpsertGroup(group) => Some((None, Some(group.id))),
                _ => None,
            };
            match apply_host_change(catalog, change.clone()) {
                Err(error) => self.status = error.to_string(),
                Ok(_) if connection.change(base, change) => {
                    if let Some((host, group)) = ids {
                        if let Some(id) = host {
                            self.host_id = Some(id);
                        }
                        if let Some(id) = group {
                            self.group_id = Some(id);
                        }
                    }
                    self.delete_host = false;
                    self.delete_group = false;
                    self.status = "保存已入队；成功后重新选择编辑项继续修改".into();
                }
                Ok(_) => self.status = "存储忙或编辑版本过期，请重新加载并选择编辑项".into(),
            }
        }
        if (preview || freeze)
            && let Some(selection) = self.selection()
        {
            let _queued = connection.query(selection, freeze);
        }
    }
}
fn edit(ui: &mut egui::Ui, value: &mut String, hint: &str, limit: usize) {
    ui.label(hint);
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(ui.available_width())
            .char_limit(limit),
    );
}
fn row_height(ui: &egui::Ui) -> f32 {
    (ui.text_style_height(&egui::TextStyle::Body) + 8.0).max(ui.spacing().interact_size.y)
}
fn group_picker(
    ui: &mut egui::Ui,
    id: &str,
    current: &mut Option<HostGroupId>,
    catalog: &HostCatalog,
    empty: &str,
) {
    egui::ComboBox::from_id_salt(id)
        .truncate()
        .selected_text(
            current
                .and_then(|id| catalog.groups.iter().find(|group| group.id == id))
                .map_or(empty, |group| group.name.as_str()),
        )
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(current, None, empty);
            for group in &catalog.groups {
                ui.selectable_value(
                    current,
                    Some(group.id),
                    format!("{} · {}", group.name, group.id),
                );
            }
        });
}

#[cfg(test)]
mod tests;
