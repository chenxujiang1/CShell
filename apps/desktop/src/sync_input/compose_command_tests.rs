use super::*;

#[test]
#[ignore = "explicit fixed offline broadcast preview"]
fn render_broadcast_preview() {
    use cshell_domain::*;
    use cshell_ui::{SavedSshProfileViewModel, WorkbenchViewModel, WorkspaceTabViewModel};
    let mut f = Fixture::new(3);
    for entry in f.sync.controller.port_mut().targets.values_mut() {
        entry.opened.kind = SyncInputTargetKind::Ssh;
        entry.configured_endpoint = Some(format!("deploy@{}.example:22", entry.opened.title));
    }
    f.sync.choose_targets(f.window, Some(f.tabs[0]));
    f.sync.compose.text.push_str("hostname");
    let mut model = WorkbenchViewModel {
        daemon_connected: true,
        ..Default::default()
    };
    let mut doc = WorkspaceDocument::default();
    for index in 0..3 {
        let id = ProfileId::new();
        let tab = f.tabs[index];
        model.saved_ssh_profiles.push(SavedSshProfileViewModel {
            id,
            name: format!("host-{index}"),
            target: format!("deploy@host-{index}.example:22"),
            favorite: false,
            folder_id: None,
            kind: ProfileKind::Ssh,
        });
        model.tabs.push(WorkspaceTabViewModel {
            id: tab,
            title: format!("host-{index}"),
            session_id: Some(f.sessions[index]),
        });
        model
            .ssh_tab_states
            .insert(tab, cshell_ui::SshConnectionState::Connected);
        if f.sync.targeted(tab) { model.input_target_tabs.insert(tab, false); }
        if index == 1 { model.unread_output_tabs.insert(tab); }
        doc.tab_groups[0].tabs.push(tab);
        doc.bindings.push(WorkspaceBinding {
            tab_id: tab,
            profile_id: Some(id),
            title: format!("host-{index}"),
        });
    }
    doc.tab_groups[0].active_tab = Some(f.tabs[0]);
    model.selected_tab = Some(f.tabs[0]);
    model.workspace_window_id = Some(doc.windows[0].id);
    model.workspace_document = Some(doc);
    for (name, picker, reviewing, size) in [
        ("ssh-broadcast", false, false, egui::vec2(900.0, 600.0)),
        ("ssh-broadcast-targets", true, false, egui::vec2(900.0, 600.0)),
        ("ssh-send-review", false, true, egui::vec2(900.0, 600.0)),
        ("ssh-send-review-small", false, true, egui::vec2(720.0, 480.0)),
        ("terminal-paste-review", false, true, egui::vec2(900.0, 600.0)),
        ("terminal-paste-review-small", false, true, egui::vec2(720.0, 480.0)),
        ("terminal-context-link", false, false, egui::vec2(900.0, 600.0)),
        ("terminal-context-path", false, false, egui::vec2(720.0, 480.0)),
    ] {
        let ctx = crate::ui_layout_gate::context();
        f.sync.pending = None;
        f.sync.pending_current = None;
        let mut mouse_app = crate::DesktopApp::default();
        if name.starts_with("terminal-context") {
            use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};
            let text = if name.ends_with("link") { "https://example.org/docs" } else { "/var/log/nginx/error.log" };
            let mut engine = AlacrittyTerminalEngine::new(TerminalSize::cells(8, 60));
            engine.feed(text.as_bytes());
            mouse_app.current_workspace_tab = Some(f.tabs[0]);
            mouse_app.egui_context = ctx.clone();
            mouse_app.terminal_surface.submit_snapshot(Arc::new(engine.snapshot()));
            mouse_app.begin_clipboard_selection(cshell_render::TerminalCellPoint { row: 0, column: 0 });
            mouse_app.update_clipboard_selection(cshell_render::TerminalCellPoint { row: 0, column: text.len() as u16 - 1 });
            mouse_app.open_clipboard_menu(egui::pos2(size.x - 10.0, size.y - 10.0));
        }
        f.sync.panel_open = false;
        f.sync.compose.focus_requested = !picker;
        if reviewing {
            f.sync.pending = None; f.sync.pending_current = None;
            f.sync.compose.multiline = true;
            f.sync.compose.text.zeroize();
            f.sync.compose.text.push_str(if size.x < 800.0 { "ls -lah /tmp\nrm -rf /tmp/cshell-demo\ndf -h" } else { "systemctl status nginx\ndf -h\njournalctl -u nginx -n 20" });
            f.sync.request_compose_send(f.tabs[0], false);
            if name.starts_with("terminal-paste") {
                f.sync.pending = None;
                let text = if size.x < 800.0 { "printf 'hello'\n\tcat /etc/os-release\n" } else { "hostname\npwd\nls -lah /var/log\n" };
                f.sync.pending_current = Some(f.sync.controller.prepare_current_input(f.tabs[0],
                    InputAction::Paste { text: text.into(), bracketed: true }, SyncInputContext::TerminalCommit).unwrap());
                f.sync.compose.review_started = Some(std::time::Instant::now());
            }
        }
        let mut textures = std::collections::HashMap::new();
        let mut primitives = vec![];
        let mut scope_pos = None;
        for frame in 0..8 {
            let events = if picker && (frame == 4 || frame == 5) {
                let pos = scope_pos.expect("scope button position");
                vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: frame == 4, modifiers: egui::Modifiers::NONE }]
            } else { vec![] };
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        size,
                    )),
                    time: Some(frame as f64 * 0.25),
                    events,
                    ..Default::default()
                },
                |ui| {
                    cshell_ui::draw_workbench_chrome(ui, &mut model);
                    f.sync.draw(ui, f.window, f.group, Some(f.tabs[0]), false);
                    let rect = cshell_ui::draw_workbench_content(ui, &mut model);
                    ui.painter().with_clip_rect(rect).rect_filled(
                        rect,
                        0.0,
                        egui::Color32::from_rgb(15, 20, 29),
                    );
                    ui.painter().with_clip_rect(rect).text(
                        rect.min + egui::vec2(18.0, 18.0),
                        egui::Align2::LEFT_TOP,
                        "离线命令栏预览 · 未连接服务器",
                        egui::FontId::proportional(14.0),
                        cshell_ui::theme::MUTED,
                    );
                    if name.starts_with("terminal-context") {
                        let _ = mouse_app.draw_clipboard_ui(ui.ctx());
                    }
                },
            );
            if let Some(tree) = &out.platform_output.accesskit_update {
                if let Some((_, node)) = tree.nodes.iter().find(|(_, n)| n.label() == Some("自定义 · 2 台")) {
                    scope_pos = Some(crate::ui_layout_gate::bounds(node).center());
                }
            }
            for (id, deltas) in out.textures_delta.set.drain() {
                for delta in deltas {
                    let egui::ImageData::Color(image) = delta.image;
                    if let Some([x, y]) = delta.pos {
                        let target: &mut egui::ColorImage = textures.get_mut(&id).unwrap();
                        for row in 0..image.size[1] {
                            let start = (y + row) * target.size[0] + x;
                            target.pixels[start..start + image.size[0]].copy_from_slice(
                                &image.pixels[row * image.size[0]..(row + 1) * image.size[0]],
                            );
                        }
                    } else {
                        textures.insert(id, (*image).clone());
                    }
                }
            }
            primitives = ctx.tessellate(out.shapes, 1.0);
        }
        let path = format!(
            "{}/../../artifacts/ui-refresh-20261015/{name}.ppm",
            env!("CARGO_MANIFEST_DIR")
        );
        super::dock_tests::raster::write(&path, size.x as usize, size.y as usize, primitives, &textures).unwrap();
    }
}
use crate::daemon_connection::DesktopDaemonView;
use crate::daemon_input::QueuedDesktopInput;
use crate::sync_input::tests::{audit_in, ready_target};
use std::sync::{Arc, Mutex};

