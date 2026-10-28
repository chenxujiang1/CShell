use super::*;
use crate::{
    DesktopApp,
    daemon_connection::{
        DesktopConnectionConfig, DesktopDaemonConnection, DesktopDaemonView,
        ResolvedDesktopEndpoint,
    },
    daemon_input::QueuedDesktopInput,
};
use cshell_domain::{
    InputAction, KeyCode, KeyEvent, Modifiers, SessionId, TabGroupId, TerminalSize,
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[test]
fn persistent_stop_button_revokes_queued_broadcast_in_a_small_window() {
    let directory=tempfile::tempdir().unwrap();
    let mut sync=DesktopSyncInput::with_audit(audit_in(directory.path()));
    let window=WorkspaceWindowId::new();
    let group=TabGroupId::new();
    let (first,view,mut queue)=ready_target(window,group);
    let (second,_,_second_queue)=ready_target(window,group);
    let tabs=[first.opened.tab_id,second.opened.tab_id];
    let session=first.opened.session_id;
    sync.replace_window(window,vec![first,second]);
    arm(&mut sync,&tabs);
    assert!(sync.route(tabs[0],InputAction::Text("x".into())));
    let input=queue.try_recv().unwrap();
    assert_eq!(input.validate(&view,session),Ok(()));
    let context=crate::ui_layout_gate::context();
    let mut draw=|events:Vec<egui::Event>| {
        let mut output=context.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(720.0,480.0))),events,..Default::default()},|ui|{sync.draw(ui,window,group,Some(tabs[0]),false);});
        output.textures_delta.clear();
        output.platform_output.accesskit_update.unwrap()
    };
    for _ in 0..3 {draw(vec![]);}
    let tree=draw(vec![]);
    assert!(tree.nodes.iter().any(|(_,n)|n.label()==Some("同步 2 个目标") || n.value()==Some("同步 2 个目标")));
    let rect=crate::ui_layout_gate::bounds(&tree.nodes.iter().find(|(_,n)|n.label()==Some("停止同步")).unwrap().1);
    assert!(rect.min.x>=0.0 && rect.max.x<=720.0);
    let pos=rect.center();
    draw(vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton{pos,button:egui::PointerButton::Primary,pressed:true,modifiers:egui::Modifiers::NONE}]);
    draw(vec![egui::Event::PointerButton{pos,button:egui::PointerButton::Primary,pressed:false,modifiers:egui::Modifiers::NONE}]);
    assert!(!sync.is_armed());
    assert!(input.validate(&view,session).is_err());
}

pub(super) fn ready_target(
    window: WorkspaceWindowId,
    group: TabGroupId,
) -> (
    DesktopSyncTarget,
    Arc<Mutex<DesktopDaemonView>>,
    tokio::sync::mpsc::Receiver<QueuedDesktopInput>,
) {
    let session_id = SessionId::new();
    let view = Arc::new(Mutex::new(DesktopDaemonView {
        connected: true,
        session_id: Some(session_id),
        session_running: true,
        input_security: DesktopInputSecurity::Terminal,
        compose_supported: true,
        ..DesktopDaemonView::default()
    }));
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    (
        DesktopSyncTarget {
            opened: OpenedSyncInputTarget {
                tab_id: TabId::new(),
                window_id: window,
                group_id: group,
                session_id,
                kind: SyncInputTargetKind::Local,
                title: "sensitive-profile-title".into(),
                eligibility: SyncInputEligibility::Ready,
            },
            sender: Some(DesktopInputSender::new(Arc::clone(&view), sender)),
            control_error: None,
            configured_endpoint: None,
        },
        view,
        receiver,
    )
}

pub(super) fn audit_in(directory: &std::path::Path) -> DesktopSyncAudit {
    let paths = cshell_ipc::RuntimePaths::prepare(directory.join("runtime"))
        .unwrap_or_else(|error| panic!("runtime: {error}"));
    DesktopSyncAudit::start(&paths).unwrap_or_else(|error| panic!("audit: {error}"))
}

fn arm(sync: &mut DesktopSyncInput, tabs: &[TabId]) {
    let review = sync
        .controller
        .review_arm(SyncInputSelection::Manual, tabs)
        .unwrap_or_else(|error| panic!("review: {error}"));
    sync.controller
        .arm(review, SyncInputApproval::Confirmed)
        .unwrap_or_else(|error| panic!("arm: {error}"));
}

