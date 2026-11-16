use super::*;
use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};

#[test]
fn select_all_consumes_its_release_after_modifiers_change() {
    let mut tracked = None;
    let key = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyA);
    assert!(consume_select_all_key(
        &mut tracked,
        key,
        ElementState::Pressed,
        true
    ));
    assert!(consume_select_all_key(
        &mut tracked,
        key,
        ElementState::Released,
        false
    ));
    assert!(tracked.is_none());
    assert!(!consume_select_all_key(
        &mut tracked,
        key,
        ElementState::Pressed,
        false
    ));
}

#[tokio::test]
async fn paged_selection_rejects_wrong_session_gaps_retention_loss_and_cancellation() {
    use cshell_ipc::{
        Envelope, HandshakePolicy, LogPage, LogRow, envelope, read_envelope, server_handshake,
        transport, write_envelope,
    };
    for failure in [
        "session",
        "gap",
        "retained",
        "cancelled",
        "truncated",
        "limit",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        #[cfg(windows)]
        let endpoint = format!(
            r"\\.\pipe\cshell-all-failure-{}-{}",
            std::process::id(),
            session
        );
        #[cfg(unix)]
        let endpoint = temp.path().join("all.sock");
        let _ = &temp;
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        let config = DesktopConnectionConfig::history_test_config(
            crate::daemon_connection::ResolvedDesktopEndpoint {
                endpoint,
                instance_token: [81; 32],
                daemon_instance_id: [82; 16],
            },
            session,
        );
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id([81; 32], [82; 16], u64::MAX),
            )
            .await
            .unwrap();
            while let Ok(request) = read_envelope(&mut stream).await {
                assert!(matches!(
                    request.payload,
                    Some(envelope::Payload::LogPageRequest(_))
                ));
                let result = LogPage {
                    session_id: if failure == "session" {
                        SessionId::new().as_uuid().as_bytes().to_vec()
                    } else {
                        session.as_uuid().as_bytes().to_vec()
                    },
                    revision: 1,
                    first_line_id: if failure == "retained" { 2 } else { 1 },
                    anchor_line_id: if failure == "gap" || failure == "retained" {
                        2
                    } else {
                        1
                    },
                    total_line_count: 5,
                    has_before: false,
                    has_after: true,
                    rows: vec![LogRow {
                        line_id: if failure == "gap" || failure == "retained" {
                            2
                        } else {
                            1
                        },
                        text: "historical content".into(),
                        style_spans: vec![],
                        soft_wrap: false,
                        truncated: failure == "truncated",
                    }],
                };
                if write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: request.request_id,
                        payload: Some(envelope::Payload::LogPage(result)),
                        ..Default::default()
                    },
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        });
        let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(1, 20));
        terminal.feed(b"live");
        let mut snapshot = terminal.snapshot();
        snapshot.screen_start_line = 3;
        let capture = Capture {
            tab: None,
            session,
            epoch: cshell_domain::ConnectionId::new(),
            snapshot: Arc::new(snapshot),
            first: Some(1),
        };
        let mut text = String::new();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_capture(
                &config,
                &capture,
                &AtomicBool::new(failure == "cancelled"),
                if failure == "limit" { 4 } else { 1024 },
                |s| {
                    text.push_str(s);
                    Ok(())
                },
            ),
        )
        .await
        .unwrap();
        if failure == "limit" {
            assert!(matches!(result, Ok(ReadResult::TooLarge(1))));
        } else {
            assert!(result.is_err(), "must fail closed for {failure}");
        }
        assert!(
            text.is_empty(),
            "must not publish partial or wrong history for {failure}"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[test]
fn shortcut_help_blocks_terminal_control_routing() {
    let mut app = DesktopApp::default();
    let session = SessionId::new();
    let tab = app
        .workspace
        .add(None, "Connected terminal".into())
        .unwrap();
    app.current_workspace_tab = Some(tab);
    app.workspace.sessions.insert(tab, session);
    app.view_model.selected = Some(session);
    app.daemon = Some(DesktopDaemonConnection::for_view_test(
        crate::daemon_connection::DesktopDaemonView {
            connected: true,
            session_id: Some(session),
            ..Default::default()
        },
    ));
    assert!(app.terminal_keyboard_available());
    assert!(app.tab_navigation_available());
    app.view_model.shortcut_help_open = true;
    assert!(app.view_model.help_open());
    assert!(!app.terminal_keyboard_available());
    assert!(!app.tab_navigation_available());
    app.view_model.shortcut_help_open = false;
    assert!(app.terminal_keyboard_available());
    assert!(app.tab_navigation_available());
}

#[test]
fn assembling_rows_preserves_soft_wrap_spaces_real_newlines_and_limits() {
    let (mut started, mut continued, mut bytes) = (false, false, 0);
    let mut text = String::new();
    let mut write = |s: &str| {
        text.push_str(s);
        Ok(())
    };
    assert!(
        emit_row(
            "abc ",
            true,
            &mut started,
            &mut continued,
            &mut bytes,
            16,
            &mut write
        )
        .unwrap()
    );
    assert!(
        emit_row(
            "中文",
            false,
            &mut started,
            &mut continued,
            &mut bytes,
            16,
            &mut write
        )
        .unwrap()
    );
    assert!(
        emit_row(
            "next",
            false,
            &mut started,
            &mut continued,
            &mut bytes,
            16,
            &mut write
        )
        .unwrap()
    );
    assert!(
        !emit_row(
            "x",
            false,
            &mut started,
            &mut continued,
            &mut bytes,
            16,
            &mut write
        )
        .unwrap()
    );
    assert_eq!(text, "abc 中文\nnext");
}

#[test]
fn select_screen_is_local_and_does_not_automatically_copy_or_send_input() {
    let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(2, 20));
    terminal.feed("本地 /var/log".as_bytes());
    let mut app = DesktopApp::default();
    app.terminal_surface
        .submit_snapshot(Arc::new(terminal.snapshot()));
    app.select_terminal_screen();
    assert!(app.terminal_decorations.selection.is_some());
    assert!(app.terminal_clipboard.all.is_none());
    assert!(app.pending_paste.is_none() && app.daemon.is_none());
    assert!(
        app.terminal_selection_text()
            .unwrap()
            .unwrap()
            .contains("本地 /var/log")
    );
}

