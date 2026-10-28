use super::*;
use cshell_domain::{
    HostSelection, QuickCommandAction, QuickCommandCatalog, QuickCommandFolder,
    QuickCommandFolderId, QuickCommandId, QuickCommandRecord, QuickCommandSystem,
    QuickCommandTarget, TaskCommand, TaskDefinition, TaskId,
};
use zeroize::{Zeroize, Zeroizing};
#[path = "compact.rs"]
mod compact;
#[path = "review.rs"]
mod review;

/// Standalone paste uses the same bounded preview without changing command drafts.
#[derive(Default)]
pub(crate) struct PasteReview { editor: ComposePanel, target: Option<SyncInputTarget> }
pub(crate) enum PasteReviewDecision { Stay, Cancel, Send }
impl PasteReview {
    pub(crate) fn is_valid(&self) -> bool {
        self.editor.review_started.is_some_and(|at| at.elapsed() < SYNC_INPUT_REVIEW_LIFETIME)
    }
    pub(crate) fn reset(&mut self) {
        let serial = self.editor.review_serial.wrapping_add(1);
        self.editor = ComposePanel::default();
        self.target = None;
        self.editor.review_serial = serial;
        self.editor.review_started = Some(std::time::Instant::now());
        self.editor.review_focus_requested = true;
    }
    pub(crate) fn show(&mut self, ctx: &egui::Context, window: WorkspaceWindowId, tab: TabId,
        session: cshell_domain::SessionId, title: &str, endpoint: Option<&str>, remote: bool, text: &str) -> PasteReviewDecision {
        if self.target.is_none() {
            self.target = Some(SyncInputTarget { tab_id: tab, session_id: session, title: title.into(), kind: if remote { SyncInputTargetKind::Ssh } else { SyncInputTargetKind::Local } });
            if let Some(endpoint) = endpoint { self.editor.review_endpoints.insert(tab, endpoint.into()); }
        }
        let action = InputAction::Paste { text: text.into(), bracketed: true };
        let risk = SyncInputRisk { multiline: text.contains(['\n', '\r']), control_characters: text.chars().any(char::is_control), destructive_command: false };
        let Some(target) = self.target.as_ref() else {
            return PasteReviewDecision::Cancel;
        };
        match review::show(ctx, window, &mut self.editor, &action, std::slice::from_ref(target), risk, text.len()) {
            review::Decision::Stay => PasteReviewDecision::Stay,
            review::Decision::Edit => PasteReviewDecision::Cancel,
            review::Decision::Send => PasteReviewDecision::Send,
        }
    }
}

#[derive(Default)]
pub(super) struct ComposePanel {
    pub(super) owner: Option<WorkspaceWindowId>,
    pub(super) source: Option<TabId>,
    pub(super) page: u8,
    pub(super) focus_requested: bool,
    pub(super) focus_active: bool,
    text: Zeroizing<String>,
    multiline: bool,
    append_enter: bool,
    ime_active: bool,
    pub(super) armed: bool,
    name: String,
    tags: String,
    folder: Option<QuickCommandFolderId>,
    systems: BTreeSet<QuickCommandSystem>,
    command_id: Option<QuickCommandId>,
    action_kind: u8,
    task_editor: Option<crate::task_definition_editor::TaskDefinitionEditor>,
    selection_json: Zeroizing<String>,
    persist_ack: bool,
    pub(super) risk_ack: bool,
    review_serial: u64,
    review_started: Option<std::time::Instant>,
    review_focus_requested: bool,
    review_endpoints: BTreeMap<TabId, String>,
    delete_ack: bool,
    filter: String,
    history_filter: Zeroizing<String>,
    history_recalled: bool,
    filter_folder: Option<QuickCommandFolderId>,
    filter_system: Option<QuickCommandSystem>,
    folder_id: Option<QuickCommandFolderId>,
    folder_name: String,
    folder_parent: Option<QuickCommandFolderId>,
}

/// Never allow a terminal escape to be rendered as an invisible preview glyph.
pub(crate) fn visible_body(text: &str) -> Zeroizing<String> {
    let mut visible = Zeroizing::new(String::new());
    for ch in text.chars() {
        match ch {
            '\n' => visible.push_str("⟦LF⟧\n"),
            '\r' => visible.push_str("⟦CR⟧"),
            '\t' => visible.push_str("⟦TAB⟧"),
            value
                if value.is_control()
                    || matches!(value, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200d}' | '\u{feff}') =>
            {
                use std::fmt::Write;
                let _written = write!(visible, "⟦U+{:04X}⟧", u32::from(value));
            }
            value => visible.push(value),
        }
    }
    visible
}

