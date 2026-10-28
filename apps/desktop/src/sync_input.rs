//! Desktop adapter for explicit synchronized input and metadata-only auditing.

use crate::daemon_input::{DesktopInputPermit, DesktopInputSecurity, DesktopInputSender};
use cshell_application::*;
use cshell_domain::{ConnectionId, InputAction, TabId, WorkspaceWindowId};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::UNIX_EPOCH,
};
pub(crate) mod compose;
mod command_bar;
mod history;
mod lifecycle;

pub(crate) struct DesktopSyncTarget {
    pub(crate) opened: OpenedSyncInputTarget,
    pub(crate) sender: Option<DesktopInputSender>,
    pub(crate) control_error: Option<String>,
    pub(crate) configured_endpoint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DesktopSyncViewStamp {
    snapshot: Option<cshell_domain::SyncInputSnapshotId>,
    blocked: bool,
    secure: bool,
    status: String,
    targets: Vec<(
        TabId,
        cshell_domain::SessionId,
        SyncInputEligibility,
        Option<String>,
    )>,
}

pub(crate) struct DesktopSyncInputPort {
    targets: BTreeMap<TabId, DesktopSyncTarget>,
    permit: DesktopInputPermit,
}

impl Default for DesktopSyncInputPort {
    fn default() -> Self {
        Self {
            targets: BTreeMap::new(),
            permit: DesktopInputPermit::new(),
        }
    }
}

impl SyncInputPort for DesktopSyncInputPort {
    fn opened_target(&self, tab: TabId) -> Option<OpenedSyncInputTarget> {
        let target = self.targets.get(&tab)?;
        let mut opened = target.opened.clone();
        opened.eligibility = target
            .sender
            .as_ref()
            .map_or(SyncInputEligibility::Disconnected, |sender| {
                sender.eligibility(opened.session_id)
            });
        Some(opened)
    }