#[test]
fn current_compose_queue_is_current_only_and_switch_revokes_it_before_write() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("temp: {error}"));
    let mut sync = DesktopSyncInput::with_audit(audit_in(directory.path()));
    let window = WorkspaceWindowId::new();
    let group = TabGroupId::new();
    let (first, view, mut first_queue) = ready_target(window, group);
    let (second, _, mut second_queue) = ready_target(window, group);
    let tabs = [first.opened.tab_id, second.opened.tab_id];
    let session = first.opened.session_id;
    sync.replace_window(window, vec![first, second]);
    arm(&mut sync, &tabs);
    sync.prepare_compose(tabs[0], "body".into(), true, false);
    assert!(first_queue.try_recv().is_err());
    let review = sync
        .pending_current
        .take()
        .unwrap_or_else(|| panic!("current review"));
    sync.dispatch_current(review);
    let input = first_queue
        .try_recv()
        .unwrap_or_else(|error| panic!("queue: {error}"));
    assert!(second_queue.try_recv().is_err());
    assert_eq!(input.validate(&view, session), Ok(()));
    assert!(sync.is_armed());
    sync.source_changed(tabs[1]);
    assert!(input.validate(&view, session).is_err());
    sync.prepare_compose(tabs[1], "pending".into(), false, false);
    sync.focus_lost(Some(tabs[1]));
    assert!(sync.pending_current.is_none());
}

#[test]
fn current_compose_pre_dispatch_audit_failure_revokes_previously_queued_input() {
    let (sender, _audit_receiver) = std::sync::mpsc::sync_channel(2);
    let audit = DesktopSyncAudit {
        sender: Some(sender),
        healthy: Arc::new(AtomicBool::new(true)),
        worker: None,
    };
    let mut sync = DesktopSyncInput::with_audit(audit);
    let window = WorkspaceWindowId::new();
    let (target, view, mut receiver) = ready_target(window, TabGroupId::new());
    let tab = target.opened.tab_id;
    let session = target.opened.session_id;
    sync.replace_window(window, vec![target]);
    sync.prepare_compose(tab, "first".into(), false, false);
    let review = sync
        .pending_current
        .take()
        .unwrap_or_else(|| panic!("first review"));
    sync.dispatch_current(review);
    let input = receiver
        .try_recv()
        .unwrap_or_else(|error| panic!("queue: {error}"));
    assert_eq!(input.validate(&view, session), Ok(()));
    sync.prepare_compose(tab, "second".into(), false, false);
    let review = sync
        .pending_current
        .take()
        .unwrap_or_else(|| panic!("second review"));
    sync.dispatch_current(review);
    assert!(receiver.try_recv().is_err());
    assert!(input.validate(&view, session).is_err());
}