#[test]
fn opening_search_temporarily_hides_command_bar_without_losing_draft_or_targets() {
    let mut f = Fixture::new(3);
    f.sync.choose_targets(f.window, Some(f.tabs[0]));
    f.sync.compose.text.push_str("printf 'keep this draft'");
    let selected = f.sync.selected.clone();
    let mut output = f.ctx.run_ui(egui::RawInput::default(), |ui| {
        f.sync.draw(ui, f.window, f.group, Some(f.tabs[0]), true);
    });
    output.textures_delta.clear();
    assert_eq!(f.sync.compose.text.as_str(), "printf 'keep this draft'");
    assert_eq!(f.sync.selected, selected);
    assert!(f.sync.compose_open(f.window));
}

struct Fixture {
    _temp: tempfile::TempDir,
    sync: DesktopSyncInput,
    ctx: egui::Context,
    window: WorkspaceWindowId,
    group: cshell_domain::TabGroupId,
    tabs: Vec<TabId>,
    sessions: Vec<cshell_domain::SessionId>,
    views: Vec<Arc<Mutex<DesktopDaemonView>>>,
    queues: Vec<tokio::sync::mpsc::Receiver<QueuedDesktopInput>>,
}
impl Fixture {
    fn new(count: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let mut sync = DesktopSyncInput::with_audit(audit_in(temp.path()));
        let window = WorkspaceWindowId::new();
        let group = cshell_domain::TabGroupId::new();
        let (mut targets, mut tabs, mut sessions, mut views, mut queues) =
            (vec![], vec![], vec![], vec![], vec![]);
        for index in 0..count {
            let (mut target, view, queue) = ready_target(window, group);
            target.opened.title = format!("host-{index}");
            tabs.push(target.opened.tab_id);
            sessions.push(target.opened.session_id);
            views.push(view);
            queues.push(queue);
            targets.push(target);
        }
        sync.replace_window(window, targets);
        sync.selected = tabs[1..].iter().copied().collect();
        sync.panel_open = true;
        Self {
            _temp: temp,
            sync,
            ctx: crate::ui_layout_gate::context(),
            window,
            group,
            tabs,
            sessions,
            views,
            queues,
        }
    }
    fn frame(&mut self, mut events: Vec<egui::Event>) -> egui::accesskit::TreeUpdate {
        let modifiers = events
            .iter()
            .rev()
            .find_map(|e| {
                if let egui::Event::Key { modifiers, .. } = e {
                    Some(*modifiers)
                } else {
                    None
                }
            })
            .unwrap_or_default();
        events.insert(0, egui::Event::ModifiersChanged(modifiers));
        let mut out = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                self.sync
                    .draw(ui, self.window, self.group, Some(self.tabs[0]), false);
            },
        );
        out.textures_delta.clear();
        out.platform_output.accesskit_update.unwrap()
    }
    fn idle(&mut self) {
        for _ in 0..3 {
            self.frame(vec![]);
        }
    }
    fn click(&mut self, label: &str) {
        self.idle();
        let tree = self.frame(vec![]);
        let node = &tree
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some(label))
            .unwrap_or_else(|| panic!("missing {label}"))
            .1;
        let pos = crate::ui_layout_gate::bounds(node).center();
        assert!(
            self.ctx.content_rect().contains(pos),
            "{label} is outside viewport: {pos:?}"
        );
        for pressed in [true, false] {
            self.frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }
        self.idle();
    }
    fn enable(&mut self) {
        self.click(&format!("启用 {} 个目标", self.sync.selected.len()));
    }
    fn text(&mut self, text: &str) {
        self.frame(vec![egui::Event::Text(text.into())]);
    }
    fn enter(&mut self, ctrl: bool) {
        for pressed in [true, false] {
            self.frame(vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers {
                    ctrl,
                    command: ctrl,
                    ..Default::default()
                },
            }]);
        }
    }
    fn key(&mut self, key: egui::Key) {
        for pressed in [true, false] {
            self.frame(vec![egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE }]);
        }
    }
    fn assert_empty(&mut self) {
        for q in &mut self.queues {
            assert!(q.try_recv().is_err());
        }
    }
    fn take_body(&mut self, index: usize, expected: &str) -> QueuedDesktopInput {
        let input = self.queues[index].try_recv().unwrap();
        assert!(
            matches!(&input.action,InputAction::Compose{text,append_enter:true} if text==expected)
        );
        assert_eq!(
            input.validate(&self.views[index], self.sessions[index]),
            Ok(())
        );
        input
    }
}

