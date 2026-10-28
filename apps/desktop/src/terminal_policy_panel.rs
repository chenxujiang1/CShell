use crate::{
    daemon_connection::DesktopConnectionConfig, profile_connection::DesktopProfileView,
    terminal_policy_connection::Connection,
};
use cshell_application::*;
use cshell_domain::*;
use cshell_ipc::{TerminalPolicyOperation as Op, TerminalPolicyRequest as Request};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
struct AppliedTheme {
    session: SessionId,
    report: Arc<TerminalResolutionReport>,
}
#[derive(Default)]
pub(super) struct TerminalPolicyPanel {
    pub open: bool,
    connection: Option<Connection>,
    last_poll: Option<Instant>,
    generation: Option<u64>,
    loaded_revision: Option<u64>,
    draft: String,
    dirty: bool,
    epoch: u64,
    context_epoch: u64,
    profile: Option<ProfileId>,
    host: String,
    temporary: TerminalOverrides,
    confirm: bool,
    seen_token: Option<[u8; 32]>,
    seen_resolution: Option<Arc<TerminalResolutionReport>>,
    launch_confirm: bool,
    launch: Option<Arc<TerminalResolutionReport>>,
    error: Option<String>,
    themes: Arc<Mutex<BTreeMap<TabId, AppliedTheme>>>,
}
impl TerminalPolicyPanel {
    pub fn take_launch(&mut self) -> Option<Arc<TerminalResolutionReport>> {
        self.launch.take()
    }
    pub fn bind_launch(
        &self,
        tab: TabId,
        session: SessionId,
        report: Arc<TerminalResolutionReport>,
    ) -> bool {
        let mut themes = self
            .themes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if themes.len() >= 256 && !themes.contains_key(&tab) {
            return false;
        }
        themes.insert(tab, AppliedTheme { session, report });
        true
    }
    pub fn fork(&self) -> Self {
        Self {
            themes: self.themes.clone(),
            ..Default::default()
        }
    }
    pub fn catalog(&self) -> Option<Arc<TerminalPolicyView>> {
        self.connection.as_ref()?.view().catalog
    }
    pub fn poll(&mut self, config: Option<&DesktopConnectionConfig>) -> bool {
        if self.connection.is_none()
            && let Some(config) = config
            && self.error.is_none()
        {
            match Connection::start(config.clone()) {
                Ok(c) => {
                    c.request(
                        Request {
                            operation: Op::Get as i32,
                            ..Default::default()
                        },
                        0,
                        true,
                        None,
                    );
                    self.connection = Some(c);
                }
                Err(_) => self.error = Some("继承策略线程无法启动".into()),
            }
        }
        let Some(c) = &self.connection else {
            return false;
        };
        let view = c.view();
        let changed = self.generation != Some(view.generation);
        self.generation = Some(view.generation);
        if !view.busy
            && self
                .last_poll
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
        {
            c.request(
                Request {
                    operation: Op::Get as i32,
                    ..Default::default()
                },
                0,
                false,
                None,
            );
            self.last_poll = Some(Instant::now());
        }
        changed
    }
    pub fn theme_for(
        &self,
        tab: TabId,
        session: SessionId,
        profile: ProfileId,
        profile_revision: u64,
    ) -> Option<String> {
        let catalog = self.catalog()?;
        let mut themes = self
            .themes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bound = themes.get(&tab)?;
        if bound.session != session
            || bound.report.context.profile_id != profile
            || bound.report.profile_revision != profile_revision
            || bound.report.profile_revision != catalog.profile_revision
            || bound.report.host_revision != catalog.host_revision
            || bound.report.policy_revision != catalog.catalog.revision
        {
            themes.remove(&tab);
            return None;
        }
        Some(bound.report.settings.theme.value.clone())
    }
    pub fn draw(
        &mut self,
        context: &egui::Context,
        profiles: Option<&DesktopProfileView>,
        active: Option<ProfileId>,
        tab: Option<TabId>,
        session: Option<SessionId>,
    ) {
        if !self.open {
            return;
        }
        context.request_repaint_after(Duration::from_millis(150));
        let view = self
            .connection
            .as_ref()
            .map(Connection::view)
            .unwrap_or_default();
        if self.seen_resolution != view.report {
            self.seen_resolution = view.report.clone();
            self.launch_confirm = false;
        }
        if self.seen_token != view.preview.as_ref().map(|p| p.token) {
            self.seen_token = view.preview.as_ref().map(|p| p.token);
            self.confirm = false;
        }
        let Some(policy) = view.catalog.as_ref() else {
            let mut open = self.open;
            cshell_ui::form::dialog(&context, "标签 / Host / 临时终端继承")
                .open(&mut open)
                .show(context, |ui| {
                    if let Some(error) = self.error.as_ref().or(view.error.as_ref()) {
                        ui.colored_label(cshell_ui::theme::WARNING, error);
                    } else {
                        ui.label("等待继承策略目录");
                    }
                    if view.busy {
                        ui.spinner();
                    }
                    if ui
                        .add_enabled(!view.busy, egui::Button::new("重新读取"))
                        .clicked()
                    {
                        self.error = None;
                        if let Some(c) = &self.connection {
                            c.request(
                                Request {
                                    operation: Op::Get as i32,
                                    ..Default::default()
                                },
                                0,
                                true,
                                None,
                            );
                        }
                    }
                });
            self.open = open;
            return;
        };
        if self.loaded_revision != Some(policy.catalog.revision) && !self.dirty {
            self.draft = serde_json::to_string_pretty(&policy.catalog).unwrap_or_default();
            self.loaded_revision = Some(policy.catalog.revision);
            self.confirm = false;
        }
        let profiles = profiles.and_then(|p| p.catalog.as_ref());
        let ready = !view.busy
            && view.writable
            && profiles.is_some_and(|p| p.revision == policy.profile_revision);
        let mut open = self.open;
        cshell_ui::form::dialog(&context, "标签 / Host / 临时终端继承").open(&mut open).default_size([850.0,750.0]).show(context,|ui|{
            ui.weak("全局 → 文件夹 → 标签（priority、UUID 升序）→ Profile → Host → 临时。当前标签按钮只改显示主题；另行确认打开新会话会使用解析的 TERM / Logging。Host 上下文不改变连接地址。");
            if let Some(error)=self.error.as_ref().or(view.error.as_ref()){ui.colored_label(cshell_ui::theme::WARNING,error);}
            if view.busy{ui.spinner();}
            ui.horizontal_wrapped(|ui|{
                if ui.add_enabled(!view.busy,egui::Button::new("明确重新读取 / 恢复写权限")).clicked() && let Some(c)=&self.connection {c.request(Request{operation:Op::Get as i32,..Default::default()},0,true,None);self.confirm=false;self.error=None;}
                if ui.button("丢弃草稿并载入已保存策略").clicked(){self.dirty=false;self.loaded_revision=None;self.epoch=self.epoch.wrapping_add(1);self.confirm=false;}
            });
            ui.label(format!("策略 {} · Profile {} · Host {}",policy.catalog.revision,policy.profile_revision,policy.host_revision));
            ui.collapsing("编辑策略 JSON（null 表示继承；最多 64 标签 / 256 Host）",|ui|{
                ui.horizontal_wrapped(|ui|{
                    if ui.button("添加标签规则示例").clicked() && let Ok(mut catalog)=decode_terminal_policies(self.draft.as_bytes()) && catalog.tags.len()<64 {
                        catalog.tags.push(TagTerminalPolicy{id:TerminalPolicyId::new(),tag:"example".into(),priority:100,terminal:TerminalOverrides::default()});
                        self.draft=serde_json::to_string_pretty(&catalog).unwrap_or_default();self.dirty=true;self.epoch=self.epoch.wrapping_add(1);self.confirm=false;
                    }
                });
                if ui.add(egui::TextEdit::multiline(&mut self.draft).code_editor().desired_rows(12).char_limit(MAX_TERMINAL_POLICY_BYTES)).changed(){self.dirty=true;self.epoch=self.epoch.wrapping_add(1);self.confirm=false;}
            });
            let proposal=decode_terminal_policies(self.draft.as_bytes()).ok().map(|catalog|TerminalPolicyProposal{catalog,profile_revision:policy.profile_revision,host_revision:policy.host_revision});
            if ui.add_enabled(ready && proposal.is_some(),egui::Button::new("预览策略及潜在影响范围")).clicked() && let Some(proposal)=&proposal && let Some(c)=&self.connection {
                match proposal.fingerprint().and_then(|_|serde_json::to_vec(proposal).map_err(|_|"策略编码失败")) {Ok(document_json)=>{c.request(Request{operation:Op::Preview as i32,expected_revision:policy.catalog.revision,profile_revision:policy.profile_revision,host_revision:policy.host_revision,document_json,..Default::default()},self.epoch,false,None);self.error=None;},Err(error)=>self.error=Some(error.into())}
            }
            if let Some(preview)=&view.preview {
                let current=view.preview_epoch==self.epoch && proposal.as_ref()==Some(&preview.proposal) && preview.proposal.catalog.revision==policy.catalog.revision;
                if current {
                    ui.label(format!("潜在关联 {} 个 Profile（可能被更高层覆盖，并非全部最终值变化）",preview.matched_profiles.len()));
                    egui::ScrollArea::vertical().max_height(90.0).show(ui,|ui|{for id in &preview.matched_profiles{ui.label(profiles.and_then(|p|p.profiles.iter().find(|p|p.id==*id)).map_or_else(||id.to_string(),|p|format!("{} · {id}",p.name)));}});
                    ui.checkbox(&mut self.confirm,"确认保存这份完整策略目录");
                    let now=SystemTime::now().duration_since(UNIX_EPOCH).map_or(u64::MAX,|t|t.as_secs());
                    if ui.add_enabled(ready && self.confirm && now<preview.expires_unix,egui::Button::new("保存策略")).clicked() && let Some(c)=&self.connection && let Ok(sha)=preview.fingerprint() {
                        if c.request(Request{operation:Op::Apply as i32,expected_revision:policy.catalog.revision,profile_revision:policy.profile_revision,host_revision:policy.host_revision,preview_token:preview.token.to_vec(),review_sha256:sha.to_vec(),confirmed:true,..Default::default()},self.epoch,false,None) {self.confirm=false;}
                    }
                } else {ui.weak("草稿或目录变化，请重新预览。");}
            }
            // Adopt a saved acknowledgement only when it describes the unchanged submitted draft.
            if let (Some(saved),Some(proposal))=(&view.saved,&proposal) {
                let mut submitted=proposal.catalog.clone();submitted.revision=saved.revision;
                if submitted==**saved && policy.catalog==**saved && saved.revision!=proposal.catalog.revision {self.dirty=false;self.loaded_revision=None;}
            }
            ui.separator();ui.heading("解析最终值与字段来源");
            let old=(self.profile,self.host.clone(),self.temporary.clone());
            if ui.add_enabled(active.is_some(),egui::Button::new("选中当前会话 Profile")).clicked(){self.profile=active;}
            if let Some(profiles)=profiles {cshell_ui::form::combo(ui, "Profile").truncate().selected_text(self.profile.and_then(|id|profiles.profiles.iter().find(|p|p.id==id)).map_or("请选择",|p|p.name.as_str())).show_ui(ui,|ui|{ cshell_ui::form::menu(ui);for profile in &profiles.profiles{ui.selectable_value(&mut self.profile,Some(profile.id),&profile.name);}});}
            ui.horizontal_wrapped(|ui|{ui.label("Host UUID（空为无 Host 上下文）");ui.add(egui::TextEdit::singleline(&mut self.host).char_limit(36));});
            override_text(ui,"临时 TERM",&mut self.temporary.terminal_type);override_text(ui,"临时 Theme",&mut self.temporary.theme);
            ui.horizontal_wrapped(|ui|{ui.label("临时 Logging");ui.selectable_value(&mut self.temporary.logging,None,"继承");ui.selectable_value(&mut self.temporary.logging,Some(true),"启用");ui.selectable_value(&mut self.temporary.logging,Some(false),"关闭");});
            if old!=(self.profile,self.host.clone(),self.temporary.clone()){self.context_epoch=self.context_epoch.wrapping_add(1);self.launch_confirm=false;}
            let host=if self.host.trim().is_empty(){Ok(None)}else{serde_json::from_value::<HostId>(serde_json::Value::String(self.host.trim().into())).map(Some).map_err(|_|"Host UUID 无效")};
            let resolution=self.profile.zip(host.ok()).map(|(profile_id,host_id)|TerminalResolutionContext{profile_id,host_id,temporary:self.temporary.clone()});
            if ui.add_enabled(!view.busy && resolution.is_some() && profiles.is_some_and(|p|p.revision==policy.profile_revision),egui::Button::new("检查继承值与来源")).clicked() && let Some(resolution)=&resolution && let Some(c)=&self.connection && let Some(profiles)=profiles && let Ok(document_json)=serde_json::to_vec(resolution) {
                c.request(Request{operation:Op::Resolve as i32,expected_revision:policy.catalog.revision,profile_revision:policy.profile_revision,host_revision:policy.host_revision,document_json,..Default::default()},self.context_epoch,false,Some(Arc::new(profiles.export_snapshot())));
            }
            if let Some(report)=&view.report && view.report_epoch==self.context_epoch && resolution.as_ref()==Some(&report.context) && report.policy_revision==policy.catalog.revision && report.profile_revision==policy.profile_revision && report.host_revision==policy.host_revision {
                ui.label(format!("TERM={} [{}]",report.settings.terminal_type.value,source(report.settings.terminal_type.source)));
                ui.label(format!("Theme={} [{}]",report.settings.theme.value,source(report.settings.theme.source)));
                ui.label(format!("Logging={} [{}]",report.settings.logging.value,source(report.settings.logging.source)));
                ui.weak("新会话按已保存连接配置启动；Logging=false 不落盘，日志分页、原始回放及依赖日志的触发器不可用。已运行会话不变。");
                ui.checkbox(&mut self.launch_confirm,"确认按上述上下文打开一个新会话");
                if ui.add_enabled(self.launch_confirm && !view.busy && profiles.is_some_and(|p|p.revision==report.profile_revision),egui::Button::new("按核对结果打开新会话")).clicked(){self.launch=Some(report.clone());self.launch_confirm=false;}
                if ui.add_enabled(active==Some(report.context.profile_id) && tab.is_some() && session.is_some() && profiles.is_some_and(|p|p.revision==report.profile_revision),egui::Button::new("对当前标签应用此显示主题")).clicked() && let (Some(tab),Some(session))=(tab,session) {
                    let mut themes=self.themes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if themes.len()<256 || themes.contains_key(&tab){themes.insert(tab,AppliedTheme{session,report:report.clone()});}
                    else{self.error=Some("临时主题最多绑定 256 个标签，请先清除旧绑定".into());}
                }
            }
            if ui.add_enabled(tab.is_some(),egui::Button::new("清除当前标签临时主题上下文")).clicked() && let Some(tab)=tab {self.themes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&tab);}
        });
        self.open = open;
    }
}
fn override_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        let mut enabled = value.is_some();
        if ui.checkbox(&mut enabled, "覆盖").changed() {
            *value = enabled.then(String::new);
        }
        if let Some(text) = value {
            ui.add(egui::TextEdit::singleline(text).char_limit(128));
        }
    });
}
fn source(value: SettingSource) -> String {
    match value {
        SettingSource::Global => "全局".into(),
        SettingSource::Folder(id) => format!("文件夹 {id}"),
        SettingSource::Tag(id) => format!("标签策略 {id}"),
        SettingSource::Profile(id) => format!("Profile {id}"),
        SettingSource::Host(id) => format!("Host {id}"),
        SettingSource::Temporary => "临时".into(),
    }
}