#[test]
fn desktop_review_freezes_payload_and_disarm_revokes_queued_input_without_logging_secrets() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("temp: {error}"));
    let mut sync = DesktopSyncInput::with_audit(audit_in(directory.path()));
    let window = WorkspaceWindowId::new();
    let group = TabGroupId::new();
    let (first, first_view, mut first_receiver) = ready_target(window, group);
    let (second, second_view, mut second_receiver) = ready_target(window, group);
    let tabs = [first.opened.tab_id, second.opened.tab_id];
    let sessions = [first.opened.session_id, second.opened.session_id];
    sync.replace_window(window, vec![first, second]);
    assert!(!sync.is_armed());
    arm(&mut sync, &tabs);
    let payload = "PRIVATE_REVIEW_BODY\nsecond line";
    assert!(sync.route(
        tabs[0],
        InputAction::Paste {
            text: payload.into(),
            bracketed: true
        }
    ));
    assert!(sync.blocks_input());
    assert!(first_receiver.try_recv().is_err());
    assert!(sync.route(
        tabs[0],
        InputAction::Text("must not replace pending payload".into())
    ));
    let prepared = sync
        .pending
        .take()
        .unwrap_or_else(|| panic!("pending exact input"));
    assert_eq!(
        prepared.action_for_review(),
        &InputAction::Paste {
            text: payload.into(),
            bracketed: true
        }
    );
    sync.dispatch(prepared, SyncInputApproval::Confirmed);
    let first = first_receiver
        .try_recv()
        .unwrap_or_else(|error| panic!("source queue: {error}"));
    let second = second_receiver
        .try_recv()
        .unwrap_or_else(|error| panic!("target queue: {error}"));
    assert_eq!(first.validate(&first_view, sessions[0]), Ok(()));
    assert_eq!(second.validate(&second_view, sessions[1]), Ok(()));
    sync.disarm(SyncInputDisarmReason::User);
    assert!(first.validate(&first_view, sessions[0]).is_err());
    assert!(second.validate(&second_view, sessions[1]).is_err());
    drop(sync);
    let entries = std::fs::read_dir(directory.path().join("runtime/sync-input-audit"))
        .unwrap_or_else(|error| panic!("audit directory: {error}"));
    let mut records = Vec::new();
    for entry in entries {
        let path = entry
            .unwrap_or_else(|error| panic!("audit entry: {error}"))
            .path();
        let contents =
            std::fs::read_to_string(path).unwrap_or_else(|error| panic!("audit contents: {error}"));
        assert!(!contents.contains("PRIVATE_REVIEW_BODY"));
        assert!(!contents.contains("sensitive-profile-title"));
        assert!(!contents.contains("must not replace"));
        records.extend(contents.lines().map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|error| panic!("json metadata: {error}"))
        }));
    }
    assert!(
        records
            .iter()
            .any(|record| record["event"]["operation"] == "armed")
    );
    assert!(
        records
            .iter()
            .any(|record| record["event"]["operation"] == "input_planned")
    );
    let dispatched = records
        .iter()
        .find(|record| record["event"]["operation"] == "input_dispatched")
        .unwrap_or_else(|| panic!("dispatch metadata"));
    assert_eq!(
        dispatched["event"]["deliveries"].as_array().map(Vec::len),
        Some(2)
    );
}

#[test]
fn audit_writer_failure_revokes_pending_wire_writes_and_unavailable_audit_cannot_arm() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("temp: {error}"));
    let audit = audit_in(directory.path());
    let healthy = Arc::clone(&audit.healthy);
    let mut sync = DesktopSyncInput::with_audit(audit);
    let window = WorkspaceWindowId::new();
    let (target, view, mut receiver) = ready_target(window, TabGroupId::new());
    let tab = target.opened.tab_id;
    let session = target.opened.session_id;
    sync.replace_window(window, vec![target]);
    arm(&mut sync, &[tab]);
    assert!(sync.route(
        tab,
        InputAction::Text("accepted before writer failure".into())
    ));
    let input = receiver
        .try_recv()
        .unwrap_or_else(|error| panic!("input: {error}"));
    assert_eq!(input.validate(&view, session), Ok(()));
    healthy.store(false, Ordering::Release);
    assert!(input.validate(&view, session).is_err());
    sync.replace_window(WorkspaceWindowId::new(), vec![]);
    assert!(!sync.is_armed());
    let mut unavailable = DesktopSyncInput::with_audit(DesktopSyncAudit::unavailable());
    let (target, _, _) = ready_target(window, TabGroupId::new());
    let tab = target.opened.tab_id;
    unavailable.replace_window(window, vec![target]);
    let review = unavailable
        .controller
        .review_arm(SyncInputSelection::Manual, &[tab])
        .unwrap_or_else(|error| panic!("review: {error}"));
    assert!(matches!(
        unavailable
            .controller
            .arm(review, SyncInputApproval::Explicit),
        Err(SyncInputError::Audit(_))
    ));
    assert!(!unavailable.is_armed());
}

#[test]
fn source_switch_and_focus_loss_clear_confirmation_and_headless_controls_fit_the_window() {
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("temp: {error}"));
    let mut sync = DesktopSyncInput::with_audit(audit_in(directory.path()));
    let window = WorkspaceWindowId::new();
    let group = TabGroupId::new();
    let (first, _, _) = ready_target(window, group);
    let (second, _, _) = ready_target(window, group);
    let tabs = [first.opened.tab_id, second.opened.tab_id];
    sync.replace_window(window, vec![first, second]);
    arm(&mut sync, &tabs);
    sync.route(
        tabs[0],
        InputAction::Paste {
            text: "line one\nline two".into(),
            bracketed: true,
        },
    );
    assert!(sync.pending.is_some());
    sync.source_changed(tabs[1]);
    assert!(sync.pending.is_none());
    assert!(sync.is_armed());
    let context = egui::Context::default();
    let mut remaining = egui::Rect::NOTHING;
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1024.0, 768.0),
            )),
            ..egui::RawInput::default()
        },
        |ui| {
            sync.draw(ui, window, group, Some(tabs[1]), false);
            remaining = ui.available_rect_before_wrap();
        },
    );
    assert!(!output.shapes.is_empty());
    output.textures_delta.clear();
    assert!(remaining.height() > 600.0);
    sync.focus_lost(Some(tabs[1]));
    assert!(!sync.is_armed());
    assert!(sync.pending.is_none());
}

