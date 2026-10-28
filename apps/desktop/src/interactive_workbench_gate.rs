//! Real PTY/IPC workbench logic gate. Native presentation and OS clipboard are separate gates.
use super::*;
use crate::daemon_connection::ResolvedDesktopEndpoint;
use cshell_domain::{SplitAxis, TabId, TerminalSize};
use cshell_ipc::{HandshakePolicy, features, transport};
use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
use cshelld::{LocalSessionRegistry, SessionIpcServer, SessionIpcService};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

const SESSION_COUNT: usize = 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_TEST: &str = "interactive_workbench_gate::child_fixture";
const CHILD_INDEX: &str = "WORKBENCH_TEST_INDEX";

// Only the owned PTY launches this ignored test, with its index in the child environment.
#[test]
#[ignore = "helper process for the real twenty-session workbench gate"]
fn child_fixture() {
    let index: usize = std::env::var(CHILD_INDEX)
        .unwrap_or_else(|e| panic!("child identity: {e}"))
        .parse()
        .unwrap_or_else(|e| panic!("child index: {e}"));
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    writeln!(output, "READY:{index:02}").unwrap_or_else(|e| panic!("ready: {e}"));
    output.flush().unwrap_or_else(|e| panic!("flush: {e}"));
    let mut line = String::new();
    loop {
        line.clear();
        if input
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("input: {e}"))
            == 0
        {
            return;
        }
        assert!(
            line.trim().starts_with(&format!("s{index:02}:")),
            "wrong input target"
        );
        let reversed: String = line.trim().chars().rev().collect();
        // Two matches exercise search navigation; transformed text excludes a PTY echo false positive.
        for _ in 0..2 {
            writeln!(output, "\x1b[32mACK:{index:02}:{reversed}\x1b[0m")
                .unwrap_or_else(|e| panic!("response: {e}"));
        }
        output.flush().unwrap_or_else(|e| panic!("flush: {e}"));
    }
}

struct Cleanup {
    registry: Arc<LocalSessionRegistry>,
    shutdown: tokio::sync::watch::Sender<bool>,
    server: tokio::task::AbortHandle,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.server.abort();
        if let Ok(sessions) = self.registry.list() {
            for session in sessions {
                let _ = self.registry.close(session.session_id);
            }
        }
    }
}

fn tick(app: &mut DesktopApp) {
    app.sync_workspace();
    app.poll_terminal_search();
    app.poll_primary_terminal(Instant::now());
    let generation = app.terminal_surface.latest_snapshot().map(|s| s.generation);
    if app.terminal_search_open
        && !app.terminal_search_query.is_empty()
        && generation != app.terminal_search_requested_generation
    {
        app.schedule_terminal_search(false);
    }
}