    fn try_submit(
        &mut self,
        source: &SyncInputTarget,
        expected: &SyncInputTarget,
        action: &InputAction,
    ) -> SyncInputDelivery {
        let Some(source_entry) = self.targets.get(&source.tab_id) else {
            return SyncInputDelivery::SourceBlocked;
        };
        if source_entry.opened.session_id != source.session_id
            || source_entry.opened.kind != source.kind
        {
            return SyncInputDelivery::SourceBlocked;
        }
        let Some(source_sender) = &source_entry.sender else {
            return SyncInputDelivery::SourceBlocked;
        };
        let Some(target) = self.targets.get(&expected.tab_id) else {
            return SyncInputDelivery::TargetChanged;
        };
        if target.opened.session_id != expected.session_id || target.opened.kind != expected.kind {
            return SyncInputDelivery::TargetChanged;
        }
        let Some(sender) = &target.sender else {
            return SyncInputDelivery::Disconnected;
        };
        sender.try_submit(
            source_sender,
            source.session_id,
            expected.session_id,
            action,
            &self.permit,
        )
    }
}

pub(crate) struct DesktopSyncAudit {
    sender: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    healthy: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DesktopSyncAudit {
    pub(crate) fn start(paths: &cshell_ipc::RuntimePaths) -> std::io::Result<Self> {
        let paths = cshell_ipc::RuntimePaths::prepare(paths.root().join("sync-input-audit"))
            .map_err(std::io::Error::other)?;
        let path = paths.root().join(format!("{}.jsonl", ConnectionId::new()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
        let healthy = Arc::new(AtomicBool::new(true));
        let worker_health = Arc::clone(&healthy);
        let worker = std::thread::Builder::new()
            .name("cshell-sync-audit".into())
            .spawn(move || {
                let mut bytes = 0_u64;
                while let Ok(line) = receiver.recv() {
                    bytes = bytes.saturating_add(line.len() as u64);
                    // A single attachment cannot grow its audit file without bound.
                    if bytes > 64 * 1024 * 1024
                        || file
                            .write_all(&line)
                            .and_then(|()| file.sync_data())
                            .is_err()
                    {
                        worker_health.store(false, Ordering::Release);
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            healthy,
            worker: Some(worker),
        })
    }

    fn unavailable() -> Self {
        Self {
            sender: None,
            healthy: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl SyncInputAuditSink for DesktopSyncAudit {
    fn record(&mut self, event: SyncInputAuditEvent) -> Result<(), SyncInputAuditError> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err(SyncInputAuditError);
        }
        let operation = match event.operation {
            SyncInputAuditOperation::Armed => serde_json::json!({"operation":"armed"}),
            SyncInputAuditOperation::Disarmed(reason) => {
                serde_json::json!({"operation":"disarmed", "reason":format!("{reason:?}")})
            }
            SyncInputAuditOperation::InputPlanned {
                send_id,
                kind,
                bytes,
            } => {
                serde_json::json!({"operation":"input_planned", "send_id":send_id, "kind":format!("{kind:?}"), "bytes":bytes})
            }
            SyncInputAuditOperation::InputDispatched {
                send_id,
                deliveries,
            } => {
                serde_json::json!({"operation":"input_dispatched", "send_id":send_id, "deliveries":deliveries.iter().map(|result| serde_json::json!({"tab_id":result.tab_id, "session_id":result.session_id, "delivery":format!("{:?}",result.delivery)})).collect::<Vec<_>>()})
            }
        };
        let value = serde_json::json!({
            "schema":1, "operator":event.operator,
            "unix_ms":event.time.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u128::from(u64::MAX)) as u64,
            "group_id":event.group_id, "snapshot_id":event.snapshot_id,
            "selection":event.selection.map(|selection| format!("{selection:?}")),
            "targets":event.targets, "event":operation,
        });
        let mut line = serde_json::to_vec(&value).map_err(|_| SyncInputAuditError)?;
        line.push(b'\n');
        self.sender
            .as_ref()
            .ok_or(SyncInputAuditError)?
            .try_send(line)
            .map_err(|_| SyncInputAuditError)
    }
}

impl Drop for DesktopSyncAudit {
    fn drop(&mut self) {
        self.sender = None;
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}

pub(crate) struct DesktopSyncInput {
    controller: SyncInputController<DesktopSyncInputPort, DesktopSyncAudit>,
    pub(crate) selected: BTreeSet<TabId>,
    secure: BTreeSet<TabId>,
    pub(crate) panel_open: bool,
    selection: Option<SyncInputSelection>,
    arm_review: Option<SyncInputArmReview>,
    // Command targets are independent of live terminal keystrokes.
    command_only: bool,
    picker_command_only: bool,
    target_filter: String,
    history: history::CommandHistory,
    hidden_command_bars: BTreeSet<WorkspaceWindowId>,
    modal_blocked: bool,
    search_requested: bool,
    pending: Option<PreparedSyncInput>,
    pending_current: Option<PreparedCurrentInput>,
    compose: compose::ComposePanel,
    return_focus_requested: bool,
    task_draft: Option<(
        WorkspaceWindowId,
        cshell_domain::TaskDefinition,
        cshell_domain::HostSelection,
    )>,
    quick_commands: Option<crate::quick_command_connection::QuickCommandConnection>,
    source: Option<TabId>,
    pub(crate) status: String,
}

impl DesktopSyncInput {
    pub(crate) fn start(config: crate::daemon_connection::DesktopConnectionConfig) -> Self {
        let audit = cshell_ipc::RuntimePaths::for_current_user()
            .ok()
            .and_then(|paths| DesktopSyncAudit::start(&paths).ok());
        let mut input = Self::with_audit(audit.unwrap_or_else(DesktopSyncAudit::unavailable));
        input.quick_commands =
            crate::quick_command_connection::QuickCommandConnection::start(config).ok();
        input
    }

    fn with_audit(audit: DesktopSyncAudit) -> Self {
        let status = if audit.healthy.load(Ordering::Acquire) {
            String::new()
        } else {
            "同步输入审计不可用，无法 Arm".into()
        };
        let port = DesktopSyncInputPort {
            permit: DesktopInputPermit::with_audit_health(Arc::clone(&audit.healthy)),
            ..DesktopSyncInputPort::default()
        };
        Self {
            controller: SyncInputController::new(ConnectionId::new(), port, audit),
            selected: BTreeSet::new(),
            secure: BTreeSet::new(),
            panel_open: false,
            selection: None,
            arm_review: None,
            command_only: false,
            picker_command_only: true,
            target_filter: String::new(),
            history: history::CommandHistory::default(),
            hidden_command_bars: BTreeSet::new(),
            modal_blocked: false,
            search_requested: false,
            pending: None,
            pending_current: None,
            compose: compose::ComposePanel::default(),
            return_focus_requested: false,
            task_draft: None,
            quick_commands: None,
            source: None,
            status,
        }
    }

    pub(crate) fn replace_window(
        &mut self,
        window: WorkspaceWindowId,
        targets: Vec<DesktopSyncTarget>,
    ) {
        self.controller
            .port_mut()
            .targets
            .retain(|_, target| target.opened.window_id != window);
        for target in targets {
            if let Some(sender) = &target.sender {
                sender.set_security(if self.secure.contains(&target.opened.tab_id) {
                    DesktopInputSecurity::Secure
                } else {
                    DesktopInputSecurity::Terminal
                });
            }
            self.controller
                .port_mut()
                .targets
                .insert(target.opened.tab_id, target);
        }
        if !self.controller.audit().healthy.load(Ordering::Acquire) && self.is_armed() {
            self.disarm(SyncInputDisarmReason::AuditFailure);
            self.status = "审计写入失败，同步输入已退出；之前的入队结果不代表执行成功".into();
        }
        if self.source.is_some_and(|tab| {
            self.controller
                .port()
                .opened_target(tab)
                .is_none_or(|target| {
                    target.eligibility != SyncInputEligibility::Ready
                        || self.controller.armed_snapshot().is_some_and(|snapshot| {
                            snapshot
                                .targets()
                                .iter()
                                .find(|frozen| frozen.tab_id == tab)
                                .is_none_or(|frozen| {
                                    frozen.session_id != target.session_id
                                        || frozen.kind != target.kind
                                })
                        })
                        || self.pending_current.as_ref().is_some_and(|review| {
                            review.target().tab_id == tab
                                && (review.target().session_id != target.session_id
                                    || review.target().kind != target.kind)
                        })
                })
        }) {
            self.disarm(SyncInputDisarmReason::SourceUnavailable);
        }
    }

    pub(crate) fn remove_window(&mut self, window: WorkspaceWindowId) {
        self.hidden_command_bars.remove(&window);
        if self.compose.owner == Some(window)
            || self.source.is_some_and(|tab| {
                self.controller
                    .port()
                    .targets
                    .get(&tab)
                    .is_some_and(|entry| entry.opened.window_id == window)
            })
        {
            self.disarm(SyncInputDisarmReason::SourceUnavailable);
        }
        if self.controller.armed_snapshot().is_some_and(|snapshot| {
            snapshot.targets().iter().any(|target| {
                self.controller
                    .port()
                    .targets
                    .get(&target.tab_id)
                    .is_some_and(|entry| entry.opened.window_id == window)
            })
        }) {
            self.disarm(SyncInputDisarmReason::SourceUnavailable);
        }
        self.controller
            .port_mut()
            .targets
            .retain(|_, target| target.opened.window_id != window);
        if self.compose.owner == Some(window) { self.close_compose(); }
    }

    pub(crate) fn is_armed(&self) -> bool {
        self.controller.armed_snapshot().is_some()
    }
    pub(crate) fn is_realtime(&self)->bool {self.is_armed() && !self.command_only}
    pub(crate) fn view_stamp(&self, active: Option<TabId>) -> DesktopSyncViewStamp {
        DesktopSyncViewStamp {
            snapshot: self.controller.armed_snapshot().map(SyncInputSnapshot::id),
            blocked: self.blocks_input(),
            secure: active.is_some_and(|tab| self.secure.contains(&tab)),
            status: format!(
                "{}|{:?}",
                self.status,
                self.quick_commands.as_ref().map(|connection| {
                    let view = connection.view();
                    (
                        view.snapshot.as_ref().map(|snapshot| snapshot.revision),
                        view.writable,
                        view.busy,
                        view.status,
                    )
                })
            ),
            targets: self
                .controller
                .port()
                .targets
                .iter()
                .filter_map(|(tab, entry)| {
                    self.controller.port().opened_target(*tab).map(|target| {
                        (
                            *tab,
                            target.session_id,
                            target.eligibility,
                            entry.control_error.clone(),
                        )
                    })
                })
                .collect(),
        }
    }
    pub(crate) fn blocks_input(&self) -> bool {
        self.modal_blocked || self.panel_open
            || self.arm_review.is_some()
            || self.pending.is_some()
            || self.pending_current.is_some()
            || self.compose.owner.is_some() && (self.compose.focus_active || self.compose.focus_requested || self.compose.page != 0)
    }
    pub(crate) fn targeted(&self, tab: TabId) -> bool {
        self.controller
            .armed_snapshot()
            .is_some_and(|snapshot| snapshot.targets().iter().any(|target| target.tab_id == tab))
    }
    pub(crate) fn target_badges(&self) -> BTreeMap<TabId, bool> {
        if !self.is_realtime() && self.ordinary_command_draft() && !self.compose_uses_targets() {
            return self.compose.source.into_iter().map(|tab| (tab, false)).collect();
        }
        self.controller.armed_snapshot().map(|snapshot| snapshot.targets().iter()
            .map(|target| (target.tab_id, self.is_realtime())).collect()).unwrap_or_default()
    }

    pub(crate) fn disarm(&mut self, reason: SyncInputDisarmReason) {
        let editor = if self.ordinary_command_draft() && reason != SyncInputDisarmReason::Shutdown {
            Some(std::mem::take(&mut self.compose))
        } else { None };
        self.controller.port().permit.revoke();
        self.pending = None;
        self.pending_current = None;
        self.compose = compose::ComposePanel::default();
        self.task_draft = None;
        self.arm_review = None;
        self.source = None;
        if let Err(error) = self.controller.disarm(reason) {
            self.status = error.to_string();
        }
        self.controller.port_mut().permit =
            DesktopInputPermit::with_audit_health(Arc::clone(&self.controller.audit().healthy));
        if let Some(mut editor) = editor {
            editor.risk_ack = false;
            if reason == SyncInputDisarmReason::User { editor.armed = false; }
            self.compose = editor;
        }
    }

    pub(crate) fn source_changed(&mut self, tab: TabId) {
        let mut draft = if !self.is_realtime() && self.ordinary_command_draft() {
            Some(std::mem::take(&mut self.compose))
        } else { None };
        self.controller.port().permit.revoke();
        self.controller.port_mut().permit =
            DesktopInputPermit::with_audit_health(Arc::clone(&self.controller.audit().healthy));
        self.pending = None;
        self.pending_current = None;
        self.compose = compose::ComposePanel::default();
        self.task_draft = None;
        self.arm_review = None;
        if let Some(mut saved) = draft.take() {
            saved.source = Some(tab);
            saved.risk_ack = false;
            saved.focus_requested = false;
            saved.focus_active = false;
            self.compose = saved;
        }
        if self.is_armed() && self.command_only {
            self.source=self.controller.armed_snapshot().and_then(|s|s.targets().first()).map(|t|t.tab_id);
        } else if self.is_armed() && !self.targeted(tab) {
            self.disarm(SyncInputDisarmReason::SourceUnavailable);
        } else {
            self.source = Some(tab);
        }
    }

    pub(crate) fn focus_lost(&mut self, tab: Option<TabId>) {
        // Command entry has no background keystroke route. Revoke queued work and
        // reviews, but keep the editor and frozen scope for the next explicit send.
        if !self.is_realtime() && (self.ordinary_command_draft() || self.command_only && self.is_armed()) {
            self.controller.port().permit.revoke();
            self.controller.port_mut().permit = DesktopInputPermit::with_audit_health(Arc::clone(&self.controller.audit().healthy));
            self.pending = None;
            self.pending_current = None;
            self.arm_review = None;
            self.panel_open = false;
            self.compose.risk_ack = false;
            self.compose.focus_requested = true;
            return;
        }
        self.disarm(SyncInputDisarmReason::FocusLost);
        if let Some(sender) = tab
            .and_then(|tab| self.controller.port().targets.get(&tab))
            .and_then(|entry| entry.sender.as_ref())
        {
            sender.set_security(DesktopInputSecurity::Locked);
        }
    }

    /// True means this route consumed the action, including a rejection or review.
    pub(crate) fn route(&mut self, tab: TabId, action: InputAction) -> bool {
        if !self.is_realtime() {
            return false;
        }
        if self.blocks_input() {
            return true;
        }
        self.source = Some(tab);
        match self
            .controller
            .prepare_input(tab, action, SyncInputContext::TerminalCommit, false)
        {
            Ok(prepared) if prepared.risk().requires_confirmation() => {
                self.pending = Some(prepared);
                self.begin_paste_review();
            }
            Ok(prepared) => { self.dispatch(prepared, SyncInputApproval::Explicit); },
            Err(error) => {
                self.disarm(SyncInputDisarmReason::SourceUnavailable);
                self.status = error.to_string();
            }
        }
        true
    }

    fn dispatch(&mut self, prepared: PreparedSyncInput, approval: SyncInputApproval) -> bool {
        let history_body = match prepared.action_for_review() {
            InputAction::Compose { text, .. } => Some(zeroize::Zeroizing::new(text.clone())),
            _ => None,
        };
        match self.controller.dispatch(prepared, approval) {
            Ok(receipt) => {
                let accepted = receipt
                    .deliveries
                    .iter()
                    .filter(|result| result.delivery == SyncInputDelivery::QueueAccepted)
                    .count();
                if accepted > 0 && let Some(body) = history_body { self.history.record(&body); }
                let failures: Vec<_> = receipt
                    .deliveries
                    .iter()
                    .filter(|result| result.delivery != SyncInputDelivery::QueueAccepted)
                    .map(|result| {
                        format!(
                            "{}: {:?}",
                            receipt
                                .snapshot
                                .targets()
                                .iter()
                                .find(|target| target.tab_id == result.tab_id)
                                .map_or("目标", |target| target.title.as_str()),
                            result.delivery
                        )
                    })
                    .collect();
                self.status = format!(
                    "已入队 {accepted}/{}{}",
                    receipt.deliveries.len(),
                    if failures.is_empty() {
                        String::new()
                    } else {
                        format!("；{}", failures.join("；"))
                    }
                );
                if !receipt.audit_complete {
                    self.disarm(SyncInputDisarmReason::AuditFailure);
                    self.status.push_str("；派发审计未完成，同步输入已退出");
                } else if !self.is_armed() {
                    self.disarm(SyncInputDisarmReason::SourceUnavailable);
                }
                accepted > 0 && receipt.audit_complete
            }
            Err(error) => {
                self.disarm(SyncInputDisarmReason::SourceUnavailable);
                self.status = error.to_string();
                false
            }
        }
    }

    pub(crate) fn draw(
        &mut self,
        root: &mut egui::Ui,
        window: WorkspaceWindowId,
        group: cshell_domain::TabGroupId,
        active: Option<TabId>,
        search_open: bool,
    ) -> bool {
        if !search_open { self.ensure_command_bar(window, active); }
        let mut search_requested=false;
        let context = root.ctx().clone();
        if !self.compose_open(window) || self.compose.page != 0 || self.is_realtime() {
        egui::Panel::bottom("synchronized-input-control")
            .exact_size(30.0)
            .frame(
                cshell_ui::form::utility_bar(),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    if self.is_armed() && (!self.command_only || !self.compose_open(window)) {
                        if let Some(snapshot)=self.controller.armed_snapshot() {
                            ui.colored_label(cshell_ui::theme::WARNING,egui::RichText::new(format!("{} {} 个目标",if self.command_only{"命令发送"}else{"同步"},snapshot.targets().len())).strong())
                                .on_hover_text(snapshot.targets().iter().map(|t|t.title.as_str()).collect::<Vec<_>>().join("\n"));
                        }
                        if ui.button(egui::RichText::new("停止同步").color(cshell_ui::theme::WARNING))
                             .on_hover_text("立即停止多目标发送 · Ctrl+Shift+Esc").clicked(){self.disarm(SyncInputDisarmReason::User);self.return_focus_requested=true;}
                         if ui.small_button("更改目标").clicked(){self.picker_command_only=self.command_only;self.panel_open=true;}
                    }else if !self.is_armed() && ui.button("键盘同步…").clicked(){self.picker_command_only=false;self.panel_open=true;}
                    ui.separator();
                    search_requested=ui.selectable_label(search_open,"查找").on_hover_text("Ctrl+Shift+F").clicked();
                    if ui.selectable_label(self.compose_open(window) && self.compose.page==0,"命令栏").clicked(){
                         if search_open {search_requested=true;self.compose.focus_requested=true;}
                         else if self.compose_open(window) && self.compose.page==0 {self.hide_command_bar();}
                        else if self.compose_open(window){self.compose.page=0;self.compose.focus_requested=true;}
                        else {self.open_compose(window,active);}
                    }
                    if ui.selectable_label(self.compose_open(window) && self.compose.page==1,"快捷命令").clicked(){
                        if self.compose_open(window) && self.compose.page==1 {self.close_compose();}else{self.open_quick_commands(window,active);}
                    }
                    if let Some(tab) = active {
                        let mut secure = self.secure.contains(&tab);
                        if ui.checkbox(&mut secure, "安全输入").changed() {
                            self.disarm(SyncInputDisarmReason::SecureInput);
                            if secure {
                                self.secure.insert(tab);
                            } else {
                                self.secure.remove(&tab);
                            }
                            if let Some(sender) = self
                                .controller
                                .port()
                                .targets
                                .get(&tab)
                                .and_then(|entry| entry.sender.as_ref())
                            {
                                sender.set_security(if secure {
                                    DesktopInputSecurity::Secure
                                } else {
                                    DesktopInputSecurity::Terminal
                                });
                            }
                        }
                    }
                    if !self.status.is_empty() {
                        let brief: String = self.status.chars().take(36).collect();
                        ui.add(egui::Label::new(egui::RichText::new(brief).small()).truncate()).on_hover_text(format!("{}\n入队仅代表已交给连接队列，不表示远端执行成功。",self.status));
                    }
                    let failures: Vec<_> = self
                        .controller
                        .port()
                        .targets
                        .values()
                        .filter_map(|entry| {
                            entry
                                .control_error
                                .as_ref()
                                .map(|error| format!("{}: {error}", entry.opened.title))
                        })
                        .collect();
                    if !failures.is_empty() {
                        ui.colored_label(
                            cshell_ui::theme::WARNING,
                            format!("{} 个终端输入被拒绝", failures.len()),
                        )
                        .on_hover_text(failures.join("\n"));
                    }
                });
            });
        }
        if search_requested {self.compose.focus_active=false;self.compose.focus_requested=false;}
        // A newly opened large-target review must not consume the Enable click.
        let arm_review_was_open=self.arm_review.is_some();
        if !search_open && !search_requested {self.draw_compose(root, window, active);}
        search_requested |= std::mem::take(&mut self.search_requested);
        if search_requested { self.close_compose(); }
        if self.panel_open {
            let targets=&self.controller.port().targets;
            self.selected.retain(|tab|targets.get(tab).is_some_and(|t|t.opened.window_id==window));
            let mut open = true;
            cshell_ui::form::dialog(&context, "选择已打开的同步目标").default_width(600.0).open(&mut open).collapsible(false).show(&context, |ui| {
                ui.horizontal_wrapped(|ui|{
                    ui.radio_value(&mut self.picker_command_only,true,"命令栏发送（推荐）");
                    ui.radio_value(&mut self.picker_command_only,false,"实时键盘同步");
                });
                ui.weak(if self.picker_command_only {"选定一次目标，普通单行命令按 Enter 连续发送；新标签不会自动加入。"}else{"终端内逐键同步到固定目标；密码提示前请开启安全输入。"});
                ui.horizontal_wrapped(|ui| {
                    if ui.button("清空选择").clicked() { self.selected.clear(); self.selection = Some(SyncInputSelection::Manual); }
                    for (label, selection) in [("当前窗口 SSH", SyncInputSelection::CurrentWindow(window)), ("当前标签组 SSH", SyncInputSelection::CurrentTabGroup(group))] {
                        if ui.button(label).clicked() {
                            self.selection = Some(selection);
                            self.selected = self.controller.port().targets.iter().filter(|(tab, entry)| self.controller.port().opened_target(**tab).is_some_and(|t|t.eligibility==SyncInputEligibility::Ready) && entry.opened.window_id==window && entry.opened.kind == SyncInputTargetKind::Ssh && match selection { SyncInputSelection::CurrentWindow(id) => entry.opened.window_id == id, SyncInputSelection::CurrentTabGroup(id) => entry.opened.group_id == id, SyncInputSelection::Manual => true }).map(|(tab,_)| *tab).collect();
                        }
                    }
                });
                egui::ScrollArea::vertical().max_height((context.content_rect().height()-310.0).clamp(60.0,240.0)).show(ui, |ui| {
                    for (tab, entry) in &self.controller.port().targets {
                        if entry.opened.window_id!=window {continue;}
                        let mut selected = self.selected.contains(tab);
                        let status = self.controller.port().opened_target(*tab).map(|target| target.eligibility);
                        let ready=status==Some(SyncInputEligibility::Ready);
                        let label=format!("{} · {} · {}",entry.opened.title,if entry.opened.kind==SyncInputTargetKind::Ssh {"SSH"}else{"本地终端"},if ready {"已连接"}else{"暂不可用"});
                        if ui.add_enabled(ready || selected,egui::Checkbox::new(&mut selected,label)).changed() {
                            self.selection = Some(SyncInputSelection::Manual);
                            if selected { self.selected.insert(*tab); } else { self.selected.remove(tab); }
                        }
                    }
                });
                ui.weak("本地终端需手选，不与 SSH 混选；超过 10 个目标时再确认一次。");
                if ui.add_enabled(!self.selected.is_empty(),egui::Button::new(format!("启用 {} 个目标",self.selected.len()))).clicked() {
                    self.choose_targets(window,active);
                }
                if !self.status.is_empty(){ui.add(egui::Label::new(&self.status).truncate()).on_hover_text(&self.status);}
            });
            if !open || context.input_mut(|i|i.consume_key(egui::Modifiers::NONE,egui::Key::Escape)) {
                self.panel_open = false;
                self.compose.focus_requested=true;
            }
        }
        if arm_review_was_open && let Some(review) = &self.arm_review {
            let mut confirm = false;
            let mut cancel = false;
            cshell_ui::form::dialog(&context, "确认启用同步输入")
                .collapsible(false)
                .show(&context, |ui| {
                    ui.label(format!(
                        "固定 {} 个目标，之后打开的标签不会加入：",
                        review.targets().len()
                    ));
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for target in review.targets() {
                                ui.label(format!("{} · {}", target.title, target.session_id));
                            }
                        });
                    if review.requires_confirmation() {
                        ui.colored_label(
                            cshell_ui::theme::WARNING,
                            "超过 10 个目标；此按钮是额外的第二次确认。",
                        );
                    }
                    confirm = ui.button("确认启用这些目标").clicked();
                    cancel = ui.button("取消").clicked();
                });
            if cancel || context.input_mut(|i|i.consume_key(egui::Modifiers::NONE,egui::Key::Escape)) {
                self.arm_review = None;
                self.compose.focus_requested=true;
            } else if confirm && let Some(review) = self.arm_review.take() {
                self.enable_targets(review,window,active);
            }
        }
        self.draw_paste_review(&context, window);
        if let Some(prepared) = self
            .pending
            .as_ref()
            .filter(|review| !matches!(review.action_for_review(), InputAction::Compose { .. } | InputAction::Paste { .. }))
        {
            let mut confirm = false;
            let mut cancel = false;
            cshell_ui::form::dialog(&context, "确认同步输入正文与目标")
                .collapsible(false)
                .show(&context, |ui| {
                    ui.label(format!(
                        "风险：{:?}；{} 字节；{} 个固定目标",
                        prepared.risk(),
                        prepared.bytes(),
                        prepared.snapshot().targets().len()
                    ));
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            match prepared.action_for_review() {
                                InputAction::Text(text)
                                | InputAction::Paste { text, .. }
                                | InputAction::Compose { text, .. } => {
                                    ui.monospace(compose::visible_body(text).as_str());
                                    if let InputAction::Compose { append_enter, .. } =
                                        prepared.action_for_review()
                                    {
                                        ui.label(format!("正文后追加 Enter：{append_enter}"));
                                    }
                                }
                                _ => {
                                    ui.label("逻辑终端控制动作");
                                }
                            }
                            for target in prepared.snapshot().targets() {
                                ui.label(format!("{} · {}", target.title, target.session_id));
                            }
                        });
                    confirm = ui.button("确认发送这段正文").clicked();
                    cancel = ui.button("取消").clicked();
                });
            if cancel {
                self.pending = None;
            } else if confirm && let Some(prepared) = self.pending.take() {
                self.dispatch(prepared, SyncInputApproval::Confirmed);
            }
        }
            search_requested
    }
}

impl Drop for DesktopSyncInput {
    fn drop(&mut self) {
        self.controller.port().permit.revoke();
    }
}

#[cfg(test)]
mod tests;