#[test]
fn bounded_final_audit_rejection_reports_real_enqueue_and_revokes_unwritten_input() {
    let (sender, audit_receiver) = std::sync::mpsc::sync_channel(1);
    let audit = DesktopSyncAudit {
        sender: Some(sender),
        healthy: Arc::new(AtomicBool::new(true)),
        worker: None,
    };
    let mut sync = DesktopSyncInput::with_audit(audit);
    let window = WorkspaceWindowId::new();
    let (target, view, mut receiver) = ready_target(window, TabGroupId::new());
    let tab = target.opened.tab_id;
    let session = target.opened.session_id;
    sync.replace_window(window, vec![target]);
    arm(&mut sync, &[tab]);
    let armed_metadata = audit_receiver
        .try_recv()
        .unwrap_or_else(|error| panic!("arm metadata: {error}"));
    assert!(String::from_utf8_lossy(&armed_metadata).contains("armed"));
    // Planned metadata fills the one-record fixture queue; final delivery metadata
    // is rejected after the input queue actually accepted the action.
    assert!(sync.route(tab, InputAction::Text("AUDIT_COMPLETION_PROBE".into())));
    assert!(sync.status.starts_with("已入队 1/1"));
    assert!(sync.status.contains("审计未完成"));
    assert!(!sync.is_armed());
    let input = receiver
        .try_recv()
        .unwrap_or_else(|error| panic!("accepted queue item: {error}"));
    assert!(input.validate(&view, session).is_err());
}

#[test]
#[ignore = "owned PTY helper for native synchronized input gates"]
fn child_fixture() {
    use std::io::BufRead;
    let index: usize = std::env::var("SYNC_TEST_INDEX")
        .unwrap_or_else(|error| panic!("index: {error}"))
        .parse()
        .unwrap_or_else(|error| panic!("index value: {error}"));
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "SYNC_READY:{index:03}").unwrap_or_else(|error| panic!("ready: {error}"));
    stdout
        .flush()
        .unwrap_or_else(|error| panic!("flush: {error}"));
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap_or_else(|error| panic!("input: {error}"));
        let reversed: String = line.trim().chars().rev().collect();
        writeln!(stdout, "SYNC_ACK:{index:03}:{reversed}")
            .unwrap_or_else(|error| panic!("ack: {error}"));
        stdout
            .flush()
            .unwrap_or_else(|error| panic!("flush: {error}"));
    }
}

