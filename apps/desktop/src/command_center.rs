//! Application actions share the existing workbench dispatcher; never send shell text.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    ShellHistory,
    NewSsh,
    Sessions,
    Search,
    Sftp,
    Reconnect,
    Disconnect,
    CloneChannel,
    QuickCommands,
    Tunnels,
    Settings,
    Split,
}
const ACTIONS: [(Action, &str, &str); 12] = [
    (Action::NewSsh, "新建 SSH 配置", "连接 主机 new ssh"),
    (Action::Sessions, "会话管理", "服务器 profile sessions"),
    (Action::Search, "查找终端内容", "搜索 find search"),
    (Action::ShellHistory,"命令历史与输出","命令 上一条 下一条 command history output shell integration"),
    (Action::Sftp, "打开 SFTP", "上传 下载 文件 file transfer"),
    (Action::Reconnect, "重新连接当前 SSH", "重连 reconnect"),
    (Action::Disconnect, "断开当前 SSH", "disconnect"),
    (
        Action::CloneChannel,
        "复制 SSH 渠道",
        "复用认证 clone channel",
    ),
    (Action::QuickCommands, "快捷命令", "常用命令 quick commands"),
    (Action::Tunnels, "SSH 隧道管理", "端口转发 tunnel forward"),
    (Action::Settings, "全局设置", "字号 鼠标 凭据 settings"),
    (Action::Split, "左右分屏", "窗口 split"),
];
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bindings {
    keys: BTreeMap<Action, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profiles: BTreeMap<cshell_domain::ProfileId, BTreeMap<Action, String>>,
}
impl Default for Bindings {
    fn default() -> Self {
        Self {
            profiles: BTreeMap::new(),
            keys: [
                (Action::NewSsh, "Ctrl+Shift+N"),
                (Action::Sessions, "Ctrl+Shift+O"),
                (Action::Search, "Ctrl+Shift+F"),
                (Action::ShellHistory,"Ctrl+Shift+H"),
                (Action::Sftp, "Ctrl+Shift+E"),
                (Action::Reconnect, "Ctrl+Shift+R"),
                (Action::QuickCommands, "Ctrl+Shift+G"),
                (Action::Tunnels, "Ctrl+Shift+T"),
            ]
            .into_iter()
            .map(|(a, s)| (a, s.into()))
            .collect(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Shortcut {
    ctrl: bool,
    shift: bool,
    alt: bool,
    key: String,
}
fn shortcut(value: &str) -> Option<Shortcut> {
    let mut result = Shortcut {
        ctrl: false,
        shift: false,
        alt: false,
        key: String::new(),
    };
    for part in value.trim().split('+') {
        match part {
            "Ctrl" if !result.ctrl => result.ctrl = true,
            "Shift" if !result.shift => result.shift = true,
            "Alt" if !result.alt => result.alt = true,
            key if result.key.is_empty() => result.key = key.to_ascii_uppercase(),
            _ => return None,
        }
    }
    let function = result
        .key
        .strip_prefix('F')
        .and_then(|v| v.parse::<u8>().ok())
        .is_some_and(|n| (2..=24).contains(&n)&&result.key==format!("F{n}"));
    let letter = result.key.len() == 1
        && result.key.as_bytes()[0].is_ascii_alphabetic()
        && result.ctrl
        && result.shift
        && !result.alt;
    (function || letter).then_some(result)
}
fn active_profile(model: &cshell_ui::WorkbenchViewModel) -> Option<cshell_domain::ProfileId> {
    model
        .workspace_document
        .as_ref()?
        .bindings
        .iter()
        .find(|b| Some(b.tab_id) == model.selected_tab)?
        .profile_id
}
impl Bindings {
    fn effective(&self, profile: Option<cshell_domain::ProfileId>) -> &BTreeMap<Action, String> {
        profile
            .and_then(|p| self.profiles.get(&p))
            .unwrap_or(&self.keys)
    }
    fn validate(&self) -> Result<(), String> {
        for map in std::iter::once(&self.keys).chain(self.profiles.values()) {
            let mut used = BTreeSet::new();
            for (action, value) in map {
                if value.trim().is_empty() {
                    continue;
                }
                let key = shortcut(value)
                    .ok_or("使用 Ctrl+Shift+字母或 F2–F24，可配合 Ctrl / Shift / Alt；留空禁用")?;
                if key.ctrl
                    && key.shift
                    && !key.alt
                    && (["C", "V", "P", "K"].contains(&key.key.as_str())
                        || (key.key == "F" && *action != Action::Search))
                    || key.alt && !key.ctrl && key.key == "F4"
                    || key.shift && !key.ctrl && !key.alt && key.key == "F10"
                {
                    return Err(format!(
                        "{value} 已被系统、复制粘贴、标签列表或快捷操作保留"
                    ));
                }
                if !used.insert(key) {
                    return Err(format!("{value} 重复分配，请使用不同按键"));
                }
            }
        }
        Ok(())
    }
    fn action(&self, key: &str) -> Option<Action> {
        let mut chars = key.chars();
        let c = chars.next()?.to_ascii_uppercase();
        if chars.next().is_some() {
            return None;
        }
        self.keys.iter().find_map(|(a, s)| {
            (shortcut(s)
                == Some(Shortcut {
                    ctrl: true,
                    shift: true,
                    alt: false,
                    key: c.to_string(),
                }))
            .then_some(*a)
        })
    }
}
static BINDINGS: OnceLock<Mutex<Bindings>> = OnceLock::new();
fn store() -> &'static Mutex<Bindings> {
    BINDINGS.get_or_init(|| Mutex::new(Bindings::default()))
}
fn current() -> Bindings {
    store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
pub(super) fn uses_default_search(model: &cshell_ui::WorkbenchViewModel) -> bool {
    current()
        .effective(active_profile(model))
        .get(&Action::Search)
        .is_some_and(|v| v == "Ctrl+Shift+F")
}
fn settings_path() -> Option<std::path::PathBuf> {
    crate::display_settings::settings_path().map(|p| p.with_file_name("workbench-shortcuts.json"))
}
fn save_bindings(path: &std::path::Path, bindings: &Bindings) -> std::io::Result<()> {
    use std::io::Write;
    bindings.validate().map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("pending-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(bindings)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
fn matching(query: &str) -> Vec<&'static (Action, &'static str, &'static str)> {
    let query = query.to_lowercase();
    ACTIONS
        .iter()
        .filter(|(_, label, tags)| {
            query
                .split_whitespace()
                .all(|q| format!("{label} {tags}").to_lowercase().contains(q))
        })
        .collect()
}
struct Item {
    label: String,
    hint: String,
    action: Option<Action>,
    command: Option<WorkbenchMenuCommand>,
}
impl Item {
    fn resolve(
        &self,
        model: &cshell_ui::WorkbenchViewModel,
    ) -> Result<WorkbenchMenuCommand, &'static str> {
        if model.layout_paused {
            return Err("请等待当前操作完成");
        }
        self.action
            .map(|a| command(a, model))
            .unwrap_or_else(|| self.command.ok_or("此操作已不可用"))
    }
}
fn palette_items(
    query: &str,
    model: &cshell_ui::WorkbenchViewModel,
    quick: &[(cshell_domain::QuickCommandId, String)],
) -> Vec<Item> {
    let keys = current();
    let mut items = matching(query)
        .into_iter()
        .map(|(action, label, _)| Item {
            label: (*label).into(),
            hint: keys
                .effective(active_profile(model))
                .get(action)
                .cloned()
                .unwrap_or_default(),
            action: Some(*action),
            command: None,
        })
        .collect::<Vec<_>>();
    if query.trim().is_empty() {
        return items;
    }
    let query = query.to_lowercase();
    let matches = |s: &str| {
        query
            .split_whitespace()
            .all(|q| s.to_lowercase().contains(q))
    };
    for (tab,title,target) in cshell_ui::searchable_window_tabs(model) {
        if matches(&format!("标签 tab {title} {target}")) {
            items.push(Item {
                label: title,
                hint: "切换标签".into(),
                action: None,
                command: Some(WorkbenchMenuCommand::OpenWorkspaceTab(tab)),
            });
        }
    }
    for profile in &model.saved_ssh_profiles {
        if items.len()>=200 {break;}
        if matches(&format!("主机 host {} {}", profile.name, profile.target)) {
            items.push(Item {
                label: format!("{} · {}", profile.name, profile.target),
                hint: "连接主机".into(),
                action: None,
                command: Some(WorkbenchMenuCommand::OpenSavedProfile(profile.id)),
            });
        }
    }
    for (id, name) in quick {
        if items.len()>=200 {break;}
        if matches(&format!("快捷命令 command {name}")) {
            items.push(Item {
                label: name.clone(),
                hint: "载入命令草稿".into(),
                action: None,
                command: Some(WorkbenchMenuCommand::LoadQuickCommand(*id)),
            });
        }
    }
    items.truncate(200);
    items
}
pub(super) fn initialize() {
    let bindings = settings_path()
        .and_then(|p| {
            if std::fs::metadata(&p).ok()?.len() > 8192 {
                return None;
            }
            let value: Bindings = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
            value.validate().ok()?;
            Some(value)
        })
        .unwrap_or_default();
    *store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = bindings;
}
fn command(
    action: Action,
    model: &cshell_ui::WorkbenchViewModel,
) -> Result<WorkbenchMenuCommand, &'static str> {
    use cshell_ui::WorkbenchMenuCommand as C;
    if model.layout_paused {
        return Err("请等待当前操作完成");
    }
    Ok(match action {
        Action::ShellHistory if model.selected.is_some()=>C::ShellHistory,
        Action::NewSsh => C::NewSshProfile,
        Action::Sessions => C::SshConnections,
        Action::Settings => C::GlobalSettings,
        Action::Tunnels => C::Tunnels,
        Action::QuickCommands => C::QuickCommands,
        Action::Search if model.selected.is_some() => C::SearchTerminal,
        Action::Sftp if model.can_sftp => C::SftpFiles,
        Action::Reconnect if model.can_restart_ssh && !model.ssh_control_busy => C::ReconnectSsh,
        Action::Disconnect if model.can_disconnect_ssh && !model.ssh_control_busy => {
            C::DisconnectSsh
        }
        Action::CloneChannel
            if !model.ssh_control_busy
                && model
                    .tabs
                    .iter()
                    .find(|t| Some(t.id) == model.selected_tab)
                    .and_then(|t| t.session_id)
                    .is_some_and(|id| model.reusable_ssh_sessions.contains(&id)) =>
        {
            C::CloneSshChannel(model.selected_tab.ok_or("请先选择 SSH 标签")?)
        }
        Action::Split if model.selected_tab.is_some() => {
            C::SplitWorkspace(cshell_domain::SplitAxis::Horizontal)
        }
        _ => return Err("当前会话状态不支持此操作"),
    })
}
#[derive(Default)]
pub(super) struct State {
    profile_scope: bool,
    capture: Option<Action>,
    pub quick: Vec<(cshell_domain::QuickCommandId, String)>,
    pub open: bool,
    settings_open: bool,
    query: String,
    index: usize,
    focus: bool,
    draft: Option<Bindings>,
    error: Option<String>,
    return_focus: bool,
    composing: bool,
    previous_focus: Option<egui::Id>,
}
impl State {
    pub fn blocks_input(&self) -> bool {
        self.open || self.settings_open
    }
    pub fn suspend(&mut self) {
        if self.open {
            self.close();
        }
    }
    pub fn show(&mut self) {
        self.open = true;
        self.query.clear();
        self.index = 0;
        self.focus = true;
        self.error = None;
        self.composing = false;
    }
    pub fn show_settings(&mut self) {
        self.open = false;
        self.settings_open = true;
        self.draft = Some(current());
        self.error = None;
    }
    fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.return_focus = true;
        self.composing = false;
    }
    pub fn draw(&mut self, ctx: &egui::Context, model: &mut cshell_ui::WorkbenchViewModel) {
        if self.open {
            let mut items = palette_items(&self.query, model, &self.quick);
            self.index = self.index.min(items.len().saturating_sub(1));
            let mut selected = None;
            let mut close = false;
            let ime_commit = ctx.input(|i| {
                i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Ime(egui::ImeEvent::Commit(_))))
            });
            ctx.input(|i| {
                for event in &i.events {
                    if let egui::Event::Ime(ime) = event {
                        match ime {
                            egui::ImeEvent::Preedit { text, .. } => {
                                self.composing = !text.is_empty()
                            }
                            egui::ImeEvent::Enabled => self.composing = true,
                            egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => {
                                self.composing = false
                            }
                            _ => {}
                        }
                    }
                }
            });
            let escape = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            let down =
                ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown));
            let up = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp));
            if down && !items.is_empty() {
                self.index = (self.index + 1) % items.len();
            }
            if up && !items.is_empty() {
                self.index = (self.index + items.len() - 1) % items.len();
            }
            let enter = !self.composing
                && !ime_commit
                && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let modal =
                egui::Modal::new(egui::Id::new("workbench-command-center")).show(ctx, |ui| {
                    ui.set_width((ctx.content_rect().width() - 48.0).clamp(240.0, 600.0));
                    ui.heading("快捷操作");
                    ui.weak("搜索功能、已保存主机、当前窗口标签和快捷命令；命令只载入草稿。");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("搜索功能、主机、标签、快捷命令…")
                            .desired_width(f32::INFINITY),
                    );
                    if self.focus {
                        response.request_focus();
                        self.focus = false;
                    }
                    if response.changed() {
                        items = palette_items(&self.query, model, &self.quick);
                        self.index = 0;
                        self.error = None;
                    }
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height((ctx.content_rect().height() - 220.0).clamp(80.0, 340.0))
                        .show(ui, |ui| {
                            if items.is_empty() {
                                ui.weak("没有匹配的操作");
                            }
                            for (index, item) in items.iter().enumerate() {
                                let available = item.resolve(model);
                                let button = egui::Button::new(&item.label)
                                    .truncate()
                                    .right_text(&item.hint)
                                    .selected(index == self.index);
                                let r = ui.push_id((format!("{:?}",item.command),format!("{:?}",item.action)),|ui|ui
                                    .add_enabled_ui(available.is_ok(), |ui| {
                                        ui.add_sized([ui.available_width(), 32.0], button)
                                    })
                                    .inner).inner;
                                if (down || up) && index == self.index {
                                    r.scroll_to_me(Some(egui::Align::Center));
                                }
                                if r.clicked() {
                                    selected = Some(index);
                                }
                                if let Err(reason) = available {
                                    r.on_disabled_hover_text(reason);
                                }
                            }
                        });
                    if let Some(error) = &self.error {
                        ui.colored_label(cshell_ui::theme::WARNING, error);
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.weak("↑ ↓ 选择 · Enter 执行 · Esc 返回");
                        if ui.button("关闭").clicked() {
                            close = true;
                        }
                    });
                });
            if enter {
                selected = (!items.is_empty()).then_some(self.index);
            }
            if let Some(index) = selected {
                match items[index].resolve(model) {
                    Ok(cmd) => {
                        model.menu_command = Some(cmd);
                        self.close();
                    }
                    Err(reason) => self.error = Some(reason.into()),
                }
            }
            if close || escape || modal.should_close() {
                self.close();
            }
        }
        if self.settings_open {
            let mut open = true;
            let mut save = false;
            let mut close = false;
            cshell_ui::form::dialog(ctx,"键盘快捷键").open(&mut open).default_width(580.0).vscroll(false).show(ctx,|ui|{
                ui.weak("主工作台应用操作：Ctrl+Shift+字母或 F2–F24（可加修饰键）。编辑框、SFTP 内部按键保持独立。留空禁用。");
                ui.label("快捷操作：Ctrl+Shift+K　标签列表：Ctrl+Shift+P");
                let profile=active_profile(model);let previous=self.profile_scope;
                ui.horizontal(|ui|{ui.selectable_value(&mut self.profile_scope,false,"全局方案");ui.add_enabled_ui(profile.is_some(),|ui|ui.selectable_value(&mut self.profile_scope,true,"当前 SSH 会话方案"));});
                if previous!=self.profile_scope {self.capture=None;}
                if profile.is_none()&&self.profile_scope{self.profile_scope=false;self.capture=None;}
                let draft=self.draft.get_or_insert_with(current);let mut enabled=true;
                if self.profile_scope && let Some(profile)=profile {
                    let mut independent=draft.profiles.contains_key(&profile);
                    if ui.checkbox(&mut independent,"本会话使用独立方案；关闭后继承全局").changed(){if independent {draft.profiles.insert(profile,draft.keys.clone());}else{draft.profiles.remove(&profile);}self.capture=None;}
                    enabled=independent;
                }
                let profile_keys = profile.filter(|_| self.profile_scope)
                    .and_then(|id| draft.profiles.get_mut(&id));
                let keys = profile_keys.unwrap_or(&mut draft.keys);
                if let Some(action)=self.capture {
                    ui.colored_label(cshell_ui::theme::ACCENT,"请按要分配的快捷键，Esc 取消录入");
                    if let Some((key,mods))=ctx.input(|i|i.events.iter().find_map(|e|if let egui::Event::Key{key,modifiers,pressed:true,repeat:false,..}=e{Some((*key,*modifiers))}else{None})) {
                        ctx.input_mut(|i|{i.consume_key(mods,key);});
                        if key!=egui::Key::Escape {let text=format!("{}{}{}{:?}",if mods.ctrl||mods.mac_cmd{"Ctrl+"}else{""},if mods.shift{"Shift+"}else{""},if mods.alt{"Alt+"}else{""},key);keys.insert(action,text);}self.capture=None;
                    }
                }
                ui.separator();
                egui::ScrollArea::vertical().max_height((ctx.content_rect().height()-310.0).clamp(60.0,300.0)).show(ui,|ui|egui::Grid::new("workbench-shortcut-fields").num_columns(3).spacing([10.0,8.0]).show(ui,|ui|for (action,label,_) in ACTIONS {
                    ui.add_sized([160.0,26.0],egui::Label::new(label).halign(egui::Align::Min).truncate());
                    ui.add_enabled_ui(enabled,|ui|ui.add_sized([180.0,26.0],egui::TextEdit::singleline(keys.entry(action).or_default()).char_limit(32).desired_width(180.0)));
                    if ui.add_enabled(enabled,egui::Button::new("录入")).clicked(){self.capture=Some(action);}ui.end_row();
                }));
                let validation=draft.validate();if let Err(error)=&validation{ui.colored_label(cshell_ui::theme::WARNING,error);}
                if let Some(error)=&self.error{ui.colored_label(cshell_ui::theme::DANGER,error);}
                ui.separator();ui.horizontal(|ui|{
                    if ui.button(if self.profile_scope{"恢复继承"}else{"恢复默认"}).clicked(){if self.profile_scope {if let Some(profile)=profile{draft.profiles.remove(&profile);}}else{draft.keys=Bindings::default().keys;}self.capture=None;self.error=None;}
                    save=ui.add_enabled(validation.is_ok(),egui::Button::new("保存快捷键")).clicked();
                    close=ui.button("取消").clicked();
                });
            });
            if save && let Some(draft) = self.draft.as_ref() {
                let mut active = store()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let result = draft
                    .validate()
                    .map_err(std::io::Error::other)
                    .and_then(|_| {
                        let path = settings_path()
                            .ok_or_else(|| std::io::Error::other("无法确定设置目录"))?;
                        save_bindings(&path, draft)
                    });
                match result {
                    Ok(()) => {
                        *active = draft.clone();
                        close = true;
                    }
                    Err(error) => self.error = Some(format!("保存失败，原快捷键继续生效：{error}")),
                }
            }
            if !open || close {
                self.settings_open = false;
                self.draft = None;
                self.return_focus = true;
            }
        }
        if std::mem::take(&mut self.return_focus) {
            let previous = self.previous_focus.take();
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
                if let Some(id) = previous {
                    m.request_focus(id);
                }
            });
        }
    }
}
impl DesktopApp {
    pub(super) fn route_workbench_shortcut(&mut self, event: &winit::event::KeyEvent) -> bool {
        if event.state != ElementState::Pressed || self.modifiers.control_key()&&self.modifiers.super_key() {
            return false;
        }
        let key = match &event.logical_key {
            Key::Character(key) if key.len() == 1 => key.to_ascii_uppercase(),
            Key::Named(key) => format!("{key:?}"),
            _ => return false,
        };
        let pressed = Shortcut {
            ctrl: self.modifiers.control_key() != self.modifiers.super_key(),
            shift: self.modifiers.shift_key(),
            alt: self.modifiers.alt_key(),
            key,
        };
        if pressed.ctrl && pressed.shift && !pressed.alt && pressed.key == "K" {
            if !event.repeat {
                let focus = self.egui_context.memory(|m| m.focused());
                self.release_remote_mouse();
                self.dismiss_clipboard_menu();
                self.command_center.show();
                self.command_center.previous_focus = focus;
            }
            return true;
        }
        let bindings = current();
        let Some(action) = bindings
            .effective(active_profile(&self.view_model))
            .iter()
            .find_map(|(action, value)| {
                (shortcut(value) == Some(pressed.clone())).then_some(*action)
            })
        else {
            return false;
        };
        if !event.repeat {
            match command(action, &self.view_model) {
                Ok(command) => self.view_model.menu_command = Some(command),
                Err(reason) => self.view_model.input_warning = Some(reason.into()),
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bindings_protect_terminal_keys_and_reject_duplicates() {
        let mut b = Bindings::default();
        assert!(b.validate().is_ok());
        for key in [
            "Ctrl+C",
            "Alt+R",
            "Ctrl+Shift+C",
            "Ctrl+Shift+K",
            "Ctrl+Shift+P",
            "Ctrl+Shift+F",
        ] {
            b.keys.insert(Action::Disconnect, key.into());
            assert!(b.validate().is_err(), "{key}");
        }
        b.keys.insert(Action::Disconnect, "Ctrl+Shift+R".into());
        assert!(b.validate().is_err());
        b.keys.insert(Action::Disconnect, String::new());
        assert!(b.validate().is_ok());
        assert_eq!(b.action("r"), Some(Action::Reconnect));
        assert_eq!(b.action("rr"), None);
        let encoded = serde_json::to_vec(&b).unwrap();
        assert!(
            serde_json::from_slice::<Bindings>(&encoded)
                .unwrap()
                .validate()
                .is_ok()
        );
    }
    #[test]
    fn unavailable_actions_never_produce_commands() {
        let mut model = cshell_ui::WorkbenchViewModel::default();
        assert!(command(Action::Sftp, &model).is_err());
        model.can_sftp = true;
        assert_eq!(
            command(Action::Sftp, &model),
            Ok(WorkbenchMenuCommand::SftpFiles)
        );
        model.ssh_control_busy = true;
        model.can_reconnect_ssh = true;
        assert!(command(Action::Reconnect, &model).is_err());
        model.layout_paused = true;
        assert!(command(Action::NewSsh, &model).is_err());
    }
    #[test]
    fn function_bindings_and_profile_schemes_validate_without_mutating_global_defaults(){
        let mut settings=Bindings::default();let original=settings.keys.clone();let profile=cshell_domain::ProfileId::new();let mut private=original.clone();private.insert(Action::Settings,"Ctrl+F8".into());settings.profiles.insert(profile,private);
        assert!(settings.validate().is_ok());assert_eq!(settings.keys,original);assert_eq!(settings.effective(Some(profile)).get(&Action::Settings).map(String::as_str),Some("Ctrl+F8"));assert!(!settings.effective(Some(cshell_domain::ProfileId::new())).contains_key(&Action::Settings));
        for reserved in ["Alt+F4","Shift+F10","F1","Ctrl+C"]{settings.profiles.get_mut(&profile).unwrap().insert(Action::Settings,reserved.into());assert!(settings.validate().is_err(),"{reserved}");}
        settings.profiles.remove(&profile);assert!(settings.validate().is_ok());
    }
    #[test]
    fn palette_search_finds_profiles_tabs_and_quick_commands_as_distinct_actions(){
        let profile=cshell_domain::ProfileId::new();let tab=cshell_domain::TabId::new();let quick=cshell_domain::QuickCommandId::new();let mut model=cshell_ui::WorkbenchViewModel::default();
        model.saved_ssh_profiles.push(cshell_ui::SavedSshProfileViewModel{id:profile,name:"sample-server".into(),target:"operator@sample.example:22".into(),favorite:false,folder_id:None,kind:cshell_domain::ProfileKind::Ssh});
        model.tabs.push(cshell_ui::WorkspaceTabViewModel{id:tab,title:"sample-shell".into(),session_id:None});
        let items=palette_items("sample",&model,&[(quick,"sample-check".into())]);
        let actions=items.iter().filter_map(|i|i.resolve(&model).ok()).collect::<Vec<_>>();
        assert!(actions.contains(&WorkbenchMenuCommand::OpenSavedProfile(profile)));assert!(actions.contains(&WorkbenchMenuCommand::OpenWorkspaceTab(tab)));assert!(actions.contains(&WorkbenchMenuCommand::LoadQuickCommand(quick)));
    }
    #[test]
    fn recording_function_key_changes_only_unsaved_shortcut_draft(){
        let ctx=crate::ui_layout_gate::context();let mut state=State::default();state.show_settings();state.capture=Some(Action::Settings);let original=current().keys;
        let mut model=cshell_ui::WorkbenchViewModel::default();let mut out=ctx.run_ui(egui::RawInput{events:vec![egui::Event::Key{key:egui::Key::F8,physical_key:None,pressed:true,repeat:false,modifiers:egui::Modifiers::NONE}],..Default::default()},|ui|state.draw(ui.ctx(),&mut model));out.textures_delta.clear();
        assert_eq!(state.draft.as_ref().unwrap().keys.get(&Action::Settings).map(String::as_str),Some("F8"));assert_eq!(current().keys,original);assert!(model.menu_command.is_none());
    }
    #[test]
    fn atomic_binding_save_rejects_invalid_edits_without_replacing_previous_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keys.json");
        let mut bindings = Bindings::default();
        save_bindings(&path, &bindings).unwrap();
        let original = std::fs::read(&path).unwrap();
        bindings
            .keys
            .insert(Action::Disconnect, "Ctrl+Shift+R".into());
        assert!(save_bindings(&path, &bindings).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        bindings
            .keys
            .insert(Action::Disconnect, "Ctrl+Shift+D".into());
        save_bindings(&path, &bindings).unwrap();
        assert_eq!(
            serde_json::from_slice::<Bindings>(&std::fs::read(&path).unwrap())
                .unwrap()
                .action("d"),
            Some(Action::Disconnect)
        );
        let blocked = temp.path().join("blocked");
        std::fs::write(&blocked, b"keep").unwrap();
        assert!(save_bindings(&blocked.join("keys.json"), &bindings).is_err());
        assert_eq!(std::fs::read(blocked).unwrap(), b"keep");
    }
    #[test]
    fn filtering_and_enter_in_one_frame_uses_new_query_and_ime_commit_never_executes() {
        let ctx = crate::ui_layout_gate::context();
        let mut state = State::default();
        state.show();
        let mut model = cshell_ui::WorkbenchViewModel {
            can_sftp: true,
            ..Default::default()
        };
        let enter = egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            state.draw(ui.ctx(), &mut model)
        });
        out.textures_delta.clear();
        let mut out = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Text("sftp".into()), enter.clone()],
                ..Default::default()
            },
            |ui| state.draw(ui.ctx(), &mut model),
        );
        out.textures_delta.clear();
        assert_eq!(model.menu_command, Some(WorkbenchMenuCommand::SftpFiles));
        model.menu_command = None;
        state.show();
        state.query = "tunnel".into();
        let mut out = ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::Ime(egui::ImeEvent::Commit("隧道".into())),
                    enter,
                ],
                ..Default::default()
            },
            |ui| state.draw(ui.ctx(), &mut model),
        );
        out.textures_delta.clear();
        assert!(model.menu_command.is_none());
        assert!(state.blocks_input());
    }
    #[test]
    fn keyboard_settings_keep_footer_inside_small_windows() {
        for size in [egui::vec2(720.0, 480.0), egui::vec2(900.0, 600.0)] {
            let ctx = crate::ui_layout_gate::context();
            let mut state = State::default();
            state.show_settings();
            let mut model = cshell_ui::WorkbenchViewModel::default();
            for _ in 0..3 {
                let mut out = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ui| state.draw(ui.ctx(), &mut model),
                );
                out.textures_delta.clear();
                for label in ["保存快捷键", "取消", "恢复默认"] {
                    let tree = out.platform_output.accesskit_update.as_ref().unwrap();
                    let node = &tree
                        .nodes
                        .iter()
                        .find(|(_, n)| n.label() == Some(label))
                        .unwrap()
                        .1;
                    assert!(
                        egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                            .contains_rect(crate::ui_layout_gate::bounds(node)),
                        "{label}"
                    );
                }
            }
        }
    }
    #[test]
    fn search_selects_existing_dispatch_and_escape_closes() {
        let ctx = crate::ui_layout_gate::context();
        let mut state = State::default();
        state.show();
        state.query = "tunnel".into();
        let mut model = cshell_ui::WorkbenchViewModel::default();
        for frame in 0..2 {
            let events = if frame == 1 {
                vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }]
            } else {
                vec![]
            };
            let mut out = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| state.draw(ui.ctx(), &mut model),
            );
            out.textures_delta.clear();
        }
        assert_eq!(model.menu_command, Some(WorkbenchMenuCommand::Tunnels));
        assert!(!state.blocks_input());
        state.show();
        let mut out = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| state.draw(ui.ctx(), &mut model),
        );
        out.textures_delta.clear();
        assert!(!state.blocks_input());
    }
}