#[test]
fn inline_scope_ten_commands_history_and_exclusion_need_no_enable_dialog() {
    let mut f = Fixture::new(3);
    f.sync.panel_open = false;
    f.sync.open_compose(f.window, Some(f.tabs[0]));
    f.idle();
    f.click("当前会话");
    f.click("应用 · 2 台");
    assert!(f.sync.compose.armed && f.sync.arm_review.is_none() && !f.sync.panel_open);
    for i in 0..10 {
        let body = format!("echo {i}");
        f.text(&body); f.enter(false);
        f.take_body(1, &body); f.take_body(2, &body);
        assert!(f.queues[0].try_recv().is_err());
        assert!(f.sync.compose_open(f.window));
    }
    assert_eq!(f.sync.history.entries().count(), 10);
    f.key(egui::Key::ArrowUp);
    assert_eq!(f.sync.compose.text.as_str(), "echo 9");
    f.key(egui::Key::ArrowUp);
    assert_eq!(f.sync.compose.text.as_str(), "echo 8");
    f.enter(false); f.take_body(1, "echo 8"); f.take_body(2, "echo 8");
    f.click("自定义 · 2 台");
    f.click("host-2 · 本地");
    f.click("应用 · 1 台");
    f.text("hostname"); f.enter(false);
    f.take_body(1, "hostname");
    assert!(f.queues[0].try_recv().is_err() && f.queues[2].try_recv().is_err());
}