#[test]
fn broadcast_keys_preserve_release_unicode_and_target_specific_encoding_from_a_legacy_source() {
    use cshell_terminal::{InputEncoder, TerminalModes};
    use winit::keyboard::{Key, ModifiersState, NamedKey};
    let legacy = TerminalModes::default();
    let kitty = TerminalModes {
        kitty_keyboard_flags: TerminalModes::KITTY_DISAMBIGUATE
            | TerminalModes::KITTY_REPORT_ALL_KEYS
            | TerminalModes::KITTY_REPORT_EVENTS
            | TerminalModes::KITTY_REPORT_ASSOCIATED_TEXT,
        ..TerminalModes::default()
    };
    let key = Key::Character("é".into());
    let input = crate::DesktopKeyInput {
        key: &key,
        text: Some("e\u{301}"),
        pressed: true,
        repeated: false,
    };
    let action = crate::desktop_key_action(input, ModifiersState::empty(), legacy, true)
        .unwrap_or_else(|| panic!("logical Unicode key"));
    assert_eq!(
        InputEncoder::encode(&action, legacy).unwrap_or_else(|error| panic!("legacy: {error}")),
        "e\u{301}".as_bytes()
    );
    assert!(
        String::from_utf8_lossy(
            &InputEncoder::encode(&action, kitty).unwrap_or_else(|error| panic!("Kitty: {error}"))
        )
        .contains(";101:769u")
    );
    let released = crate::desktop_key_action(
        crate::DesktopKeyInput {
            key: &key,
            text: None,
            pressed: false,
            repeated: false,
        },
        ModifiersState::empty(),
        legacy,
        true,
    )
    .unwrap_or_else(|| panic!("logical release"));
    assert!(
        InputEncoder::encode(&released, legacy)
            .unwrap_or_else(|error| panic!("legacy release: {error}"))
            .is_empty()
    );
    assert!(
        String::from_utf8_lossy(
            &InputEncoder::encode(&released, kitty)
                .unwrap_or_else(|error| panic!("Kitty release: {error}"))
        )
        .contains(":3")
    );
    assert!(
        crate::desktop_key_action(
            crate::DesktopKeyInput {
                key: &key,
                text: None,
                pressed: false,
                repeated: false
            },
            ModifiersState::empty(),
            legacy,
            false
        )
        .is_none()
    );
    let arrow = Key::Named(NamedKey::ArrowUp);
    let arrow = crate::desktop_key_action(
        crate::DesktopKeyInput {
            key: &arrow,
            text: None,
            pressed: true,
            repeated: false,
        },
        ModifiersState::empty(),
        legacy,
        true,
    )
    .unwrap_or_else(|| panic!("logical arrow"));
    assert_eq!(
        InputEncoder::encode(&arrow, legacy).unwrap_or_else(|error| panic!("arrow: {error}")),
        b"\x1b[A"
    );
    assert_eq!(
        InputEncoder::encode(
            &arrow,
            TerminalModes {
                application_cursor: true,
                ..legacy
            }
        )
        .unwrap_or_else(|error| panic!("application cursor: {error}")),
        b"\x1bOA"
    );
}

struct Cleanup {
    registry: Arc<cshelld::LocalSessionRegistry>,
    shutdown: tokio::sync::watch::Sender<bool>,
    server: tokio::task::AbortHandle,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _sent = self.shutdown.send(true);
        self.server.abort();
        if let Ok(sessions) = self.registry.list() {
            for session in sessions {
                let _closed = self.registry.close(session.session_id);
            }
        }
    }
}