impl DesktopSyncInput {
    pub(super) fn begin_paste_review(&mut self) {
        self.compose.risk_ack = false;
        self.compose.review_serial = self.compose.review_serial.wrapping_add(1);
        self.compose.review_started = Some(std::time::Instant::now());
        self.compose.review_focus_requested = true;
        self.compose.review_endpoints = self.pending.as_ref().map(|p| p.snapshot().targets().iter().filter_map(|target| {
            let entry = self.controller.port().targets.get(&target.tab_id)?;
            (entry.opened.session_id == target.session_id).then(|| entry.configured_endpoint.as_ref().map(|endpoint| (target.tab_id, endpoint.clone()))).flatten()
        }).collect()).unwrap_or_default();
    }
    pub(super) fn draw_paste_review(&mut self, context: &egui::Context, window: WorkspaceWindowId) {
        if self.source.and_then(|tab| self.controller.port().targets.get(&tab)).is_none_or(|entry| entry.opened.window_id != window) { return; }
        let Some(prepared) = self.pending.as_ref().filter(|p| matches!(p.action_for_review(), InputAction::Paste { .. })) else { return; };
        match review::show(context, window, &mut self.compose, prepared.action_for_review(), prepared.snapshot().targets(), prepared.risk(), prepared.bytes()) {
            review::Decision::Edit => { self.pending = None; self.return_focus_requested = true; }
            review::Decision::Send => {
                if let Some(prepared) = self.pending.take() { self.dispatch(prepared, SyncInputApproval::Confirmed); }
                self.return_focus_requested = true;
            }
            review::Decision::Stay => {}
        }
    }
    pub(crate) fn edit_clipboard(&mut self, window: WorkspaceWindowId, source: TabId, text: String) -> bool {
        if !self.compose.text.is_empty() {
            self.status = "命令栏已有草稿，请先发送或清空，再载入剪贴板".into();
            if self.compose.owner == Some(window) { self.hidden_command_bars.remove(&window); }
            self.compose.focus_requested = true;
            return false;
        }
        self.open_compose(window, Some(source));
        self.compose.text = Zeroizing::new(text);
        self.compose.multiline = true;
        self.compose.append_enter = false;
        self.compose.armed = false;
        self.status = "剪贴板已载入，尚未发送；目标为当前会话".into();
        true
    }
    pub(super) fn ordinary_command_draft(&self) -> bool {
        self.compose.owner.is_some() && self.compose.page == 0 && self.compose.action_kind < 2
    }
    pub(crate) fn take_task_draft(
        &mut self,
        window: WorkspaceWindowId,
    ) -> Option<(TaskDefinition, HostSelection)> {
        if self
            .task_draft
            .as_ref()
            .is_some_and(|(owner, _, _)| *owner == window)
        {
            self.task_draft
                .take()
                .map(|(_, definition, selection)| (definition, selection))
        } else {
            None
        }
    }
    pub(crate) fn open_compose(&mut self, window: WorkspaceWindowId, source: Option<TabId>) {
        self.hidden_command_bars.remove(&window);
        self.pending = None;
        self.pending_current = None;
        self.arm_review = None;
        self.panel_open = false;
        self.compose = ComposePanel {
            owner: Some(window),
            source,
            focus_requested: true,
            append_enter: true,
            armed: self.command_only && self.is_armed(),
            ..Default::default()
        };
        self.source = source;
    }
    pub(crate) fn compose_open(&self, window: WorkspaceWindowId) -> bool {self.compose.owner == Some(window)}
    pub(crate) fn open_quick_commands(&mut self, window: WorkspaceWindowId, source: Option<TabId>) {
        if !self.compose_open(window) {self.open_compose(window, source);}
        self.compose.page=1;
    }
    pub(crate) fn palette_commands(&self)->Vec<(QuickCommandId,String)>{
        self.quick_commands.as_ref().and_then(|c|c.view().snapshot).map(|s|s.catalog.commands.iter().map(|c|(c.id,c.name.clone())).collect()).unwrap_or_default()
    }
    pub(crate) fn load_history_command(&mut self,window:WorkspaceWindowId,source:Option<TabId>,text:String)->bool {
        if !self.compose.text.is_empty() || self.compose.task_editor.is_some() || source.is_none() || text.len()>4096{return false;}
        if !self.compose_open(window){self.open_compose(window,source);}
        self.compose.text=Zeroizing::new(text);self.compose.multiline=self.compose.text.contains(['\n','\r']);self.compose.page=0;self.compose.focus_requested=true;true
    }
    pub(crate) fn load_palette_command(&mut self,window:WorkspaceWindowId,source:Option<TabId>,id:QuickCommandId)->bool {
        if !self.compose.text.is_empty()||self.compose.task_editor.is_some(){self.status="命令栏已有草稿，请先处理草稿后再载入快捷命令".into();return false;}
        let command=self.quick_commands.as_ref().and_then(|c|c.view().snapshot).and_then(|s|s.catalog.commands.iter().find(|c|c.id==id).cloned());
        let Some(command)=command else{self.status="快捷命令已变化，请刷新后重新选择".into();return false;};
        self.open_compose(window,source);self.load_command(&command);true
    }
    pub(crate) fn take_focus_return(&mut self) -> bool {std::mem::take(&mut self.return_focus_requested)}
    pub(super) fn open_command_targets(&mut self,window:WorkspaceWindowId,active:Option<TabId>){
        if !self.compose_open(window){self.open_compose(window,active);}
        self.compose.page=0;self.compose.armed=true;self.compose.focus_requested=true;
    }
    pub(super) fn compose_uses_targets(&self)->bool {self.compose.armed}
    pub(super) fn compose_can_send(&self)->bool {!self.compose.text.is_empty() && self.compose.text.len()<=MAX_QUICK_COMMAND_BODY_BYTES}
    pub(super) fn prepare_compose_draft(&mut self,source:TabId){self.prepare_compose(source,self.compose.text.to_string(),self.compose.append_enter,self.compose.armed);}
    pub(super) fn finish_command_send(&mut self){self.compose.text.zeroize();self.compose.risk_ack=false;self.compose.focus_requested=true;}
    fn load_command(&mut self, command: &QuickCommandRecord) {
        let draft = &mut self.compose;
        draft.page=0;
        draft.focus_requested=true;
        draft.text.zeroize();
        draft.task_editor = None;
        draft.selection_json.zeroize();
        let (text, enter, multiline, target, kind): (&str, bool, bool, QuickCommandTarget, u8) =
            match &command.action {
                QuickCommandAction::SendText {
                    text,
                    append_enter,
                    target,
                } => (text, *append_enter, text.contains(['\n', '\r']), *target, 0),
                QuickCommandAction::OpenCompose {
                    text,
                    append_enter,
                    multiline,
                    target,
                } => (text, *append_enter, *multiline, *target, 1),
                QuickCommandAction::RunTask {
                    definition,
                    selection,
                }
                | QuickCommandAction::RunScript {
                    definition,
                    selection,
                } => {
                    let Ok(hosts) = serde_json::to_string_pretty(selection) else {
                        self.status = "无法打开主机选择".into();
                        return;
                    };
                    draft.task_editor = Some(
                        crate::task_definition_editor::TaskDefinitionEditor::from_definition(
                            definition,
                        ),
                    );
                    draft.selection_json.zeroize();
                    draft.selection_json.push_str(&hosts);
                    (
                        "",
                        false,
                        false,
                        QuickCommandTarget::Current,
                        if matches!(command.action, QuickCommandAction::RunScript { .. }) {
                            3
                        } else {
                            2
                        },
                    )
                }
                _ => {
                    self.status = "此快捷命令类型尚不可用".into();
                    return;
                }
            };
        draft.text.push_str(text);
        draft.append_enter = enter;
        draft.multiline = multiline;
        draft.armed = target == QuickCommandTarget::Armed;
        draft.name = command.name.clone();
        draft.tags = command.tags.iter().cloned().collect::<Vec<_>>().join(", ");
        draft.folder = command.folder_id;
        draft.systems.clone_from(&command.systems);
        draft.command_id = Some(command.id);
        draft.action_kind = kind;
        draft.persist_ack = false;
        draft.delete_ack = false;
    }
    pub(super) fn prepare_compose(
        &mut self,
        source: TabId,
        text: String,
        append_enter: bool,
        armed: bool,
    ) {
        self.pending = None;
        self.pending_current = None;
        self.compose.risk_ack = false;
        self.compose.review_serial = self.compose.review_serial.wrapping_add(1);
        self.compose.review_started = Some(std::time::Instant::now());
        self.compose.review_focus_requested = true;
        self.compose.review_endpoints.clear();
        if !self.command_only || armed || !self.is_armed(){self.source=Some(source);}
        let action = InputAction::Compose { text, append_enter };
        if armed {
            match self.controller.prepare_input(
                source,
                action,
                SyncInputContext::TerminalCommit,
                false,
            ) {
                Ok(prepared) => self.pending = Some(prepared),
                Err(error) => self.status = error.to_string(),
            }
        } else {
            match self.controller.prepare_current_input(
                source,
                action,
                SyncInputContext::TerminalCommit,
            ) {
                Ok(prepared) => self.pending_current = Some(prepared),
                Err(error) => self.status = error.to_string(),
            }
        }
        let review_targets: Vec<_> = self.pending.as_ref().map(|p| p.snapshot().targets().to_vec())
            .or_else(|| self.pending_current.as_ref().map(|p| vec![p.target().clone()])).unwrap_or_default();
        self.compose.review_endpoints = review_targets.iter().filter_map(|target| {
            let entry = self.controller.port().targets.get(&target.tab_id)?;
            (entry.opened.session_id == target.session_id).then(|| entry.configured_endpoint.as_ref().map(|endpoint| (target.tab_id, endpoint.clone()))).flatten()
        }).collect();
    }
    pub(super) fn dispatch_current(&mut self, prepared: PreparedCurrentInput) -> bool {
        self.dispatch_current_with_approval(prepared,SyncInputApproval::Confirmed)
    }
    pub(super) fn dispatch_current_with_approval(&mut self, prepared: PreparedCurrentInput, approval:SyncInputApproval) -> bool {
        let body = match prepared.action_for_review() {
            InputAction::Compose { text, .. } => Some(Zeroizing::new(text.clone())),
            _ => None,
        };
        match self
            .controller
            .dispatch_current(prepared, approval)
        {
            Ok(receipt) => {
                if receipt.delivery == SyncInputDelivery::QueueAccepted {
                    if let Some(body) = body { self.history.record(&body); }
                    self.status = "已入队 1/1".into();
                } else { self.status = format!("未发送：{:?}", receipt.delivery); }
                if !receipt.audit_complete {
                    self.disarm(SyncInputDisarmReason::AuditFailure);
                    self.status.push_str("；派发审计未完成，已退出同步输入");
                } else if !matches!(
                    receipt.delivery,
                    SyncInputDelivery::QueueAccepted
                        | SyncInputDelivery::Backpressure
                        | SyncInputDelivery::UnsupportedInput
                ) {
                    self.disarm(SyncInputDisarmReason::SourceUnavailable);
                }
                receipt.delivery == SyncInputDelivery::QueueAccepted && receipt.audit_complete
            }
            Err(error) => {
                self.disarm(SyncInputDisarmReason::SourceUnavailable);
                self.status = error.to_string();
                false
            }
        }
    }
    pub(crate) fn close_compose(&mut self) {
        self.return_focus_requested |= self.compose.owner.is_some();
        self.compose = ComposePanel::default();
        self.pending_current = None;
        if self
            .pending
            .as_ref()
            .is_some_and(|review| matches!(review.action_for_review(), InputAction::Compose { .. }))
        {
            self.pending = None;
        }
    }
    pub(super) fn draw_compose(
        &mut self,
        root: &mut egui::Ui,
        window: WorkspaceWindowId,
        active: Option<TabId>,
    ) {
        let context=root.ctx().clone();
        if self.compose.owner != Some(window) {
            return;
        }
        if active != self.compose.source {
            if let Some(tab) = active { self.source_changed(tab); }
            else { self.close_compose(); return; }
        }
        let mut ime_event=false;
        context.input(|input|{for event in &input.events {if let egui::Event::Ime(event)=event {
            ime_event=true;
            #[allow(deprecated)]
            match event {egui::ImeEvent::Preedit{text,..}=>self.compose.ime_active=!text.is_empty(),egui::ImeEvent::Commit(_)|egui::ImeEvent::Disabled=>self.compose.ime_active=false,_=>{}}
        }}});
        if self.pending.is_none() && self.pending_current.is_none() && !self.modal_blocked && (self.compose.focus_active || self.compose.focus_requested || self.compose.page != 0) && !self.panel_open && self.arm_review.is_none() && !egui::Popup::is_any_open(&context) && context.input_mut(|i|i.consume_key(egui::Modifiers::NONE,egui::Key::Escape)) {
            self.hide_command_bar();return;
        }
        let selected = self
            .pending
            .as_ref()
            .filter(|review| matches!(review.action_for_review(), InputAction::Compose { .. }));
        if self.pending_current.is_some() || selected.is_some() {
            let (action, targets, risk, bytes) = if let Some(review) = &self.pending_current {
                (
                    review.action_for_review(),
                    std::slice::from_ref(review.target()),
                    review.risk(),
                    review.bytes(),
                )
            } else if let Some(review) = selected {
                (
                    review.action_for_review(),
                    review.snapshot().targets(),
                    review.risk(),
                    review.bytes(),
                )
            } else {
                return;
            };
            let decision = review::show(&context, window, &mut self.compose, action, targets, risk, bytes);
            match decision {
                review::Decision::Edit => {
                    self.pending = None;
                    self.pending_current = None;
                    self.compose.risk_ack = false;
                    self.compose.review_started = None;
                    self.compose.focus_requested = true;
                }
                review::Decision::Send => {
                    let accepted = if let Some(review) = self.pending_current.take() {
                        self.dispatch_current(review)
                    } else if let Some(review) = self.pending.take() {
                        self.dispatch(review, SyncInputApproval::Confirmed)
                    } else { false };
                    if accepted { self.finish_command_send(); }
                    else { self.compose.focus_requested = true; }
                }
                review::Decision::Stay => {}
            }
            return;
        }
        let view = self
            .quick_commands
            .as_ref()
            .map(|connection| connection.view())
            .unwrap_or_default();
        let empty = QuickCommandCatalog::default();
        let catalog = view
            .snapshot
            .as_ref()
            .map_or(&empty, |snapshot| &snapshot.catalog);
        if self.compose.page == 0 && self.compose.action_kind < 2 {
            self.draw_compact_command_bar(root, window, active, ime_event);
            return;
        }
        let mut open = true;
        let mut preview = false;
        let mut send_requested = false;
        let mut open_task = false;
        let mut load = None;
        let mut edit_library = false;
        let mut run = None;
        let mut change = None;
        let mut reload = false;
        let height=if self.compose.page==0 && self.compose.action_kind<2 && !self.compose.multiline {
            160.0_f32.min(root.available_height()*0.4).max(120.0)
        }else{(root.available_height()*0.38).clamp(120.0,260.0)};
        let choosing_targets=self.modal_blocked || self.panel_open || self.arm_review.is_some();
        egui::Panel::bottom("terminal-compose-dock").exact_size(height)
            .frame(cshell_ui::form::utility_bar()).show(root, |ui| {
                if choosing_targets {ui.disable();}
                ui.horizontal(|ui| {
                    cshell_ui::form::tab(ui,&mut self.compose.page,0,"发送正文");
                    cshell_ui::form::tab(ui,&mut self.compose.page,1,"快捷命令");
                    cshell_ui::form::tab(ui,&mut self.compose.page,2,"管理");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui|{
                        if ui.small_button("收起工具区").on_hover_text("Esc：清除草稿并返回终端").clicked(){open=false;}
                    });
                });
                egui::ScrollArea::vertical().id_salt("compose-dock-body")
                    .max_height((height-68.0).max(40.0)).auto_shrink([false,false]).show(ui,|ui| {
                if self.compose.page==2 || (self.compose.page==0 && self.compose.action_kind>=2) {
                egui::ComboBox::from_id_salt("quick-action-kind")
                    .truncate()
                    .selected_text(
                        ["发送正文", "Compose 模板", "任务模板", "单步脚本模板"]
                            [usize::from(self.compose.action_kind)],
                    )
                    .show_ui(ui, |ui| {
                        cshell_ui::form::menu(ui);
                        for (kind, label) in [
                            (0, "发送正文"),
                            (1, "Compose 模板"),
                            (2, "任务模板"),
                            (3, "单步脚本模板"),
                        ] {
                            if ui
                                .selectable_value(&mut self.compose.action_kind, kind, label)
                                .changed()
                            {
                                self.compose.persist_ack = false;
                            }
                        }
                    });
                    if self.compose.page==2 && ui.button("编辑正文 / 模板…").clicked(){self.compose.page=0;self.compose.focus_requested=true;}
                }
                if self.compose.page==0 {
                if self.compose.action_kind >= 2 {
                    if self.compose.task_editor.is_none() {
                        self.compose.seed_task();
                    }
                    ui.label(
                        "编辑任务定义及主机选择；使用模板后仍需保存计划、核对冻结目标并确认启动。",
                    );
                    if let Some(editor) = &mut self.compose.task_editor {
                        let revision = editor.revision();
                        editor.draw(ui, false, self.compose.action_kind == 3);
                        if editor.revision() != revision {
                            self.compose.persist_ack = false;
                        }
                    }
                    ui.label("主机选择 JSON（selector / group / explicit）");
                    if ui
                        .add(
                            egui::TextEdit::multiline(&mut *self.compose.selection_json)
                                .desired_rows(3)
                                .code_editor()
                                .char_limit(MAX_HOST_SELECTION_BYTES),
                        )
                        .changed()
                    {
                        self.compose.persist_ack = false;
                    }
                    match self.compose.task_template() {
                        Ok(_) => open_task = ui.button("打开任务草稿…").clicked(),
                        Err(error) => {
                            ui.colored_label(cshell_ui::theme::WARNING, error);
                        }
                    }
                } else {
                    let editor = if self.compose.multiline {
                        egui::TextEdit::multiline(&mut *self.compose.text).desired_rows(3)
                    } else {
                        egui::TextEdit::singleline(&mut *self.compose.text)
                    };
                    let enter_shortcut=!self.compose.ime_active && !ime_event && context.input(|i|i.key_pressed(egui::Key::Enter) && !i.modifiers.alt && !i.modifiers.shift && (!self.compose.multiline || i.modifiers.command || i.modifiers.ctrl));
                    let editor=if self.compose.multiline && enter_shortcut {editor.return_key(None)}else{editor};
                    let editor_response=ui.add(
                        editor
                            .font(egui::TextStyle::Monospace)
                            .char_limit(MAX_QUICK_COMMAND_BODY_BYTES)
                            .desired_width(f32::INFINITY),
                    );
                    if self.compose.focus_requested {editor_response.request_focus();self.compose.focus_requested=false;}
                    if (self.compose.ime_active || ime_event) && editor_response.lost_focus(){editor_response.request_focus();}
                    if !choosing_targets && enter_shortcut && (editor_response.has_focus() || editor_response.lost_focus()) {send_requested=true;}
                    editor_response.on_hover_text("单行 Enter 发送；多行 Ctrl+Enter 发送。多行正文、内嵌控制字符和识别出的危险命令仍需确认。草稿仅在本窗口临时保留。");
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut self.compose.multiline,"多行编辑");
                        ui.checkbox(&mut self.compose.append_enter,"追加 Enter（执行命令）");
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(&mut self.compose.armed, false, "仅当前终端");
                        ui.add_enabled_ui(self.is_armed(), |ui| {
                            ui.radio_value(&mut self.compose.armed, true, "已选择的目标");
                        });
                        if ui.small_button("选择目标…").clicked(){self.picker_command_only=true;self.panel_open=true;}
                    });
                    if self.compose.armed && !self.is_armed() {
                        ui.colored_label(
                            cshell_ui::theme::WARNING,
                            "请先选择发送目标",
                        );
                    }
                }
                }
                if self.compose.page==2 {
                ui.collapsing("保存或编辑快捷命令", |ui| {
                    cshell_ui::form::text(ui, "名称", &mut self.compose.name);
                    cshell_ui::form::text(ui, "标签（逗号分隔）", &mut self.compose.tags);
                    folder_picker(
                        ui,
                        "command-folder",
                        &mut self.compose.folder,
                        catalog,
                        "未归类",
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.label("适用系统（留空表示全部）");
                        for (system, label) in [
                            (QuickCommandSystem::Windows, "Windows"),
                            (QuickCommandSystem::Linux, "Linux"),
                            (QuickCommandSystem::Macos, "macOS"),
                        ] {
                            let mut checked = self.compose.systems.contains(&system);
                            if ui.checkbox(&mut checked, label).changed() {
                                if checked {
                                    self.compose.systems.insert(system);
                                } else {
                                    self.compose.systems.remove(&system);
                                }
                            }
                        }
                    });
                    ui.checkbox(
                        &mut self.compose.persist_ack,
                        "确认将正文按普通配置写入磁盘；请勿保存密码或密钥",
                    );
                    if ui
                        .add_enabled(
                            view.writable && !view.busy && self.compose.persist_ack,
                            egui::Button::new(if self.compose.command_id.is_some() {
                                "保存修改"
                            } else {
                                "保存新快捷命令"
                            }),
                        )
                        .clicked()
                    {
                        match self.compose.record() {
                            Ok(record) => change = Some(QuickCommandChange::Upsert(record)),
                            Err(error) => self.status = error,
                        }
                    }
                    if self.compose.command_id.is_some() {
                        if ui.button("另存为新命令").clicked() {
                            self.compose.command_id = None;
                            self.compose.persist_ack = false;
                        }
                        ui.checkbox(&mut self.compose.delete_ack, "确认删除所选快捷命令");
                        if ui
                            .add_enabled(
                                view.writable && !view.busy && self.compose.delete_ack,
                                egui::Button::new("删除所选命令"),
                            )
                            .clicked()
                            && let Some(id) = self.compose.command_id
                        {
                            change = Some(QuickCommandChange::Remove(id));
                        }
                    }
                });
                }
                if self.compose.page==1 {
                egui::CollapsingHeader::new("快捷命令库").default_open(true).show(ui, |ui| {
                    ui.text_edit_singleline(&mut self.compose.filter);
                    folder_picker(
                        ui,
                        "filter-folder",
                        &mut self.compose.filter_folder,
                        catalog,
                        "全部文件夹",
                    );
                    egui::ComboBox::from_id_salt("quick-system-filter")
                        .truncate()
                        .selected_text(format!("系统：{:?}", self.compose.filter_system))
                        .show_ui(ui, |ui| {
                            cshell_ui::form::menu(ui);
                            ui.selectable_value(&mut self.compose.filter_system, None, "全部系统");
                            for system in [
                                QuickCommandSystem::Windows,
                                QuickCommandSystem::Linux,
                                QuickCommandSystem::Macos,
                            ] {
                                ui.selectable_value(
                                    &mut self.compose.filter_system,
                                    Some(system),
                                    format!("{system:?}"),
                                );
                            }
                        });
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .show(ui, |ui| {
                            for command in &catalog.commands {
                                if !self.compose.matches(command) {
                                    continue;
                                }
                                ui.push_id(command.id.to_string(), |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(&command.name).on_hover_text(
                                            command
                                                .tags
                                                .iter()
                                                .cloned()
                                                .collect::<Vec<_>>()
                                                .join(", "),
                                        );
                                        if ui.button("编辑").clicked() {
                                            load = Some(command);
                                            edit_library = true;
                                        }
                                        if ui.button(if matches!(command.action, QuickCommandAction::SendText { .. } | QuickCommandAction::OpenCompose { .. }) { "填入命令栏" } else { "使用模板…" }).clicked() {
                                            if matches!(command.action, QuickCommandAction::SendText { .. } | QuickCommandAction::OpenCompose { .. }) { load = Some(command); }
                                            else { run = Some(command); }
                                        }
                                    });
                                });
                            }
                        });
                });
                }
                if self.compose.page==2 {
                ui.collapsing("管理文件夹", |ui| {
                    if ui.button("新文件夹").clicked() {
                        self.compose.folder_id = None;
                        self.compose.folder_name.clear();
                        self.compose.folder_parent = None;
                    }
                    for folder in &catalog.folders {
                        if ui
                            .selectable_label(
                                self.compose.folder_id == Some(folder.id),
                                &folder.name,
                            )
                            .clicked()
                        {
                            self.compose.folder_id = Some(folder.id);
                            self.compose.folder_name.clone_from(&folder.name);
                            self.compose.folder_parent = folder.parent_id;
                        }
                    }
                    ui.text_edit_singleline(&mut self.compose.folder_name);
                    folder_picker(
                        ui,
                        "folder-parent",
                        &mut self.compose.folder_parent,
                        catalog,
                        "顶层文件夹",
                    );
                    if ui
                        .add_enabled(view.writable && !view.busy, egui::Button::new("保存文件夹"))
                        .clicked()
                    {
                        change = Some(QuickCommandChange::UpsertFolder(QuickCommandFolder {
                            id: self.compose.folder_id.unwrap_or_default(),
                            name: self.compose.folder_name.clone(),
                            parent_id: self.compose.folder_parent,
                        }));
                    }
                    if let Some(id) = self.compose.folder_id {
                        let referenced = catalog
                            .commands
                            .iter()
                            .any(|command| command.folder_id == Some(id))
                            || catalog
                                .folders
                                .iter()
                                .any(|folder| folder.parent_id == Some(id));
                        if ui
                            .add_enabled(
                                view.writable && !view.busy && !referenced,
                                egui::Button::new("删除空文件夹"),
                            )
                            .clicked()
                        {
                            change = Some(QuickCommandChange::RemoveFolder(id));
                        }
                    }
                });
                }
                if self.compose.page!=0 {
                ui.add(egui::Label::new(&view.status).truncate());
                reload = ui
                    .add_enabled(!view.busy, egui::Button::new("重新加载快捷命令库"))
                    .clicked();
                }
                });
                if self.compose.page==0 && self.compose.action_kind<2 {
                    ui.horizontal_wrapped(|ui| {
                        let count=if self.compose.armed {self.controller.armed_snapshot().map_or(0,|s|s.targets().len())}else{usize::from(active.is_some())};
                        ui.label(format!("发送到 {count} 个目标"));
                        send_requested|=ui.add_enabled(active.is_some() && !self.compose.text.is_empty() && self.compose.text.len()<=MAX_QUICK_COMMAND_BODY_BYTES && (!self.compose.armed || self.is_armed()),egui::Button::new(egui::RichText::new(if self.compose.multiline {"发送（Ctrl+Enter）"}else{"发送（Enter）"}).color(cshell_ui::theme::ACCENT)).fill(cshell_ui::theme::ACCENT_FILL)).clicked();
                        preview=ui.add_enabled(active.is_some() && !self.compose.text.is_empty()
                            && self.compose.text.len()<=MAX_QUICK_COMMAND_BODY_BYTES && (!self.compose.armed || self.is_armed()),
                             egui::Button::new("预览…").small()).on_hover_text("可选：发送前查看正文和完整目标清单").clicked();
                    });
                }
            });
        if !open {
            self.close_compose();
            return;
        }
        if let Some(command) = load {
            let scope = self.compose.armed;
            self.load_command(command);
            if edit_library { self.compose.page = 2; }
            else { self.compose.armed = scope; self.history.reset_cursor(); }
        }
        if let Some(command) = run {
            self.load_command(command);
            // Explicit command execution shares the ordinary/risky send policy;
            // OpenCompose only opens an editable draft.
            send_requested = matches!(command.action, QuickCommandAction::SendText { .. });
            open_task = matches!(
                command.action,
                QuickCommandAction::RunTask { .. } | QuickCommandAction::RunScript { .. }
            );
        }
        if reload && let Some(connection) = &self.quick_commands {
            let _queued = connection.reload();
        }
        if let Some(change) = change {
            let saved_id = match &change {
                QuickCommandChange::Upsert(record) => Some(record.id),
                _ => None,
            };
            if apply_quick_command_change(catalog, change.clone()).is_err() {
                self.status = "名称、标签、正文大小或文件夹关系无效，未保存".into();
            } else if self
                .quick_commands
                .as_ref()
                .is_some_and(|connection| connection.change(change))
            {
                if let Some(id) = saved_id {
                    self.compose.command_id = Some(id);
                }
                self.compose.persist_ack = false;
                self.compose.delete_ack = false;
                self.status = "保存请求已入队，等待存储确认".into();
            } else {
                self.status = "存储忙或不可写，未提交修改".into();
            }
        }
        if open_task {
            match self.compose.task_template() {
                Ok((mut definition, selection)) => {
                    definition.id = TaskId::new();
                    self.task_draft = Some((window, definition, selection));
                    self.close_compose();
                    self.status = "任务模板已打开为草稿，尚未保存或启动".into();
                }
                Err(error) => self.status = error,
            }
        } else if (preview || send_requested)
            && self.compose.action_kind < 2
            && !self.compose.text.is_empty()
            && let Some(source) = active
        {
            self.request_compose_send(source,preview);
        }
    }
}
impl ComposePanel {
    fn seed_task(&mut self) {
        self.task_editor = Some(crate::task_definition_editor::TaskDefinitionEditor::default());
        if self.selection_json.is_empty() {
            self.selection_json
                .push_str("{\"kind\":\"selector\",\"expression\":\"all\"}");
        }
    }
    fn task_template(&self) -> Result<(TaskDefinition, HostSelection), String> {
        if self.selection_json.len() > MAX_HOST_SELECTION_BYTES {
            return Err("主机选择超出大小限制".into());
        }
        let definition = self
            .task_editor
            .as_ref()
            .ok_or("缺少任务草稿")?
            .definition(false)?;
        let selection = decode_host_selection(self.selection_json.as_bytes())
            .map_err(|_| "主机选择 JSON 无效")?;
        validate_quick_task_template(&definition, &selection)
            .map_err(|_| "任务定义、变量引用或主机选择无效")?;
        if self.action_kind == 3
            && (definition.steps.len() != 1
                || !matches!(definition.steps[0].command, TaskCommand::Script { .. }))
        {
            return Err("单步脚本模板必须包含且仅包含一个 Script 步骤".into());
        }
        Ok((definition, selection))
    }
    fn record(&self) -> Result<QuickCommandRecord, String> {
        let target = if self.armed {
            QuickCommandTarget::Armed
        } else {
            QuickCommandTarget::Current
        };
        let action = if self.action_kind >= 2 {
            let (definition, selection) = self.task_template()?;
            if self.action_kind == 3 {
                QuickCommandAction::RunScript {
                    definition,
                    selection,
                }
            } else {
                QuickCommandAction::RunTask {
                    definition,
                    selection,
                }
            }
        } else if self.action_kind == 1 {
            QuickCommandAction::OpenCompose {
                text: self.text.to_string(),
                append_enter: self.append_enter,
                multiline: self.multiline,
                target,
            }
        } else {
            QuickCommandAction::SendText {
                text: self.text.to_string(),
                append_enter: self.append_enter,
                target,
            }
        };
        Ok(QuickCommandRecord {
            id: self.command_id.unwrap_or_default(),
            name: self.name.clone(),
            folder_id: self.folder,
            tags: self
                .tags
                .split(',')
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_owned)
                .collect(),
            systems: self.systems.clone(),
            action,
        })
    }
    fn matches(&self, command: &QuickCommandRecord) -> bool {
        self.filter_folder
            .is_none_or(|id| command.folder_id == Some(id))
            && self.filter_system.is_none_or(|system| {
                command.systems.is_empty() || command.systems.contains(&system)
            })
            && (self.filter.is_empty()
                || command.name.contains(&self.filter)
                || command.tags.iter().any(|tag| tag.contains(&self.filter)))
    }
}
fn folder_picker(
    ui: &mut egui::Ui,
    id: &str,
    selected: &mut Option<QuickCommandFolderId>,
    catalog: &QuickCommandCatalog,
    empty: &str,
) {
    egui::ComboBox::from_id_salt(id)
        .truncate()
        .selected_text(
            selected
                .and_then(|id| catalog.folders.iter().find(|folder| folder.id == id))
                .map_or(empty, |folder| folder.name.as_str()),
        )
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(selected, None, empty);
            for folder in &catalog.folders {
                ui.selectable_value(selected, Some(folder.id), &folder.name);
            }
        });
}