#[test]
fn history_navigation_restores_draft_and_popup_escape_keeps_editor() {
    let mut f = Fixture::new(3); f.enable();
    f.text("hostname"); f.enter(false); f.take_body(1, "hostname"); f.take_body(2, "hostname");
    f.text("echo 草稿");
    f.key(egui::Key::ArrowUp);
    assert_eq!(f.sync.compose.text.as_str(), "hostname");
    f.key(egui::Key::ArrowDown);
    assert_eq!(f.sync.compose.text.as_str(), "echo 草稿");
    f.click("历史");
    f.enter(false); f.assert_empty();
    f.key(egui::Key::Escape);
    f.idle();
    assert!(f.sync.compose_open(f.window));
    assert_eq!(f.sync.compose.text.as_str(), "echo 草稿");
    f.enter(false); f.take_body(1, "echo 草稿"); f.take_body(2, "echo 草稿");
}

#[test]
fn history_search_edits_only_the_popup_and_recalled_command_appends_at_end() {
    let mut f = Fixture::new(3); f.enable();
    f.text("hostname"); f.enter(false); f.take_body(1, "hostname"); f.take_body(2, "hostname");
    f.text("echo unfinished");
    f.click("历史"); f.text("host");
    assert_eq!(f.sync.compose.text.as_str(), "echo unfinished");
    f.assert_empty();
    f.click("hostname");
    assert_eq!(f.sync.compose.text.as_str(), "hostname");
    f.text(" -f"); f.enter(false);
    f.take_body(1, "hostname -f"); f.take_body(2, "hostname -f");
}

#[test]
fn pinned_bar_keeps_scope_across_sends_modals_and_terminal_focus() {
    let mut f = Fixture::new(3); f.enable();
    for body in ["hostname", "whoami"] {
        f.text(body); f.enter(false); f.take_body(1, body); f.take_body(2, body);
        f.sync.set_modal_blocked(true);
        f.sync.request_compose_send(f.tabs[0], false); f.assert_empty();
        f.sync.set_modal_blocked(false); f.idle();
        assert!(f.sync.compose_open(f.window) && f.sync.compose.armed && f.sync.is_armed());
        f.sync.compose.focus_requested = true; f.idle();
    }
    assert!(f.sync.focus_terminal());
    f.ctx.memory_mut(|m| { if let Some(id) = m.focused() { m.surrender_focus(id); } });
    f.idle();
    assert!(!f.sync.blocks_input());
    assert!(f.sync.compose_open(f.window) && f.sync.compose.armed);
    f.key(egui::Key::Escape); assert!(f.sync.compose_open(f.window));
    f.sync.hide_command_bar(); f.idle(); assert!(!f.sync.compose_open(f.window));
}