fn text(connection: &DesktopDaemonConnection) -> String {
    connection
        .view()
        .snapshot
        .map_or_else(String::new, |snapshot| {
            snapshot
                .cells
                .chunks(usize::from(snapshot.cols))
                .map(|row| {
                    row.iter()
                        .flat_map(cshell_terminal::Cell::characters)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
}

async fn native_sync(count: usize) {
    use cshell_ipc::{HandshakePolicy, features, transport};
    use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
    use cshelld::{LocalSessionRegistry, SessionIpcServer, SessionIpcService};
    let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("temp: {error}"));
    let registry = Arc::new(
        LocalSessionRegistry::new(directory.path().join("journals"), 256)
            .unwrap_or_else(|error| panic!("registry: {error}")),
    );
    #[cfg(windows)]
    let endpoint = format!(
        r"\\.\pipe\cshell-sync-native-{}-{}",
        std::process::id(),
        ConnectionId::new()
    );
    #[cfg(unix)]
    let endpoint = directory.path().join("sync.sock");
    let listener = transport::LocalListener::bind(&endpoint)
        .unwrap_or_else(|error| panic!("listener: {error}"));
    let runtime = cshell_ipc::RuntimePaths::prepare(directory.path().join("ipc-runtime"))
        .unwrap_or_else(|error| panic!("IPC runtime: {error}"));
    let discovery = cshell_ipc::DiscoveryRecord::generate(&runtime);
    let token = discovery.instance_token;
    let daemon_instance_id = discovery.daemon_instance_id;
    let server = Arc::new(SessionIpcServer::new(
        listener,
        HandshakePolicy::with_instance_id(
            token,
            daemon_instance_id,
            features::FULL_FRAME_RECOVERY
                | features::PRIORITY_STREAMS
                | features::TERMINAL_CONTROL
                | features::COMPOSE_INPUT
                | features::SSH_SESSION_STATUS,
        ),
        SessionIpcService::new(Arc::clone(&registry)),
    ));
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { server.run_until(receiver).await });
    let _cleanup = Cleanup {
        registry: Arc::clone(&registry),
        shutdown,
        server: task.abort_handle(),
    };
    let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
        endpoint,
        instance_token: token,
        daemon_instance_id,
    });
    let sync = Arc::new(Mutex::new(DesktopSyncInput::with_audit(audit_in(
        directory.path(),
    ))));
    let mut app = DesktopApp {
        sync_input: Some(Arc::clone(&sync)),
        daemon_config: Some(config.clone()),
        workspace_loaded: true,
        ..DesktopApp::default()
    };
    app.workspace.writable = true;
    let mut identities = Vec::new();
    for index in 0..count {
        let profile = LocalProfile {
            name: format!("sync native {index}"),
            program: std::env::current_exe().unwrap_or_else(|error| panic!("exe: {error}")),
            args: vec![
                "--ignored".into(),
                "--exact".into(),
                "sync_input::tests::child_fixture".into(),
                "--nocapture".into(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::from([("SYNC_TEST_INDEX".into(), index.to_string())]),
        };
        let attachment = registry
            .spawn_local(&profile, TerminalSize::cells(24, 100))
            .unwrap_or_else(|error| panic!("PTY {index}: {error}"));
        let session = attachment.session_id();
        drop(attachment);
        let tab = app
            .workspace
            .observe(session, None, format!("native-{index:03}"))
            .unwrap_or_else(|| panic!("tab {index}"));
        identities.push((tab, session));
    }
    let first = identities[0];
    app.workspace.select(first.0);
    app.current_workspace_tab = Some(first.0);
    app.daemon = Some(
        DesktopDaemonConnection::start_idle(config)
            .unwrap_or_else(|error| panic!("source worker: {error}")),
    );
    assert!(
        app.daemon
            .as_ref()
            .is_some_and(|connection| connection.attach_session(first.1))
    );
    // Unselected fresh sessions must become eligible without the old circular
    // dependency on already being in the selected set.
    assert!(sync.lock().unwrap().selected.is_empty());
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        app.refresh_sync_input();
        let ready = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .controller
            .port()
            .targets
            .iter()
            .all(|(tab, _)| sync_ready(&app, *tab));
        if ready && app.sync_daemons.len() + 1 == count {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native target workers did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    app.view_model.selected = Some(first.1);
    app.host_panel.open = true;
    assert!(app.sync_blocks_input());
    assert!(!app.submit_terminal_action(InputAction::Text("HOST_EDITOR_MUST_NOT_SEND".into())));
    app.host_panel.open = false;
    {
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        arm(
            &mut sync,
            &identities.iter().map(|(tab, _)| *tab).collect::<Vec<_>>(),
        );
    }
    assert!(app.submit_terminal_action(InputAction::Paste {
        text: "SYNC_PAYLOAD_1\r".into(),
        bracketed: true
    }));
    {
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prepared = sync
            .pending
            .take()
            .unwrap_or_else(|| panic!("native paste confirmation"));
        sync.dispatch(prepared, SyncInputApproval::Confirmed);
        assert!(
            sync.status.starts_with(&format!("已入队 {count}/{count}")),
            "{}",
            sync.status
        );
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let all = identities.iter().enumerate().all(|(index, (tab, _))| {
            let connection = if *tab == first.0 {
                app.daemon.as_ref()
            } else {
                app.sync_daemons.get(tab)
            };
            connection.is_some_and(|connection| {
                text(connection).contains(&format!("SYNC_ACK:{index:03}:1_DAOLYAP_CNYS"))
            })
        });
        if all {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{count} real PTYs did not acknowledge synchronized payload"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A reviewed current-only Compose must stay current-only even while Armed.
    {
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sync.prepare_compose(first.0, "COMPOSE_CURRENT".into(), true, false);
        let prepared = sync
            .pending_current
            .take()
            .unwrap_or_else(|| panic!("current Compose review"));
        sync.dispatch_current(prepared);
        assert!(sync.status.contains("已入队 1/1"), "{}", sync.status);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !app
        .daemon
        .as_ref()
        .is_some_and(|connection| text(connection).contains("SYNC_ACK:000:TNERRUC_ESOPMOC"))
    {
        assert!(
            Instant::now() < deadline,
            "current Compose was not acknowledged"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        app.sync_daemons
            .values()
            .all(|connection| !text(connection).contains("TNERRUC_ESOPMOC"))
    );
    {
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sync.prepare_compose(first.0, "COMPOSE_SELECTED".into(), true, true);
        let prepared = sync
            .pending
            .take()
            .unwrap_or_else(|| panic!("selected Compose review"));
        sync.dispatch(prepared, SyncInputApproval::Confirmed);
        assert!(
            sync.status.starts_with(&format!("已入队 {count}/{count}")),
            "{}",
            sync.status
        );
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let all = identities.iter().enumerate().all(|(index, (tab, _))| {
            let connection = if *tab == first.0 {
                app.daemon.as_ref()
            } else {
                app.sync_daemons.get(tab)
            };
            connection.is_some_and(|connection| {
                text(connection).contains(&format!("SYNC_ACK:{index:03}:DETCELES_ESOPMOC"))
            })
        });
        if all {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "selected Compose was not acknowledged by all targets"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Secure destination fails independently while the source still receives input.
    let second = identities[1];
    {
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sync.secure.insert(second.0);
    }
    app.refresh_sync_input();
    assert!(app.submit_terminal_action(InputAction::Text("SYNC_PAYLOAD_2".into())));
    assert!(app.submit_terminal_action(InputAction::Key(KeyEvent {
        code: KeyCode::Enter,
        modifiers: Modifiers::default(),
        pressed: true,
        repeated: false
    })));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !app
        .daemon
        .as_ref()
        .is_some_and(|connection| text(connection).contains("SYNC_ACK:000:2_DAOLYAP_CNYS"))
    {
        assert!(
            Instant::now() < deadline,
            "source did not acknowledge second input"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        app.sync_daemons
            .get(&second.0)
            .is_some_and(|connection| !text(connection).contains("2_DAOLYAP_CNYS"))
    );
    assert!(
        sync.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .contains("SecureInput")
    );
    registry
        .close(second.1)
        .unwrap_or_else(|error| panic!("close destination: {error}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while app
        .sync_daemons
        .get(&second.0)
        .is_some_and(|connection| connection.view().connected)
    {
        assert!(
            Instant::now() < deadline,
            "closed destination still connected"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    sync.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .secure
        .remove(&second.0);
    app.refresh_sync_input();
    assert!(app.submit_terminal_action(InputAction::Paste {
        text: "SYNC_PAYLOAD_3\r".into(),
        bracketed: true
    }));
    {
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prepared = sync
            .pending
            .take()
            .unwrap_or_else(|| panic!("third input review"));
        sync.dispatch(prepared, SyncInputApproval::Confirmed);
        assert!(
            sync.status.contains("Closed") || sync.status.contains("Disconnected"),
            "{}",
            sync.status
        );
        assert!(sync.is_armed());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !app
        .daemon
        .as_ref()
        .is_some_and(|connection| text(connection).contains("SYNC_ACK:000:3_DAOLYAP_CNYS"))
    {
        assert!(
            Instant::now() < deadline,
            "closed destination blocked source delivery"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    sync.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .focus_lost(Some(first.0));
    assert!(
        !sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_armed()
    );
    app.refresh_sync_input();
    {
        let mut sync = sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        arm(&mut sync, &[first.0]);
    }
    assert!(app.submit_terminal_action(InputAction::Paste {
        text: "PENDING_SOURCE_CLOSED\r".into(),
        bracketed: true
    }));
    registry
        .close(first.1)
        .unwrap_or_else(|error| panic!("close source: {error}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        app.refresh_sync_input();
        if !sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_armed()
        {
            break;
        }
        assert!(Instant::now() < deadline, "closed source did not disarm");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        sync.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .is_none()
    );
    assert_eq!(
        registry
            .list()
            .unwrap_or_else(|error| panic!("owned sessions: {error}"))
            .len(),
        count - 2
    );
    drop(app);
    drop(sync);
    eprintln!(
        "native synchronized input gate: {count} owned PTYs, authenticated IPC, desktop logical routing, exact paste review, secure/closed destination, closed source and focus-loss disarm passed"
    );
}

fn sync_ready(app: &DesktopApp, tab: TabId) -> bool {
    let connection = if Some(tab) == app.current_workspace_tab {
        app.daemon.as_ref()
    } else {
        app.sync_daemons.get(&tab)
    };
    connection.is_some_and(|connection| {
        connection.view().connected && text(connection).contains("SYNC_READY:")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_native_ptys_use_desktop_logical_broadcast_and_secure_destination_guard() {
    native_sync(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit 100 native PTY synchronized-input gate"]
async fn hundred_native_ptys_use_desktop_logical_broadcast_and_secure_destination_guard() {
    native_sync(100).await;
}