async fn wait_for(app: &mut DesktopApp, label: &str, ready: impl Fn(&DesktopApp) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        tick(app);
        if ready(app) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: tab={:?}, daemon={:?}, search={:?}",
            app.current_workspace_tab,
            app.daemon.as_ref().map(DesktopDaemonConnection::view),
            app.terminal_search_error,
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn snapshot_text(snapshot: &cshell_terminal::FrameSnapshot) -> String {
    snapshot
        .cells
        .chunks(usize::from(snapshot.cols))
        .map(|row| row.iter().map(|cell| cell.character).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_marker(app: &DesktopApp, marker: &str) -> bool {
    app.terminal_surface
        .latest_snapshot()
        .is_some_and(|s| snapshot_text(s).contains(marker))
}

fn marker_selection(app: &DesktopApp, marker: &str) -> TerminalSelection {
    let snapshot = app
        .terminal_surface
        .latest_snapshot()
        .unwrap_or_else(|| panic!("snapshot"));
    for (row, cells) in snapshot
        .cells
        .chunks(usize::from(snapshot.cols))
        .enumerate()
    {
        let text: String = cells.iter().map(|cell| cell.character).collect();
        if let Some(column) = text.find(marker) {
            return TerminalSelection {
                anchor: TerminalCellPoint {
                    row: row as u16,
                    column: column as u16,
                },
                focus: TerminalCellPoint {
                    row: row as u16,
                    column: (column + marker.len() - 1) as u16,
                },
                mode: TerminalSelectionMode::Character,
            };
        }
    }
    panic!("marker missing from visible snapshot: {marker}");
}

fn assert_tab_preferences(app: &DesktopApp, tab: TabId, session: SessionId, marker: &str) {
    assert_eq!(app.current_workspace_tab, Some(tab));
    assert_eq!(app.view_model.selected, Some(session));
    assert_eq!(app.terminal_search_query, marker);
    assert!(app.terminal_search_open);
    assert_eq!(
        app.terminal_search_options,
        cshell_render::TerminalSearchOptions {
            case_sensitive: true,
            whole_word: false,
            regex: false,
        }
    );
    assert_eq!(
        app.terminal_selection_text()
            .and_then(Result::ok)
            .as_deref(),
        Some(marker)
    );
}

fn assert_tab_state(app: &DesktopApp, tab: TabId, session: SessionId, marker: &str) {
    assert_tab_preferences(app, tab, session, marker);
    assert_eq!(app.terminal_decorations.search.matches.len(), 2);
    assert_eq!(app.terminal_decorations.active_search_match, Some(1));
    let snapshot = app
        .terminal_surface
        .latest_snapshot()
        .unwrap_or_else(|| panic!("search snapshot"));
    for matched in app.terminal_decorations.search.matches.iter() {
        assert_eq!(
            TerminalSelection {
                anchor: matched.start,
                focus: matched.end,
                mode: TerminalSelectionMode::Character,
            }
            .text(snapshot)
            .unwrap_or_else(|e| panic!("match text: {e}")),
            marker
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_live_tabs_preserve_copy_search_split_and_reconnect_state() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();
    let directory = tempfile::tempdir().unwrap_or_else(|e| panic!("temporary runtime: {e}"));
    let registry = Arc::new(
        LocalSessionRegistry::new(directory.path().join("journals"), 32)
            .unwrap_or_else(|e| panic!("registry: {e}")),
    );
    let mut sessions = Vec::new();
    for index in 0..SESSION_COUNT {
        let profile = LocalProfile {
            name: format!("Workbench {index:02}"),
            program: std::env::current_exe().unwrap_or_else(|e| panic!("test binary: {e}")),
            args: vec![
                "--ignored".into(),
                "--exact".into(),
                CHILD_TEST.into(),
                "--nocapture".into(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::from([(CHILD_INDEX.into(), index.to_string())]),
        };
        sessions.push(
            registry
                .spawn_local(&profile, TerminalSize::cells(16, 100))
                .unwrap_or_else(|e| panic!("PTY {index}: {e}"))
                .session_id(),
        );
    }
    assert_eq!(
        sessions.iter().collect::<BTreeSet<_>>().len(),
        SESSION_COUNT
    );
    #[cfg(windows)]
    let endpoint = format!(
        r"\\.\pipe\cshell-workbench-{}-{}",
        std::process::id(),
        sessions[0]
    );
    #[cfg(unix)]
    let endpoint = directory.path().join("workbench.sock");
    let listener = transport::LocalListener::bind(&endpoint).unwrap_or_else(|e| panic!("IPC: {e}"));
    let mut token = [0; 32];
    token[..16].copy_from_slice(SessionId::new().as_uuid().as_bytes());
    token[16..].copy_from_slice(SessionId::new().as_uuid().as_bytes());
    let instance = *SessionId::new().as_uuid().as_bytes();
    let server = Arc::new(SessionIpcServer::new(
        listener,
        HandshakePolicy::with_instance_id(
            token,
            instance,
            features::FULL_FRAME_RECOVERY
                | features::PRIORITY_STREAMS
                | features::LOG_PAGING
                | features::TERMINAL_CONTROL
                | features::HISTORY_SEARCH
                | features::SSH_SESSION_STATUS
                | features::LOCAL_LAUNCH_OPTIONS,
        ),
        SessionIpcService::new(Arc::clone(&registry)),
    ));
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run_until(stopped).await })
    };
    let cleanup = Cleanup {
        registry: Arc::clone(&registry),
        shutdown,
        server: task.abort_handle(),
    };
    let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
        endpoint,
        instance_token: token,
        daemon_instance_id: instance,
    });
    let mut app = DesktopApp {
        workspace_loaded: true,
        daemon_config: Some(config),
        terminal_search: Some(
            TerminalSearchWorker::start().unwrap_or_else(|e| panic!("search: {e}")),
        ),
        ..Default::default()
    };
    app.workspace.writable = true;
    let tabs: Vec<_> = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let tab = app
                .workspace
                .add(None, format!("Workbench {index:02}"))
                .unwrap_or_else(|| panic!("tab limit"));
            app.workspace.sessions.insert(tab, *session);
            tab
        })
        .collect();
    let mut markers = Vec::new();
    for (index, (&tab, &session)) in tabs.iter().zip(&sessions).enumerate() {
        app.activate_workspace_tab(tab);
        wait_for(&mut app, "attach", |app| {
            app.active_terminal_session_id() == Some(session)
                && has_marker(app, &format!("READY:{index:02}"))
        })
        .await;
        if index == 0 {
            assert!(app.terminal_keyboard_available());
            app.view_model.tab_navigator.show(Some(tab));
            assert!(!app.terminal_keyboard_available(), "tab search must isolate all terminal input");
            assert!(!app.terminal_wheel_available(1.0));
            app.view_model.tab_navigator.close();
            app.return_focus_from_tab_navigation();
            assert!(app.terminal_keyboard_available(), "closing search must restore the current terminal");
        }
        let nonce = format!("s{index:02}:{}", session.as_uuid().simple());
        let marker = format!("ACK:{index:02}:{}", nonce.chars().rev().collect::<String>());
        let rows = 16 + index as u16 % 3;
        let cols = 100 + index as u16 % 5;
        let daemon = app
            .daemon
            .as_ref()
            .unwrap_or_else(|| panic!("active client"));
        assert!(daemon.request_resize(rows, cols, 0, 0));
        assert!(daemon.send_input(InputAction::Paste {
            text: format!("{nonce}\r"),
            bracketed: false
        }));
        wait_for(&mut app, "input and resize", |app| {
            has_marker(app, &marker)
                && app
                    .terminal_surface
                    .latest_snapshot()
                    .is_some_and(|s| s.rows == rows && s.cols == cols)
        })
        .await;
        let text = snapshot_text(
            app.terminal_surface
                .latest_snapshot()
                .unwrap_or_else(|| panic!("output")),
        );
        for other in 0..SESSION_COUNT {
            if other != index {
                assert!(
                    !text.contains(&format!("ACK:{other:02}:")),
                    "cross-session output"
                );
            }
        }
        app.terminal_decorations.selection = Some(marker_selection(&app, &marker));
        assert_eq!(
            app.terminal_selection_text()
                .and_then(Result::ok)
                .as_deref(),
            Some(marker.as_str())
        );
        app.terminal_search_open = true;
        app.terminal_search_query.clone_from(&marker);
        app.terminal_search_options.case_sensitive = true;
        app.schedule_terminal_search(true);
        wait_for(&mut app, "two search matches", |app| {
            app.terminal_decorations.search.matches.len() == 2
        })
        .await;
        app.navigate_terminal_search(1);
        assert_tab_state(&app, tab, session, &marker);
        markers.push(marker);
    }
    // Leave a search request in flight while switching tabs; cached state must remain local to each tab.
    app.schedule_terminal_search(false);
    for index in (0..SESSION_COUNT).rev() {
        app.activate_workspace_tab(tabs[index]);
        assert_tab_preferences(&app, tabs[index], sessions[index], &markers[index]);
        wait_for(&mut app, "cached tab reconnect", |app| {
            app.active_terminal_session_id() == Some(sessions[index])
                && has_marker(app, &markers[index])
                && app.terminal_decorations.search.matches.len() == 2
        })
        .await;
        assert_tab_state(&app, tabs[index], sessions[index], &markers[index]);
    }
    // Hidden tabs retain the same control attachment and report unread output.
    app.window_active = true;
    wait_for(&mut app, "hidden attachment ready", |app| app.pane_daemons.get(&tabs[1]).is_some_and(|c| c.view().connected)).await;
    let epoch = app.pane_daemons[&tabs[1]].view().input_epoch;
    app.activate_workspace_tab(tabs[1]); tick(&mut app);
    assert_eq!(app.daemon.as_ref().unwrap().view().input_epoch, epoch);
    app.activate_workspace_tab(tabs[0]); tick(&mut app);
    assert_eq!(app.pane_daemons[&tabs[1]].view().input_epoch, epoch, "switching must not rebuild the hidden attachment");
    assert!(app.pane_daemons[&tabs[1]].send_input(InputAction::Paste { text: "s01:background-activity\r".into(), bracketed: false }));
    wait_for(&mut app, "background output red dot", |app| app.view_model.unread_output_tabs.contains(&tabs[1])).await;
    assert!(!app.view_model.unread_output_tabs.contains(&tabs[0]));
    app.activate_workspace_tab(tabs[1]); tick(&mut app);
    assert!(!app.view_model.unread_output_tabs.contains(&tabs[1]));

    // Reconnect all desktop IPC workers while keeping the owned daemon and all PTYs alive.
    drop(app.daemon.take());
    drop(std::mem::take(&mut app.pane_daemons));
    for index in (0..SESSION_COUNT).rev() {
        app.activate_workspace_tab(tabs[index]);
        wait_for(&mut app, "worker restart", |app| {
            app.active_terminal_session_id() == Some(sessions[index])
                && has_marker(app, &markers[index])
                && app.terminal_decorations.search.matches.len() == 2
        })
        .await;
        assert_tab_state(&app, tabs[index], sessions[index], &markers[index]);
    }
    // Split production workspace layout and move existing live tabs into four visible panes.
    for (index, axis) in [
        (1, SplitAxis::Horizontal),
        (2, SplitAxis::Vertical),
        (3, SplitAxis::Horizontal),
    ] {
        let placeholder = app.workspace.split(axis).unwrap_or_else(|| panic!("split"));
        let group = app.workspace.focused_group();
        app.workspace.move_to_group(tabs[index], group);
        app.workspace.remove(placeholder);
        app.activate_workspace_tab(tabs[index]);
    }
    app.refresh_workspace_tabs();
    let visible = app.workspace.visible_tabs();
    assert_eq!(visible.len(), 4);
    wait_for(&mut app, "four independent pane snapshots", |app| {
        visible.iter().all(|tab| {
            let index = tabs
                .iter()
                .position(|id| id == tab)
                .unwrap_or_else(|| panic!("visible tab"));
            if Some(*tab) == app.current_workspace_tab {
                app.active_terminal_session_id() == Some(sessions[index])
                    && has_marker(app, &markers[index])
            } else {
                app.pane_daemons.get(tab).is_some_and(|c| {
                    c.view().connected && c.view().session_id == Some(sessions[index])
                }) && app
                    .cached_tab_views
                    .get(tab)
                    .and_then(|v| v.surface.latest_snapshot())
                    .is_some_and(|s| snapshot_text(s).contains(&markers[index]))
            }
        })
    })
    .await;
    // Focus each live pane and submit a second nonce through the foreground client.
    for &tab in &visible {
        let index = tabs
            .iter()
            .position(|id| *id == tab)
            .unwrap_or_else(|| panic!("pane tab"));
        app.activate_workspace_tab(tab);
        wait_for(&mut app, "pane focus", |app| {
            app.active_terminal_session_id() == Some(sessions[index])
        })
        .await;
        let nonce = format!("s{index:02}:split:{}", sessions[index].as_uuid().simple());
        let marker = format!("ACK:{index:02}:{}", nonce.chars().rev().collect::<String>());
        assert!(
            app.daemon
                .as_ref()
                .unwrap_or_else(|| panic!("pane client"))
                .send_input(InputAction::Paste {
                    text: format!("{nonce}\r"),
                    bracketed: false
                })
        );
        wait_for(&mut app, "focused pane input", |app| {
            has_marker(app, &marker)
        })
        .await;
        for (&other, connection) in &app.pane_daemons {
            assert_ne!(other, tab);
            assert!(
                connection
                    .view()
                    .snapshot
                    .as_deref()
                    .is_some_and(|s| !snapshot_text(s).contains(&marker))
            );
        }
    }
    let context = egui::Context::default();
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        },
        |ui| {
            let _ = cshell_ui::draw_workbench(ui, &mut app.view_model);
        },
    );
    output.textures_delta.clear();
    assert!(!output.shapes.is_empty());
    assert_eq!(app.view_model.pane_views.len(), 4);
    for pane in &app.view_model.pane_views {
        assert!(pane.rect.width() > 0.0 && pane.rect.height() > 0.0);
        assert!(pane.tab_id.is_some_and(|tab| visible.contains(&tab)));
    }
    assert!(cshell_application::validate_workspace(&app.workspace.document).is_ok());
    assert_eq!(app.workspace.sessions.len(), SESSION_COUNT);
    // Closing a workspace tab explicitly terminates its PTY even with KeepAlive.
    for &tab in &tabs {
        app.activate_workspace_tab(tab);
        let session = app.workspace.sessions[&tab];
        wait_for(&mut app, "close target attachment", |app| {
            app.active_terminal_session_id() == Some(session)
        })
        .await;
        app.close_workspace_tab(tab);
        wait_for(&mut app, "GUI close acknowledgement", |app| {
            !app.workspace.sessions.contains_key(&tab)
        })
        .await;
        assert!(!registry.list().unwrap().iter().any(|entry| entry.session_id == session));
    }
    assert!(app.workspace.document.bindings.is_empty());
    let live: BTreeSet<_> = registry
        .list()
        .unwrap_or_else(|e| panic!("live sessions: {e}"))
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    assert!(live.is_empty(), "closed tabs must not leave retained processes");
    drop(app);
    assert!(
        registry
            .list()
            .unwrap_or_else(|e| panic!("closed sessions: {e}"))
            .is_empty()
    );
    let _ = cleanup.shutdown.send(true);
    let stats = tokio::time::timeout(TIMEOUT, task)
        .await
        .unwrap_or_else(|e| panic!("server timeout: {e}"))
        .unwrap_or_else(|e| panic!("server join: {e}"))
        .unwrap_or_else(|e| panic!("server result: {e}"));
    assert_eq!(stats.rejected_connections, 0);
    println!(
        "{}",
        serde_json::json!({
            "version": 1, "gate": "interactive_workbench", "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH, "sessions": SESSION_COUNT,
            "copy_checks": SESSION_COUNT, "search_checks": SESSION_COUNT,
            "cached_tab_checks": SESSION_COUNT, "reconnect_checks": SESSION_COUNT,
        "visible_panes": 4, "terminating_tab_closes": SESSION_COUNT,
        "focused_pane_input_checks": 4,
            "closed_processes": SESSION_COUNT, "native_presentation_measured": false,
            "os_clipboard_measured": false,
        })
    );
}