#[test]
fn newly_ready_current_tab_can_join_and_failed_scope_never_falls_back_to_current() {
    let mut f = Fixture::new(3); f.enable();
    f.click("自定义 · 2 台"); f.click("加入当前会话");
    assert!(f.tabs.iter().all(|tab| f.sync.targeted(*tab)));
    f.text("hostname"); f.enter(false);
    for i in 0..3 { f.take_body(i, "hostname"); }
    f.sync.disarm(SyncInputDisarmReason::SourceUnavailable);
    f.idle();
    assert!(f.sync.compose_open(f.window) && f.sync.compose.armed);
    let tree = f.frame(vec![]);
    assert!(tree.nodes.iter().any(|(_,n)| n.label() == Some("目标已暂停")));
    f.text("whoami"); f.enter(false); f.assert_empty();
}

#[test]
fn ordinary_new_session_shows_one_bar_without_stealing_terminal_focus() {
    let mut f = Fixture::new(3); f.sync.panel_open = false; f.idle();
    assert!(f.sync.compose_open(f.window) && !f.sync.blocks_input());
    let tree = f.frame(vec![]);
    assert!(!tree.nodes.iter().any(|(_,n)| n.label() == Some("键盘同步…")));
}

#[test]
fn command_scope_and_draft_survive_focus_but_old_work_and_review_are_revoked() {
    let mut f = Fixture::new(3); f.enable();
    f.text("hostname"); f.enter(false);
    let queued = f.take_body(1, "hostname"); f.take_body(2, "hostname");
    f.text("echo unfinished");
    f.sync.focus_lost(Some(f.tabs[0]));
    assert!(f.sync.is_armed() && !f.sync.is_realtime());
    assert_eq!(f.sync.compose.text.as_str(), "echo unfinished");
    assert!(queued.validate(&f.views[1], f.sessions[1]).is_err());
    f.sync.source_changed(f.tabs[2]);
    assert_eq!(f.sync.compose.text.as_str(), "echo unfinished");
    assert!(f.sync.targeted(f.tabs[1]) && f.sync.targeted(f.tabs[2]) && !f.sync.targeted(f.tabs[0]));
    f.sync.compose.text.zeroize(); f.sync.compose.text.push_str("rm -rf /tmp/cshell-test");
    f.sync.request_compose_send(f.tabs[2], false);
    assert!(f.sync.pending.is_some());
    f.sync.focus_lost(Some(f.tabs[2]));
    assert!(f.sync.pending.is_none() && !f.sync.compose.risk_ack);
    f.assert_empty();
}

#[test]
fn keyboard_send_respects_the_same_utf8_byte_limit_as_the_button(){
    let mut f=Fixture::new(3);f.enable();
    f.sync.compose.text.push_str(&"你".repeat(MAX_QUICK_COMMAND_BODY_BYTES/3+1));
    f.sync.request_compose_send(f.tabs[0],false);f.assert_empty();
    assert!(f.sync.pending.is_none() && f.sync.pending_current.is_none());
    assert!(f.sync.compose.text.len()>MAX_QUICK_COMMAND_BODY_BYTES);
    f.sync.compose.text.zeroize();f.sync.compose.text.push_str("whoami");
    f.sync.request_compose_send(f.tabs[0],false);f.take_body(1,"whoami");f.take_body(2,"whoami");
}

#[test]
fn target_picker_blocks_background_enter_and_escape_preserves_draft(){
    let mut f=Fixture::new(3);f.enable();f.text("hostname");
    f.sync.panel_open=true;f.enter(false);f.assert_empty();
    assert_eq!(f.sync.compose.text.as_str(),"hostname");
    f.frame(vec![egui::Event::Key{key:egui::Key::Escape,physical_key:None,pressed:true,repeat:false,modifiers:egui::Modifiers::NONE}]);
    assert!(!f.sync.panel_open && f.sync.compose_open(f.window));
    f.idle();f.enter(false);f.take_body(1,"hostname");f.take_body(2,"hostname");
}