#[test]
fn select_all_shortcut_preserves_shell_ctrl_a_and_editor_shortcuts() {
    let a = Key::Character("a".into());
    assert!(!matches_terminal_select_all(
        &a,
        ModifiersState::CONTROL,
        false
    ));
    assert!(matches_terminal_select_all(
        &a,
        ModifiersState::CONTROL | ModifiersState::SHIFT,
        false
    ));
    assert!(matches_terminal_select_all(&a, ModifiersState::SUPER, true));
    assert!(!matches_terminal_select_all(
        &a,
        ModifiersState::SUPER,
        false
    ));
    assert!(!matches_terminal_select_all(
        &a,
        ModifiersState::CONTROL | ModifiersState::SHIFT | ModifiersState::ALT,
        false
    ));
}

#[tokio::test]
async fn paged_all_selection_is_complete_bounded_and_excludes_later_output() {
    use cshell_ipc::{
        Envelope, HandshakePolicy, LogPage, LogRow, envelope, read_envelope, server_handshake,
        transport, write_envelope,
    };
    let temp = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    #[cfg(windows)]
    let endpoint = format!(
        r"\\.\pipe\cshell-all-selection-{}-{}",
        std::process::id(),
        session
    );
    #[cfg(unix)]
    let endpoint = temp.path().join("all.sock");
    let _ = &temp;
    let listener = transport::LocalListener::bind(&endpoint).unwrap();
    let config = DesktopConnectionConfig::history_test_config(
        crate::daemon_connection::ResolvedDesktopEndpoint {
            endpoint,
            instance_token: [81; 32],
            daemon_instance_id: [82; 16],
        },
        session,
    );
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        server_handshake(
            &mut stream,
            &HandshakePolicy::with_instance_id([81; 32], [82; 16], u64::MAX),
        )
        .await
        .unwrap();
        let mut requests = 0;
        while let Ok(request) = read_envelope(&mut stream).await {
            let Some(envelope::Payload::LogPageRequest(page)) = request.payload else {
                panic!("history only; no terminal input");
            };
            assert!(page.terminal_history && page.rows_before == 0 && page.rows_after <= 255);
            let start = page.anchor_line_id.unwrap().max(500);
            let end = (start + u64::from(page.rows_after) + 1).min(1201);
            let rows = (start..end)
                .map(|line_id| LogRow {
                    line_id,
                    text: format!("row {line_id}"),
                    style_spans: vec![],
                    soft_wrap: false,
                    truncated: line_id == 512 && page.rows_after != 0,
                })
                .collect();
            let result = LogPage {
                session_id: page.session_id,
                revision: 100 + requests,
                first_line_id: 500,
                anchor_line_id: start,
                total_line_count: 1200,
                has_before: start > 500,
                has_after: end <= 1200,
                rows,
            };
            write_envelope(
                &mut stream,
                &Envelope {
                    request_id: request.request_id,
                    payload: Some(envelope::Payload::LogPage(result)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            requests += 1;
        }
        requests
    });
    let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(1, 20));
    terminal.feed(b"live");
    let mut snapshot = terminal.snapshot();
    snapshot.screen_start_line = 1001;
    let capture = Capture {
        tab: None,
        session,
        epoch: cshell_domain::ConnectionId::new(),
        snapshot: Arc::new(snapshot),
        first: None,
    };
    let mut text = String::new();
    let result = read_capture(
        &config,
        &capture,
        &AtomicBool::new(false),
        1024 * 1024,
        |s| {
            text.push_str(s);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, ReadResult::Complete(500)));
    let expected = (500..1001)
        .map(|i| format!("row {i}\n"))
        .collect::<String>()
        + "live";
    assert_eq!(text, expected);
    assert!(!text.contains("row 1001") && !text.contains("row 1200"));
    assert!(
        server.await.unwrap() >= 3,
        "a truncated row is retried with a full per-row budget"
    );
}