#[cfg(test)]
#[path = "compose_command_tests.rs"]
mod command_tests;

#[cfg(test)]
#[path = "compose_dock_tests.rs"]
mod dock_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_load_keeps_a_draft_and_never_submits_or_overwrites() {
        let mut sync=DesktopSyncInput::with_audit(DesktopSyncAudit::unavailable());
        let window=WorkspaceWindowId::new();let source=Some(TabId::new());
        assert!(!sync.load_history_command(window,None,"echo nope".into()));
        assert!(sync.load_history_command(window,source,"echo first\necho second".into()));
        assert_eq!(sync.compose.text.as_str(),"echo first\necho second");assert!(sync.compose.multiline);
        assert!(sync.pending.is_none()&&sync.pending_current.is_none()&&!sync.is_armed());
        assert!(!sync.load_history_command(window,source,"replacement".into()));
        assert_eq!(sync.compose.text.as_str(),"echo first\necho second");
    }
    #[test]
    fn preview_shows_newlines_escape_bidi_and_template_body_is_not_in_debug() {
        assert_eq!(
            visible_body("a\n\r\t\x1b\u{202e}").as_str(),
            "a⟦LF⟧\n⟦CR⟧⟦TAB⟧⟦U+001B⟧⟦U+202E⟧"
        );
        let draft = ComposePanel {
            text: Zeroizing::new("PRIVATE_DRAFT".into()),
            name: "health".into(),
            ..Default::default()
        };
        let record = draft
            .record()
            .unwrap_or_else(|error| panic!("record: {error}"));
        assert!(!format!("{record:?}").contains("PRIVATE_DRAFT"));
        assert!(matches!(
            record.action,
            QuickCommandAction::SendText {
                target: QuickCommandTarget::Current,
                ..
            }
        ));
    }
    #[test]
    fn command_draft_survives_focus_and_tab_switch_but_reviews_are_invalidated() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("temp: {error}"));
        let audit = DesktopSyncAudit::start(
            &cshell_ipc::RuntimePaths::prepare(directory.path().to_path_buf())
                .unwrap_or_else(|error| panic!("runtime: {error}")),
        )
        .unwrap_or_else(|error| panic!("audit: {error}"));
        let mut sync = DesktopSyncInput::with_audit(audit);
        let window = WorkspaceWindowId::new();
        let tab = TabId::new();
        sync.open_compose(window, Some(tab));
        sync.compose.text.push_str("PRIVATE_DRAFT");
        assert!(sync.blocks_input());
        let context = egui::Context::default();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1024.0, 768.0),
                )),
                ..Default::default()
            },
            |ui| sync.draw_compose(ui, window, Some(tab)),
        );
        assert!(!output.shapes.is_empty());
        output.textures_delta.clear();
        sync.focus_lost(Some(tab));
        assert_eq!(sync.compose.text.as_str(), "PRIVATE_DRAFT");
        assert_eq!(sync.compose.owner, Some(window));
        sync.open_compose(window, Some(tab));
        sync.compose.text.push_str("PRIVATE_DRAFT_2");
        sync.source_changed(TabId::new());
        assert_eq!(sync.compose.text.as_str(), "PRIVATE_DRAFT_2");
        assert!(sync.pending_current.is_none());
    }
}