#[test]
fn current_only_command_does_not_steal_or_expand_the_saved_target_set() {
    let mut f = Fixture::new(3);
    f.enable();
    f.click("自定义 · 2 台");
    f.click("当前会话");
    assert_eq!(f.sync.target_badges(), [(f.tabs[0], false)].into_iter().collect());
    f.sync.compose.focus_requested = true;
    f.idle();
    f.text("hostname");
    f.enter(false);
    f.take_body(0, "hostname");
    assert!(f.queues[1].try_recv().is_err() && f.queues[2].try_recv().is_err());
    assert!(f.sync.is_armed() && f.sync.targeted(f.sync.source.unwrap()));
    f.click("当前会话");
    f.click("使用已选择的目标");
    assert_eq!(f.sync.target_badges(), [(f.tabs[1], false), (f.tabs[2], false)].into_iter().collect());
    f.sync.compose.focus_requested = true;
    f.idle();
    f.text("whoami");
    f.enter(false);
    f.take_body(1, "whoami");
    f.take_body(2, "whoami");
    assert!(f.queues[0].try_recv().is_err());
}

#[test]
fn saved_quick_commands_share_direct_send_and_risky_review_policy() {
    let mut f = Fixture::new(3);
    f.enable();
    for (body, risky) in [("whoami", false), ("rm -rf /tmp/cshell-test", true)] {
        let command = QuickCommandRecord {
            id: QuickCommandId::new(),
            name: "test".into(),
            folder_id: None,
            tags: Default::default(),
            systems: Default::default(),
            action: QuickCommandAction::SendText {
                text: body.into(),
                append_enter: true,
                target: QuickCommandTarget::Armed,
            },
        };
        f.sync.load_command(&command);
        f.sync.request_compose_send(f.tabs[0], false);
        if risky {
            assert!(f.sync.pending.as_ref().unwrap().risk().destructive_command);
            f.assert_empty();
        } else {
            f.take_body(1, body);
            f.take_body(2, body);
            assert!(f.queues[0].try_recv().is_err());
        }
    }
}

#[test]
fn select_once_enter_twice_keeps_editor_and_excludes_current_unselected_tab() {
    let mut f = Fixture::new(3);
    f.enable();
    assert!(f.sync.is_armed() && !f.sync.is_realtime());
    assert!(f.sync.arm_review.is_none());
    assert!(f.sync.compose_open(f.window));
    for body in ["hostname", "whoami"] {
        f.text(body);
        f.enter(false);
        assert!(f.sync.pending.is_none() && f.sync.pending_current.is_none());
        assert!(f.sync.compose.text.is_empty() && f.sync.compose_open(f.window));
        assert!(f.queues[0].try_recv().is_err());
        f.take_body(1, body);
        f.take_body(2, body);
    }
    f.sync.close_compose();
    assert!(!f.sync.route(
        f.tabs[0],
        InputAction::Text("ordinary-terminal-input".into())
    ));
    f.assert_empty();
    f.sync.source_changed(f.tabs[0]);
    assert!(
        f.sync.is_armed(),
        "viewing a different tab must not expand or lose command targets"
    );
}

#[test]
fn multiline_control_enter_opens_clear_review_and_explicit_send_keeps_editor() {
    let mut f = Fixture::new(3);
    f.enable();
    f.sync.compose.multiline = true;
    f.text("echo one");
    f.enter(false);
    f.text("echo two");
    f.assert_empty();
    assert!(f.sync.pending.is_none());
    f.enter(true);
    assert!(f.sync.pending.as_ref().unwrap().risk().multiline);
    f.assert_empty();
    assert!(!f.frame(vec![]).nodes.iter().any(|(_,n)| n.label() == Some("我已核对风险，并确认发送")));
    f.click("发送到 2 个会话");
    f.take_body(1, "echo one\necho two");
    f.take_body(2, "echo one\necho two");
    assert!(f.sync.compose_open(f.window) && f.sync.compose.text.is_empty());
}

