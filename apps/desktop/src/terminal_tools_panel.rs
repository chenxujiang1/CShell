use crate::{
    daemon_connection::DesktopConnectionConfig,
    profile_connection::{
        DesktopProfileView, ProfileClientCommand, ProfileImportFormat, ProfileImportSource,
    },
    replay_worker::ReplayWorker,
    terminal_tools_connection::{Connection, View},
};
use cshell_application::*;
#[cfg(test)]
mod layout_tests;
use cshell_domain::*;
use cshell_ipc::{
    ProfileImportPolicy, TerminalToolsOperation as Op, TerminalToolsRequest as Request,
};
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct Notice {
    pub message: String,
    pub bell: bool,
}
pub(super) struct TerminalToolsPanel {
    pub open: bool,
    dock:crate::command_workspace::Dock,
    shell: crate::shell_history_panel::Panel,
    page: u8,
    connection: Option<Connection>,
    error: Option<String>,
    last_generation: u64,
    last_poll: Option<Instant>,
    draft: Option<TerminalToolsCatalog>,
    dirty: bool,
    submitted: Option<TerminalToolsCatalog>,
    confirm_save: bool,
    edit_epoch: u64,
    import_path: String,
    export_path: String,
    set_index: usize,
    rule_index: usize,
    trigger_index: usize,
    confirm_arm: bool,
    allow_text: bool,
    arm_stamp: Option<(SessionId, u64, bool)>,
    event_cursor: u64,
    seen_event: Option<u64>,
    notices: std::collections::VecDeque<Notice>,
    theme: ThemeRecord,
    theme_confirm: bool,
    theme_path: String,
    theme_import_confirm: bool,
    theme_export: String,
    bound: Option<SessionId>,
    source_epoch: u64,
    sequence: u64,
    offset: u64,
    replay: Option<ReplayWorker>,
    reset_pending: bool,
    playing: bool,
    step: bool,
    speed: f32,
    play_started: Option<Instant>,
    play_origin: Option<u64>,
    last_time: Option<u64>,
    last_untimed: Option<Instant>,
    seek_target: Option<u64>,
    seek_input: u64,
    local_replay: bool,
    local_path: String,
    file: Option<Arc<[u8]>>,
    export_count: u64,
    ansi_path: String,
    scripts: crate::script_authoring_panel::ScriptAuthoringPanel,
}
impl Default for TerminalToolsPanel {
    fn default() -> Self {
        Self {
            open: false,
            dock:Default::default(),
            shell: Default::default(),
            page: 0,
            connection: None,
            error: None,
            last_generation: 0,
            last_poll: None,
            draft: None,
            dirty: false,
            submitted: None,
            confirm_save: false,
            edit_epoch: 0,
            import_path: String::new(),
            export_path: String::new(),
            set_index: 0,
            rule_index: 0,
            trigger_index: 0,
            confirm_arm: false,
            allow_text: false,
            arm_stamp: None,
            event_cursor: 0,
            seen_event: None,
            notices: Default::default(),
            theme: ThemeRecord {
                name: "my-theme".into(),
                palette: cshell_theme::default_palette(),
            },
            theme_confirm: false,
            theme_path: String::new(),
            theme_import_confirm: false,
            theme_export: String::new(),
            bound: None,
            source_epoch: 1,
            sequence: 0,
            offset: 0,
            replay: None,
            reset_pending: false,
            playing: false,
            step: false,
            speed: 1.0,
            play_started: None,
            play_origin: None,
            last_time: None,
            last_untimed: None,
            seek_target: None,
            seek_input: 0,
            local_replay: false,
            local_path: String::new(),
            file: None,
            export_count: 1,
            ansi_path: String::new(),
            scripts: Default::default(),
        }
    }
}
impl TerminalToolsPanel {
    pub fn open_commands(&mut self){self.dock.toggle();}
    pub fn take_command_load(&mut self)->Option<String>{self.shell.load_command.take()}
    pub fn draw_command_dock(&mut self,ui:&mut egui::Ui,session:Option<SessionId>){
        let view=self.connection.as_ref().map(Connection::view).unwrap_or_default();let connection=self.connection.as_ref();let shell=&mut self.shell;
        self.dock.draw(ui,|ui|shell.draw(ui,session,&view,connection));
    }
    pub fn take_command_jump(&mut self)->Option<u64>{let line=self.shell.jump.take();if line.is_some(){self.open=false;}line}
    pub fn record_input_omission(&mut self, session: SessionId, broadcast: bool) {
        self.scripts.input_omitted(session, broadcast);
    }
    pub fn poll(&mut self, config: Option<&DesktopConnectionConfig>) -> bool {
        let script_changed = self.scripts.poll() | self.scripts.poll_live(config);
        if self.connection.is_none()
            && self.error.is_none()
            && let Some(config) = config
        {
            match Connection::start(config.clone()) {
                Ok(c) => {
                    c.request(Request::default(), 0, true);
                    self.connection = Some(c);
                }
                Err(_) => self.error = Some("终端工具后台线程无法启动".into()),
            }
        }
        let Some(view) = self.connection.as_ref().map(Connection::view) else {
            return script_changed;
        };
        let changed = self.last_generation != view.generation;
        self.last_generation = view.generation;
        if changed {
            if let (Some(saved), Some(submitted)) = (&view.saved, &self.submitted) {
                let mut expected = submitted.clone();
                expected.revision = expected.revision.saturating_add(1);
                if saved.as_ref() == &expected && self.draft.as_ref() == Some(submitted) {
                    self.draft = Some(saved.as_ref().clone());
                    self.dirty = false;
                    self.confirm_save = false;
                    self.submitted = None;
                }
            }
            if let Some(authority) = &view.catalog {
                if self.draft.is_none()
                    || (!self.dirty
                        && self
                            .draft
                            .as_ref()
                            .is_some_and(|d| d.revision != authority.catalog.revision))
                {
                    self.draft = Some(authority.catalog.clone());
                    self.confirm_save = false;
                    self.edit_epoch = self.edit_epoch.wrapping_add(1);
                }
                if self.seen_event.is_none() {
                    self.seen_event = Some(authority.next_event);
                    self.event_cursor = authority.next_event;
                } else {
                    for event in &authority.events {
                        if event.id > self.seen_event.unwrap_or(0) {
                            match &event.action {
                                TriggerAction::Notify { message } => {
                                    self.notices.push_back(Notice {
                                        message: message.clone(),
                                        bell: false,
                                    })
                                }
                                TriggerAction::Bell => self.notices.push_back(Notice {
                                    message: format!("触发器 {}：铃声", event.rule_id),
                                    bell: true,
                                }),
                                _ => {}
                            }
                            self.seen_event = Some(event.id);
                            self.event_cursor = self.event_cursor.max(event.id);
                        }
                    }
                    if authority.lost_before > self.event_cursor {
                        self.event_cursor = authority.lost_before;
                    }
                    while self.notices.len() > 16 {
                        self.notices.pop_front();
                    }
                }
            }
            if let Some((epoch, doc)) = &view.imported
                && *epoch == self.edit_epoch
                && let Some(draft) = &mut self.draft
            {
                draft.highlights = doc.sets.clone();
                self.dirty = true;
                self.confirm_save = false;
                self.edit_epoch = self.edit_epoch.wrapping_add(1);
            }
            if let Some((epoch, bytes)) = &view.file
                && *epoch == self.source_epoch
            {
                self.file = Some(bytes.clone());
            }
        }
        if self.open && self.page == 4 {
            self.advance_replay(&view);
        }
        if !view.busy
            && self.last_poll.is_none_or(|t| {
                t.elapsed()
                    > if self.open {
                        Duration::from_millis(700)
                    } else {
                        Duration::from_secs(2)
                    }
            })
            && let Some(c) = &self.connection
        {
            if c.request(
                Request {
                    operation: Op::Poll as i32,
                    cursor: self.event_cursor,
                    ..Default::default()
                },
                0,
                false,
            ) {
                self.last_poll = Some(Instant::now());
            }
        }
        changed || script_changed
    }
    pub fn take_notice(&mut self) -> Option<Notice> {
        self.notices.pop_front()
    }
    pub fn highlight_rules(
        &self,
        session: SessionId,
        profile: Option<ProfileId>,
    ) -> (u64, Vec<HighlightRule>) {
        let Some(view) = self
            .connection
            .as_ref()
            .map(Connection::view)
            .and_then(|v| v.catalog)
        else {
            return (0, Vec::new());
        };
        let armed = view.armed.iter().any(|a| a.session == session);
        let mut rules = view.catalog.highlight_rules(profile);
        if armed {
            for r in &view.catalog.triggers {
                if r.enabled
                    && (r.profiles.is_empty() || profile.is_some_and(|p| r.profiles.contains(&p)))
                    && let TriggerAction::Highlight { style } = r.action
                {
                    rules.push(HighlightRule {
                        id: r.id.clone(),
                        pattern: r.pattern.clone(),
                        style,
                        enabled: true,
                    });
                }
            }
        }
        (
            view.catalog.revision.wrapping_mul(2) ^ u64::from(armed),
            rules,
        )
    }
    fn edited(&mut self) {
        self.dirty = true;
        self.confirm_save = false;
        self.confirm_arm = false;
        self.edit_epoch = self.edit_epoch.wrapping_add(1);
    }
    fn reset_replay(&mut self) {
        self.source_epoch = self.source_epoch.wrapping_add(1);
        self.sequence = 0;
        self.offset = 0;
        self.playing = false;
        self.step = false;
        self.play_started = None;
        self.play_origin = None;
        self.last_time = None;
        self.last_untimed = None;
        self.seek_target = None;
        self.reset_pending = true;
    }
    fn raw_request(&self) -> Option<Request> {
        Some(Request {
            operation: Op::RawJournal as i32,
            session_id: self.bound?.as_uuid().as_bytes().to_vec(),
            cursor: self.sequence,
            offset: self.offset,
            ..Default::default()
        })
    }
    fn advance_replay(&mut self, view: &View) {
        if self.replay.is_none() {
            match ReplayWorker::start() {
                Ok(w) => {
                    self.replay = Some(w);
                    self.reset_pending = true;
                }
                Err(_) => {
                    self.error = Some("回放线程无法启动".into());
                    return;
                }
            }
        }
        let Some(worker) = &self.replay else {
            return;
        };
        if self.reset_pending {
            if worker.reset(self.source_epoch) {
                self.reset_pending = false;
            } else {
                return;
            }
        }
        if self
            .seek_target
            .is_some_and(|target| self.sequence >= target)
        {
            self.seek_target = None;
            self.playing = false;
        }
        if !self.playing && !self.step && self.seek_target.is_none() {
            return;
        }
        let raw = if self.local_replay {
            let Some(bytes) = &self.file else {
                return;
            };
            let start = usize::try_from(self.sequence)
                .ok()
                .and_then(|n| n.checked_mul(64 * 1024));
            let Some(start) = start else {
                return;
            };
            if start >= bytes.len() {
                self.playing = false;
                self.step = false;
                self.seek_target = None;
                return;
            }
            Some(Arc::new(RawJournalSlice {
                session: self.bound.unwrap_or_default(),
                sequence: self.sequence,
                offset: 0,
                record_bytes: (bytes.len() - start).min(64 * 1024) as u64,
                total_records: bytes.len().div_ceil(64 * 1024) as u64,
                data: bytes[start..(start + 64 * 1024).min(bytes.len())].to_vec(),
                elapsed_us: None,
                rows: None,
                cols: None,
            }))
        } else {
            view.raw
                .as_ref()
                .filter(|r| {
                    view.raw_epoch == self.source_epoch
                        && Some(r.session) == self.bound
                        && r.sequence == self.sequence
                        && r.offset == self.offset
                })
                .cloned()
        };
        let Some(raw) = raw else {
            if !view.busy
                && let (Some(c), Some(request)) = (&self.connection, self.raw_request())
            {
                c.request(request, self.source_epoch, false);
            }
            return;
        };
        if raw.sequence >= raw.total_records {
            self.playing = false;
            self.step = false;
            self.seek_target = None;
            return;
        }
        if self.playing && !self.step && self.seek_target.is_none() && self.offset == 0 {
            if let Some(t) = raw.elapsed_us {
                let origin = *self.play_origin.get_or_insert(t);
                let started = *self.play_started.get_or_insert_with(Instant::now);
                let elapsed = (started.elapsed().as_micros() as f64 * f64::from(self.speed)) as u64;
                if t > origin.saturating_add(elapsed) {
                    return;
                }
            } else if self
                .last_untimed
                .is_some_and(|t| t.elapsed() < Duration::from_secs_f32(0.1 / self.speed))
            {
                return;
            }
        }
        if worker.feed(self.source_epoch, raw.data.clone(), raw.rows, raw.cols) {
            self.last_time = raw.elapsed_us;
            self.last_untimed = Some(Instant::now());
            let next = raw.next_cursor();
            self.sequence = next.0;
            self.offset = next.1;
            if self.step && self.offset == 0 {
                self.step = false;
            }
        }
    }
    pub fn draw(
        &mut self,
        context: &egui::Context,
        session: Option<SessionId>,
        profile: Option<ProfileId>,
        profiles: Option<&DesktopProfileView>,
    ) -> Option<ProfileClientCommand> {
        self.scripts.observe_session(session);
        if self.scripts.indicator(context) {
            self.open = true;
            self.page = 5;
        }
        if !self.open {
            return None;
        }
        context.request_repaint_after(Duration::from_millis(40));
        let view = self
            .connection
            .as_ref()
            .map(Connection::view)
            .unwrap_or_default();
        let mut open = self.open;
        let mut profile_command = None;
        cshell_ui::form::dialog(&context, "终端工具")
            .open(&mut open)
            .default_size([860.0, 760.0])
            .show(context, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (page, label) in [
                        (0, "主题"),
                        (1, "高亮规则"),
                        (2, "触发器"),
                        (3, "十六进制"),
                        (4, "ANSI 回放"),
                        (5, "脚本草稿"),
                        (6, "命令历史"),
                    ] {
                        if cshell_ui::form::tab(ui, &mut self.page, page, label).changed() {
                            self.playing = false;
                            self.step = false;
                            self.seek_target = None;
                            if self.page == 4 {
                                let target = self.sequence;
                                let file = self.file.clone();
                                self.reset_replay();
                                self.file = file;
                                if target > 0 {
                                    self.seek_target = Some(target);
                                }
                            }
                        }
                    }
                });
                if let Some(error) = self.error.as_ref().or(view.error.as_ref()) {
                    ui.colored_label(cshell_ui::theme::WARNING, error);
                }
                if view.busy {
                    ui.spinner();
                }
                if !view.status.is_empty() {
                    ui.weak(&view.status);
                }
                if self.page == 0 {
                    profile_command = self.themes(ui, profile, profiles);
                } else if self.page <= 2 {
                    self.rules(ui, &view, session, profile);
                } else if self.page == 6 {
                    self.shell.draw(ui,session,&view,self.connection.as_ref());
                } else if self.page == 5 {
                    self.scripts.draw(ui, session);
                } else {
                    self.journal(ui, &view, session);
                }
            });
        if !open {
            self.playing = false;
            self.step = false;
            self.seek_target = None;
        }
        if self.open && !open {context.memory_mut(|m|{if let Some(id)=m.focused(){m.surrender_focus(id);}});}
        self.open = open;
        profile_command
    }
    fn themes(
        &mut self,
        ui: &mut egui::Ui,
        profile: Option<ProfileId>,
        view: Option<&DesktopProfileView>,
    ) -> Option<ProfileClientCommand> {
        let mut command = None;
        let catalog = view.and_then(|v| v.catalog.as_ref());
        let mut changed = false;
        ui.weak(
            "编辑调色板并明确保存；选择 Profile 中的主题引用后生效。预览不写入会话或持久配置。",
        );
        if let Some(catalog) = catalog {
            ui.horizontal_wrapped(|ui| {
                for theme in &catalog.themes {
                    if ui.button(&theme.name).clicked() {
                        self.theme = theme.clone();
                        self.theme_confirm = false;
                    }
                }
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("主题名称");
            changed |= ui.text_edit_singleline(&mut self.theme.name).changed();
            if ui.button("复制默认调色板").clicked() {
                self.theme.palette = cshell_theme::default_palette();
                changed = true;
            }
        });
        egui::Grid::new("theme-palette-editor")
            .num_columns(4)
            .show(ui, |ui| {
                for (label, color) in [
                    ("前景", &mut self.theme.palette.foreground),
                    ("背景", &mut self.theme.palette.background),
                    ("光标", &mut self.theme.palette.cursor),
                    ("选择", &mut self.theme.palette.selection),
                ] {
                    ui.label(label);
                    changed |= ui.color_edit_button_srgb(color).changed();
                    ui.end_row();
                }
                for (index, color) in self.theme.palette.ansi.iter_mut().enumerate() {
                    ui.label(format!("ANSI {index}"));
                    changed |= ui.color_edit_button_srgb(color).changed();
                    if index % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
        ui.horizontal_wrapped(|ui| {
            for (index, color) in self.theme.palette.ansi.iter().enumerate() {
                ui.label(
                    egui::RichText::new(format!(" {index:02} Aa "))
                        .color(egui::Color32::from_rgb(color[0], color[1], color[2]))
                        .background_color(egui::Color32::from_rgb(
                            self.theme.palette.background[0],
                            self.theme.palette.background[1],
                            self.theme.palette.background[2],
                        )),
                );
            }
        });
        if changed {
            self.theme_confirm = false;
        }
        ui.checkbox(&mut self.theme_confirm, "确认创建或替换这个主题");
        if ui
            .add_enabled(
                self.theme_confirm && catalog.is_some(),
                egui::Button::new("保存主题"),
            )
            .clicked()
            && let Some(catalog) = catalog
        {
            command = Some(ProfileClientCommand::SaveTheme {
                expected_revision: catalog.revision,
                theme: self.theme.clone(),
            });
            self.theme_confirm = false;
        }
        if let Some(catalog) = catalog
            && catalog.themes.iter().any(|t| t.name == self.theme.name)
            && let Some(profile) = profile
            && let Some(record) = catalog.profiles.iter().find(|r| r.id == profile)
            && ui.button("将已保存的此名称主题用于当前 Profile").clicked()
        {
            let mut record = record.clone();
            record.terminal.theme = Some(self.theme.name.clone());
            command = Some(ProfileClientCommand::Apply {
                expected_revision: catalog.revision,
                changes: vec![cshell_ipc::ProfileChange {
                    change: Some(cshell_ipc::profile_change::Change::UpsertProfile(
                        cshell_ipc::ProfileRecordData::from(&record),
                    )),
                }],
            });
        }
        ui.separator();
        ui.label("导入 Theme JSON / TOML / .xcs");
        if ui.text_edit_singleline(&mut self.theme_path).changed() {
            self.theme_import_confirm = false;
        }
        if ui.button("预览主题文件").clicked() {
            command = Some(ProfileClientCommand::PreviewImport(ProfileImportSource {
                path: self.theme_path.clone(),
                policy: ProfileImportPolicy::Replace,
                format: ProfileImportFormat::Theme,
                default_username: String::new(),
            }));
            self.theme_import_confirm = false;
        }
        ui.weak("XCS 亮色映射 ANSI 8～15；光标采用前景、选择采用蓝色，独立 text(bold) 不进入 v1 调色板。");
        if let Some(view) = view
            && view.preview_source.as_ref().is_some_and(|s| {
                s.format == ProfileImportFormat::Theme && s.path == self.theme_path
            })
            && let Some(preview) = &view.preview
        {
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    for item in &preview.items {
                        ui.label(format!("{} · {}", item.name, item.details));
                    }
                });
            ui.checkbox(
                &mut self.theme_import_confirm,
                "确认提交当前文件的全部预览主题",
            );
            if ui
                .add_enabled(
                    self.theme_import_confirm
                        && preview.can_commit
                        && catalog.is_some_and(|c| c.revision == preview.base_revision),
                    egui::Button::new("提交主题导入"),
                )
                .clicked()
            {
                command = Some(ProfileClientCommand::CommitImport {
                    expected_revision: preview.base_revision,
                });
                self.theme_import_confirm = false;
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Theme JSON / TOML 导出路径");
            ui.text_edit_singleline(&mut self.theme_export);
            if ui
                .add_enabled(catalog.is_some(), egui::Button::new("导出主题"))
                .clicked()
                && let Some(catalog) = catalog
            {
                command = Some(ProfileClientCommand::ExportThemes {
                    path: self.theme_export.clone(),
                    expected_revision: catalog.revision,
                });
            }
        });
        command
    }
    fn rules(
        &mut self,
        ui: &mut egui::Ui,
        view: &View,
        session: Option<SessionId>,
        profile: Option<ProfileId>,
    ) {
        if ui
            .add_enabled(!view.busy, egui::Button::new("明确重新读取配置 / 恢复编辑"))
            .clicked()
            && let Some(c) = &self.connection
        {
            self.draft = None;
            self.dirty = false;
            self.submitted = None;
            self.seen_event = None;
            self.event_cursor = 0;
            self.confirm_save = false;
            self.confirm_arm = false;
            c.request(Request::default(), 0, true);
        }
        let Some(draft) = &mut self.draft else {
            ui.label("尚未读取配置");
            return;
        };
        let mut changed = false;
        if self.page == 1 {
            ui.weak("高亮只覆盖显示样式，后定义规则优先；不会改变终端单元或发送文本。空 Profile 范围表示全部会话。");
            ui.horizontal_wrapped(|ui| {
                for (i, set) in draft.highlights.iter().enumerate() {
                    if ui
                        .selectable_value(&mut self.set_index, i, &set.name)
                        .changed()
                    {
                        self.rule_index = 0;
                    }
                }
                if ui.button("新建 HighlightSet").clicked() && draft.highlights.len() < 16 {
                    draft.highlights.push(HighlightSet {
                        name: format!("set-{}", draft.highlights.len() + 1),
                        profiles: BTreeSet::new(),
                        rules: Vec::new(),
                    });
                    self.set_index = draft.highlights.len() - 1;
                    changed = true;
                }
            });
            self.set_index = self.set_index.min(draft.highlights.len().saturating_sub(1));
            if let Some(set) = draft.highlights.get_mut(self.set_index) {
                ui.horizontal_wrapped(|ui| {
                    ui.label("集合名称");
                    changed |= ui.text_edit_singleline(&mut set.name).changed();
                });
                changed |= scope_ui(ui, &mut set.profiles, profile);
                ui.horizontal_wrapped(|ui| {
                    for (i, rule) in set.rules.iter().enumerate() {
                        ui.selectable_value(&mut self.rule_index, i, &rule.id);
                    }
                    if ui.button("新增规则").clicked() {
                        set.rules.push(HighlightRule {
                            id: format!("rule-{}", set.rules.len() + 1),
                            pattern: OutputPattern::Literal {
                                text: "ERROR".into(),
                                case_sensitive: false,
                            },
                            style: HighlightStyle {
                                foreground: Some([255, 80, 80]),
                                ..Default::default()
                            },
                            enabled: true,
                        });
                        self.rule_index = set.rules.len() - 1;
                        changed = true;
                    }
                });
                self.rule_index = self.rule_index.min(set.rules.len().saturating_sub(1));
                if let Some(rule) = set.rules.get_mut(self.rule_index) {
                    changed |= ui.text_edit_singleline(&mut rule.id).changed();
                    changed |= ui.checkbox(&mut rule.enabled, "启用高亮规则").changed();
                    changed |= pattern_ui(ui, &mut rule.pattern);
                    changed |= style_ui(ui, &mut rule.style);
                    if ui.button("移除当前规则").clicked() {
                        set.rules.remove(self.rule_index);
                        changed = true;
                    }
                }
                if ui.button("移除当前集合").clicked() {
                    draft.highlights.remove(self.set_index);
                    changed = true;
                }
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("HighlightSet JSON 输入");
                ui.text_edit_singleline(&mut self.import_path);
                if ui
                    .add_enabled(!view.busy, egui::Button::new("预览导入集合"))
                    .clicked()
                    && let Some(c) = &self.connection
                {
                    self.edit_epoch = self.edit_epoch.wrapping_add(1);
                    c.import(self.import_path.clone(), self.edit_epoch);
                }
            });
            ui.weak("导入预览替换草稿中的集合；触发器保持当前草稿，按当前版本明确保存后生效。");
            ui.horizontal_wrapped(|ui| {
                ui.label("导出路径");
                ui.text_edit_singleline(&mut self.export_path);
                if ui
                    .add_enabled(
                        !view.busy && view.catalog.is_some(),
                        egui::Button::new("导出已保存集合"),
                    )
                    .clicked()
                    && let (Some(c), Some(authority)) = (&self.connection, &view.catalog)
                {
                    c.export(
                        self.export_path.clone(),
                        Arc::new(authority.catalog.clone()),
                    );
                }
            });
        } else {
            ui.weak("只匹配启用后的实时输出。冷却至少 1 秒，每条规则每输出序号最多一次；追加文本每次启用最多执行一次，不自动重新启用。");
            ui.horizontal_wrapped(|ui| {
                for (i, rule) in draft.triggers.iter().enumerate() {
                    ui.selectable_value(&mut self.trigger_index, i, &rule.id);
                }
                if ui.button("新增触发器").clicked() && draft.triggers.len() < 64 {
                    draft.triggers.push(TriggerRule {
                        id: format!("trigger-{}", draft.triggers.len() + 1),
                        profiles: profile.into_iter().collect(),
                        pattern: OutputPattern::Literal {
                            text: "ERROR".into(),
                            case_sensitive: false,
                        },
                        action: TriggerAction::Notify {
                            message: "匹配 ERROR".into(),
                        },
                        cooldown_ms: 5000,
                        per_minute: 10,
                        enabled: true,
                    });
                    self.trigger_index = draft.triggers.len() - 1;
                    changed = true;
                }
            });
            self.trigger_index = self
                .trigger_index
                .min(draft.triggers.len().saturating_sub(1));
            if let Some(rule) = draft.triggers.get_mut(self.trigger_index) {
                changed |= ui.text_edit_singleline(&mut rule.id).changed();
                changed |= ui.checkbox(&mut rule.enabled, "纳入启用配置").changed();
                changed |= scope_ui(ui, &mut rule.profiles, profile);
                changed |= pattern_ui(ui, &mut rule.pattern);
                ui.horizontal_wrapped(|ui| {
                    ui.label("冷却毫秒");
                    changed |= ui
                        .add(egui::DragValue::new(&mut rule.cooldown_ms).range(1000..=3_600_000))
                        .changed();
                    ui.label("每分钟上限");
                    changed |= ui
                        .add(egui::DragValue::new(&mut rule.per_minute).range(1..=60))
                        .changed();
                });
                let mut kind = action_kind(&rule.action);
                let before = kind;
                cshell_ui::form::combo(ui, "动作")
                    .truncate()
                    .selected_text(
                        ["高亮", "通知", "铃声", "追加文本", "记录事件", "脚本请求"]
                            [usize::from(kind)],
                    )
                    .show_ui(ui, |ui| {
                        cshell_ui::form::menu(ui);
                        for (i, label) in
                            ["高亮", "通知", "铃声", "追加文本", "记录事件", "脚本请求"]
                                .iter()
                                .enumerate()
                        {
                            ui.selectable_value(&mut kind, i as u8, *label);
                        }
                    });
                if kind != before {
                    rule.action = match kind {
                        0 => TriggerAction::Highlight {
                            style: HighlightStyle {
                                background: Some([100, 20, 20]),
                                ..Default::default()
                            },
                        },
                        1 => TriggerAction::Notify {
                            message: "输出匹配".into(),
                        },
                        2 => TriggerAction::Bell,
                        3 => TriggerAction::AppendText {
                            text: String::new(),
                        },
                        4 => TriggerAction::Record {
                            label: "输出匹配".into(),
                        },
                        _ => TriggerAction::Script {
                            script_id: String::new(),
                        },
                    };
                    changed = true;
                }
                match &mut rule.action {
                    TriggerAction::Highlight { style } => changed |= style_ui(ui, style),
                    TriggerAction::Notify { message }
                    | TriggerAction::Record { label: message } => {
                        changed |= ui.text_edit_singleline(message).changed()
                    }
                    TriggerAction::AppendText { text } => {
                        changed |= ui
                            .add(egui::TextEdit::multiline(text).desired_rows(3))
                            .changed();
                        ui.weak("不会添加换行。每次启用至多一次，不明投递不重试。");
                    }
                    TriggerAction::Script { script_id } => {
                        changed |= ui.text_edit_singleline(script_id).changed();
                        ui.weak(
                            "产生脚本能力请求；由 P3-060 的脚本权限执行器处理，此处不执行程序。",
                        );
                    }
                    TriggerAction::Bell => {}
                }
                if ui.button("移除当前触发器").clicked() {
                    draft.triggers.remove(self.trigger_index);
                    changed = true;
                }
            }
        }
        if changed {
            self.edited();
        }
        let Some(draft) = &self.draft else {
            return;
        };
        let ready = !view.busy
            && view.writable
            && view
                .catalog
                .as_ref()
                .is_some_and(|c| c.catalog.revision == draft.revision);
        ui.separator();
        ui.checkbox(
            &mut self.confirm_save,
            "确认按当前版本保存全部高亮和触发器定义；停止现有触发器运行",
        );
        if ui
            .add_enabled(
                ready && self.confirm_save,
                egui::Button::new("保存规则配置"),
            )
            .clicked()
            && let Some(c) = &self.connection
            && let Ok(document_json) = serde_json::to_vec(draft)
        {
            self.submitted = Some(draft.clone());
            c.request(
                Request {
                    operation: Op::Save as i32,
                    expected_revision: draft.revision,
                    document_json,
                    confirmed: true,
                    ..Default::default()
                },
                0,
                false,
            );
            self.confirm_save = false;
        }
        if !ready {
            ui.weak(
                "配置版本变化或写入结果不明时，请明确重新读取。读取和保存都不会自动启用触发器。",
            );
        }
        if self.page == 2
            && let Some(session) = session
        {
            let revision = view.catalog.as_ref().map_or(0, |c| c.catalog.revision);
            let stamp = (session, revision, self.allow_text);
            if self.arm_stamp != Some(stamp) {
                self.confirm_arm = false;
                self.arm_stamp = Some(stamp);
            }
            if ui
                .checkbox(
                    &mut self.allow_text,
                    "授权本次启用的追加文本动作（每规则一次）",
                )
                .changed()
            {
                self.confirm_arm = false;
            }
            ui.checkbox(&mut self.confirm_arm, "确认只在当前会话启用已保存规则");
            if ui
                .add_enabled(
                    ready && !self.dirty && self.confirm_arm,
                    egui::Button::new("在当前会话启用触发器"),
                )
                .clicked()
                && let Some(c) = &self.connection
            {
                c.request(
                    Request {
                        operation: Op::Arm as i32,
                        expected_revision: revision,
                        session_id: session.as_uuid().as_bytes().to_vec(),
                        allow_text: self.allow_text,
                        confirmed: true,
                        ..Default::default()
                    },
                    0,
                    false,
                );
                self.confirm_arm = false;
            }
            if ui
                .add_enabled(!view.busy, egui::Button::new("停止当前会话触发器"))
                .clicked()
                && let Some(c) = &self.connection
            {
                c.request(
                    Request {
                        operation: Op::Disarm as i32,
                        session_id: session.as_uuid().as_bytes().to_vec(),
                        ..Default::default()
                    },
                    0,
                    false,
                );
            }
        }
        if let Some(authority) = &view.catalog {
            for run in &authority.armed {
                ui.label(format!(
                    "启用中：{} · 定义版本 {} · text {}",
                    run.session, run.catalog_revision, run.allow_text
                ));
            }
            egui::ScrollArea::vertical()
                .max_height(130.0)
                .show(ui, |ui| {
                    for event in &authority.events {
                        ui.label(format!(
                            "#{} · {} · {} · {}",
                            event.id, event.rule_id, event.output_sequence, event.disposition
                        ));
                        if let TriggerAction::Script { script_id } = &event.action {
                            ui.label(format!("脚本 {script_id}；会话 {}", event.session));
                            if ui
                                .button(format!("核对事件 #{} 的单次脚本执行", event.id))
                                .clicked()
                            {
                                self.scripts.select_trigger_event(event.id);
                                self.page = 5;
                            }
                        }
                    }
                });
        }
    }
    fn journal(&mut self, ui: &mut egui::Ui, view: &View, session: Option<SessionId>) {
        ui.weak(
            "只读。回放不发送终端响应、不执行触发器、剪贴板或脚本；会话输入不在原始输出日志内。",
        );
        ui.horizontal_wrapped(|ui| {
            if ui.button("绑定当前会话 / 从头重放").clicked() {
                self.bound = session;
                self.local_replay = false;
                self.file = None;
                self.reset_replay();
            }
            ui.label(
                self.bound
                    .map_or_else(|| "未绑定日志会话".into(), |id| id.to_string()),
            );
        });
        if self.page == 3 {
            ui.horizontal_wrapped(|ui| {
                ui.label("输出序号");
                if ui.add(egui::DragValue::new(&mut self.sequence)).changed() {
                    self.offset = 0;
                    self.source_epoch = self.source_epoch.wrapping_add(1);
                }
                ui.label("记录内字节偏移");
                if ui.add(egui::DragValue::new(&mut self.offset)).changed() {
                    self.source_epoch = self.source_epoch.wrapping_add(1);
                }
                if ui
                    .add_enabled(
                        !view.busy && self.bound.is_some(),
                        egui::Button::new("读取 Hex 片段"),
                    )
                    .clicked()
                    && let (Some(c), Some(req)) = (&self.connection, self.raw_request())
                {
                    c.request(req, self.source_epoch, false);
                }
            });
            if let Some(raw) = &view.raw
                && view.raw_epoch == self.source_epoch
                && Some(raw.session) == self.bound
                && raw.sequence == self.sequence
                && raw.offset == self.offset
            {
                ui.label(format!(
                    "记录 {} / {} · {} 字节 · 时间 {:?} μs",
                    raw.sequence, raw.total_records, raw.record_bytes, raw.elapsed_us
                ));
                ui.horizontal_wrapped(|ui| {
                    if ui.button("前 64 KiB").clicked() {
                        self.offset = self.offset.saturating_sub(64 * 1024);
                        self.source_epoch = self.source_epoch.wrapping_add(1);
                    }
                    if ui.button("下一片段").clicked() {
                        (self.sequence, self.offset) = raw.next_cursor();
                        self.source_epoch = self.source_epoch.wrapping_add(1);
                    }
                });
                egui::ScrollArea::vertical().max_height(430.0).show_rows(
                    ui,
                    18.0,
                    raw.data.len().div_ceil(16),
                    |ui, range| {
                        for i in range {
                            let start = i * 16;
                            ui.monospace(hex_line(
                                raw.offset + start as u64,
                                &raw.data[start..(start + 16).min(raw.data.len())],
                            ));
                        }
                    },
                );
            }
        } else {
            ui.horizontal_wrapped(|ui| {
                ui.label("本地 ANSI 文件（最多 8 MiB，无时间信息）");
                ui.text_edit_singleline(&mut self.local_path);
                if ui
                    .add_enabled(!view.busy, egui::Button::new("载入只读文件"))
                    .clicked()
                {
                    self.local_replay = true;
                    self.file = None;
                    self.reset_replay();
                    if let Some(c) = &self.connection {
                        c.replay_file(self.local_path.clone(), self.source_epoch);
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(if self.playing || self.seek_target.is_some() {
                        "暂停 / 取消定位"
                    } else {
                        "播放"
                    })
                    .clicked()
                {
                    if self.seek_target.is_some() {
                        self.seek_target = None;
                        self.playing = false;
                        self.step = false;
                    } else {
                        self.playing = !self.playing;
                    }
                    self.play_started = Some(Instant::now());
                    self.play_origin = self.last_time;
                }
                if ui.button("单步一条记录").clicked() {
                    self.playing = false;
                    self.step = true;
                }
                if ui.button("从头重放").clicked() {
                    let file = self.file.clone();
                    self.reset_replay();
                    self.file = file;
                }
                ui.label("倍速");
                if ui
                    .add(egui::Slider::new(&mut self.speed, 0.25..=8.0))
                    .changed()
                {
                    self.play_started = Some(Instant::now());
                    self.play_origin = self.last_time;
                }
                ui.label(format!("当前位置 #{} +{}", self.sequence, self.offset));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("跳至输出序号（从头恢复此前记录）");
                ui.add(egui::DragValue::new(&mut self.seek_input));
                if ui.button("定位").clicked() {
                    let file = self.file.clone();
                    self.reset_replay();
                    self.file = file;
                    self.seek_target = Some(self.seek_input);
                }
            });
            if let Some(worker) = &self.replay {
                let state = worker.view();
                if state.epoch == self.source_epoch
                    && let Some(snapshot) = state.snapshot
                {
                    draw_replay(ui, &snapshot, self.theme.palette);
                }
            }
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label("ANSI 导出（从上述序号起，最多 4096 条 / 8 MiB）");
            ui.add(egui::DragValue::new(&mut self.export_count).range(1..=4096));
        });
        ui.horizontal_wrapped(|ui| {
            ui.text_edit_singleline(&mut self.ansi_path);
            if ui
                .add_enabled(
                    !view.busy && self.bound.is_some() && !self.local_replay,
                    egui::Button::new("导出该范围原始输出"),
                )
                .clicked()
                && let (Some(c), Some(mut req)) = (&self.connection, self.raw_request())
            {
                req.offset = 0;
                c.export_ansi(req, self.export_count, self.ansi_path.clone());
            }
        });
    }
}
fn action_kind(action: &TriggerAction) -> u8 {
    match action {
        TriggerAction::Highlight { .. } => 0,
        TriggerAction::Notify { .. } => 1,
        TriggerAction::Bell => 2,
        TriggerAction::AppendText { .. } => 3,
        TriggerAction::Record { .. } => 4,
        TriggerAction::Script { .. } => 5,
    }
}
fn scope_ui(
    ui: &mut egui::Ui,
    scope: &mut BTreeSet<ProfileId>,
    profile: Option<ProfileId>,
) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(if scope.is_empty() {
            "范围：全部 Profile / 未保存会话".into()
        } else {
            format!("范围：{} 个 Profile", scope.len())
        });
        if ui.button("全部").clicked() {
            scope.clear();
            changed = true;
        }
        if ui
            .add_enabled(profile.is_some(), egui::Button::new("仅当前 Profile"))
            .clicked()
        {
            *scope = profile.into_iter().collect();
            changed = true;
        }
    });
    changed
}
fn pattern_ui(ui: &mut egui::Ui, pattern: &mut OutputPattern) -> bool {
    let mut regex = matches!(pattern, OutputPattern::Regex { .. });
    let mut changed = ui
        .checkbox(&mut regex, "受限正则（最多 512 字节）")
        .changed();
    if changed {
        let old = pattern.clone();
        let (text, case_sensitive) = match old {
            OutputPattern::Literal {
                text,
                case_sensitive,
            }
            | OutputPattern::Regex {
                text,
                case_sensitive,
            } => (text, case_sensitive),
        };
        *pattern = if regex {
            OutputPattern::Regex {
                text,
                case_sensitive,
            }
        } else {
            OutputPattern::Literal {
                text,
                case_sensitive,
            }
        };
    }
    match pattern {
        OutputPattern::Literal {
            text,
            case_sensitive,
        }
        | OutputPattern::Regex {
            text,
            case_sensitive,
        } => {
            changed |= ui.text_edit_singleline(text).changed();
            changed |= ui.checkbox(case_sensitive, "区分大小写").changed();
        }
    }
    changed
}
fn style_ui(ui: &mut egui::Ui, style: &mut HighlightStyle) -> bool {
    let mut changed = false;
    for (label, value, default) in [
        ("覆盖前景", &mut style.foreground, [255, 220, 80]),
        ("覆盖背景", &mut style.background, [100, 25, 25]),
    ] {
        ui.horizontal_wrapped(|ui| {
            let mut enabled = value.is_some();
            if ui.checkbox(&mut enabled, label).changed() {
                *value = enabled.then_some(default);
                changed = true;
            }
            if let Some(color) = value {
                changed |= ui.color_edit_button_srgb(color).changed();
            }
        });
    }
    changed |= ui.checkbox(&mut style.underline, "下划线").changed();
    changed
}
pub(super) fn read_highlight_file(path: &str) -> Result<HighlightDocument, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| "HighlightSet 文件打开失败".to_owned())?;
    if !file
        .metadata()
        .map_err(|_| "HighlightSet 文件检查失败".to_owned())?
        .is_file()
    {
        return Err("HighlightSet 输入必须是普通文件".into());
    }
    let mut bytes = Vec::new();
    file.take((MAX_TERMINAL_TOOLS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "HighlightSet 文件读取失败".to_owned())?;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    HighlightDocument::parse(bytes).map_err(str::to_owned)
}
pub(super) fn hex_line(offset: u64, bytes: &[u8]) -> String {
    let hex = bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let text = bytes
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                char::from(*b)
            } else {
                '.'
            }
        })
        .collect::<String>();
    format!("{offset:012x}  {hex:<47}  |{text}|")
}
fn replay_color(
    color: cshell_terminal::Color,
    foreground: bool,
    palette: TerminalPalette,
) -> [u8; 3] {
    match color {
        cshell_terminal::Color::Default => {
            if foreground {
                palette.foreground
            } else {
                palette.background
            }
        }
        cshell_terminal::Color::Rgb(r, g, b) => [r, g, b],
        cshell_terminal::Color::Indexed(i) if i < 16 => palette.ansi[usize::from(i)],
        cshell_terminal::Color::Indexed(i) if i < 232 => {
            let n = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            [level(n / 36), level(n / 6 % 6), level(n % 6)]
        }
        cshell_terminal::Color::Indexed(i) => {
            let gray = 8 + 10 * (i - 232);
            [gray; 3]
        }
    }
}
fn draw_replay(
    ui: &mut egui::Ui,
    snapshot: &cshell_terminal::FrameSnapshot,
    palette: TerminalPalette,
) {
    egui::ScrollArea::both().max_height(430.0).show_rows(
        ui,
        18.0,
        usize::from(snapshot.rows),
        |ui, range| {
            for row in range {
                let mut job = egui::text::LayoutJob::default();
                if let Some(cells) = snapshot.row(row as u16) {
                    for (column, cell) in cells.iter().enumerate() {
                        if matches!(
                            cell.width,
                            cshell_terminal::CellWidth::WideSpacer
                                | cshell_terminal::CellWidth::LeadingWideSpacer
                        ) {
                            continue;
                        }
                        let mut text = cell.character.to_string();
                        for c in cell.zerowidth() {
                            text.push(*c);
                        }
                        let mut foreground = replay_color(cell.style.foreground, true, palette);
                        let mut background = replay_color(cell.style.background, false, palette);
                        if cell.style.inverse {
                            std::mem::swap(&mut foreground, &mut background);
                        }
                        if row == usize::from(snapshot.cursor_row)
                            && column == usize::from(snapshot.cursor_col)
                        {
                            background = palette.cursor;
                            foreground = palette.background;
                        }
                        job.append(
                            &text,
                            0.0,
                            egui::TextFormat {
                                font_id: egui::FontId::monospace(13.0),
                                color: egui::Color32::from_rgb(
                                    foreground[0],
                                    foreground[1],
                                    foreground[2],
                                ),
                                background: egui::Color32::from_rgb(
                                    background[0],
                                    background[1],
                                    background[2],
                                ),
                                italics: cell.style.italic,
                                underline: if cell.style.underline {
                                    egui::Stroke::new(
                                        1.0,
                                        egui::Color32::from_rgb(
                                            foreground[0],
                                            foreground[1],
                                            foreground[2],
                                        ),
                                    )
                                } else {
                                    egui::Stroke::NONE
                                },
                                ..Default::default()
                            },
                        );
                    }
                }
                ui.label(job);
            }
        },
    );
}