#[test]
fn risky_review_requires_ack_and_back_escape_close_preserve_draft_and_scope() {
    let mut f = Fixture::new(3); f.enable();
    let body = "echo check\nrm -rf /tmp/cshell-test";
    f.sync.compose.multiline = true; f.text(body); f.enter(true); f.assert_empty();
    f.click("发送到 2 个会话"); f.assert_empty();
    f.click("返回编辑");
    assert_eq!(f.sync.compose.text.as_str(), body);
    assert!(f.sync.compose.armed && f.sync.is_armed() && f.sync.compose_open(f.window));
    for close in [false, true] {
        f.enter(true); f.assert_empty();
        if close { f.click("×"); } else { f.key(egui::Key::Escape); f.idle(); }
        assert!(f.sync.pending.is_none() && f.sync.pending_current.is_none());
        assert_eq!(f.sync.compose.text.as_str(), body);
        assert!(f.sync.compose.armed && f.sync.is_armed() && f.sync.compose_open(f.window));
    }
    f.enter(true);
    f.click("我已核对风险，并确认发送");
    f.click("发送到 2 个会话");
    f.take_body(1, body); f.take_body(2, body); assert!(f.queues[0].try_recv().is_err());
    assert!(f.sync.compose_open(f.window) && f.sync.compose.text.is_empty());
}

#[test]
fn expired_review_disables_send_and_returns_to_the_unmodified_draft() {
    let mut f = Fixture::new(3); f.enable();
    f.sync.compose.multiline = true; f.text("echo one\necho two"); f.enter(true);
    f.sync.compose.review_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(31));
    f.idle(); f.click("发送到 2 个会话"); f.assert_empty();
    f.click("返回编辑");
    assert_eq!(f.sync.compose.text.as_str(), "echo one\necho two");
    assert!(f.sync.is_armed() && f.sync.compose.armed);
    f.enter(true); f.click("发送到 2 个会话");
    f.take_body(1, "echo one\necho two"); f.take_body(2, "echo one\necho two");
}

#[test]
fn connection_change_during_review_preserves_the_unsent_body() {
    let mut f = Fixture::new(3); f.enable();
    f.sync.compose.multiline = true; f.text("echo one\necho two"); f.enter(true);
    let source = f.sync.source.unwrap();
    let index = f.tabs.iter().position(|tab| *tab == source).unwrap();
    f.views[index].lock().unwrap().session_id = Some(cshell_domain::SessionId::new());
    f.click("发送到 2 个会话"); f.assert_empty();
    assert!(f.sync.compose_open(f.window));
    assert_eq!(f.sync.compose.text.as_str(), "echo one\necho two");
    assert!(f.sync.compose.armed && !f.sync.is_armed());
}

#[test]
fn ime_commit_enter_does_not_send_until_a_separate_command_enter() {
    let mut f = Fixture::new(3);
    f.enable();
    f.text("echo ");
    f.frame(vec![egui::Event::Ime(egui::ImeEvent::Preedit {
        text: "中".into(),
        active_range_chars: Some(0..1),
    })]);
    f.frame(vec![
        egui::Event::Ime(egui::ImeEvent::Commit("中".into())),
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    f.assert_empty();
    assert!(f.sync.pending.is_none());
    f.frame(vec![egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }]);
    f.idle();
    f.enter(false);
    f.take_body(1, "echo 中");
    f.take_body(2, "echo 中");
}

#[test]
fn changing_targets_revokes_old_queued_command_and_large_groups_keep_arm_review() {
    let mut f = Fixture::new(3);
    f.enable();
    f.text("hostname");
    f.enter(false);
    let old = f.take_body(1, "hostname");
    f.take_body(2, "hostname");
    f.sync.selected = [f.tabs[0], f.tabs[1]].into_iter().collect();
    f.sync.panel_open = true;
    f.enable();
    assert!(old.validate(&f.views[1], f.sessions[1]).is_err());
    f.text("whoami");
    f.enter(false);
    f.take_body(0, "whoami");
    f.take_body(1, "whoami");
    assert!(f.queues[2].try_recv().is_err());
    f.click("更多");
    f.click("清除发送目标");
    assert!(!f.sync.is_armed());
    let mut large = Fixture::new(12);
    large.enable();
    assert!(
        !large.sync.is_armed() && large.sync.arm_review.is_some(),
        "armed={}, review={}, selected={}, status={}",
        large.sync.is_armed(),
        large.sync.arm_review.is_some(),
        large.sync.selected.len(),
        large.sync.status
    );
    large.assert_empty();
}
