#![allow(clippy::unwrap_used)]
use super::*;
#[test]
fn old_settings_default_to_sftp_immediate_upload_and_success_auto_close() {
    let settings: zmodem_settings::Settings =
        serde_json::from_str(r#"{"download_directory":"D:/saved"}"#).unwrap();
    assert_eq!(settings.terminal_upload_protocol, Protocol::Sftp);
    assert!(!settings.confirm_terminal_upload);
    assert!(settings.close_successful_upload);
    assert_eq!(settings.download_directory, "D:/saved");
}

fn retry_plan(id: SessionId, row: &Row) -> RetryPlan {
    let snapshot = row.snapshot.as_ref().unwrap();
    RetryPlan {
        id,
        source: row.source,
        revision: 7,
        host: snapshot.host.clone(),
        directory: snapshot.directory.clone(),
        completed_files: 0,
        retry_files: 1,
        can_retry: true,
        detail: "只重试未完成文件".into(),
    }
}

#[test]
fn retry_requires_review_and_confirmation_and_cannot_be_submitted_twice_or_removed_in_flight() {
    let (id, mut row) = task(Some(State::Failed), Protocol::Sftp);
    let source = row.source;
    let plan = retry_plan(id, &row);
    row.accept(id, true, &serde_json::to_vec(&plan).unwrap())
        .unwrap();
    let (mut panel, mut receiver) = panel_with_task(id, row);
    let ctx = crate::ui_layout_gate::context();
    let pos = task_button(&ctx, &mut panel, "确认重试");
    click_task_button(&ctx, &mut panel, pos);
    let request = receiver.try_recv().unwrap();
    assert_eq!(request.operation, Op::TerminalUploadRetry as i32);
    assert_eq!(request.session_id, source.as_uuid().as_bytes());
    assert_eq!(request.transfer_id, id.as_uuid().as_bytes());
    assert_eq!(request.expected_revision, 7);
    assert!(request.confirmed && request.document_json.is_empty());
    panel.retry(source, id, 7);
    panel.remove_finished(source, id);
    assert!(receiver.try_recv().is_err());
    assert!(
        panel
            .connection
            .as_ref()
            .unwrap()
            .shared
            .lock()
            .unwrap()
            .rows
            .contains_key(&id)
    );
}

#[test]
fn retry_review_rejects_changed_target_and_new_attempt_clears_finished_timer() {
    let (id, mut row) = task(Some(State::Failed), Protocol::Sftp);
    let mut plan = retry_plan(id, &row);
    plan.directory = "/other".into();
    assert!(
        row.accept(id, true, &serde_json::to_vec(&plan).unwrap())
            .is_err()
    );
    plan = retry_plan(id, &row);
    plan.source = SessionId::new();
    assert!(
        row.accept(id, true, &serde_json::to_vec(&plan).unwrap())
            .is_err()
    );
    plan = retry_plan(id, &row);
    row.accept(id, true, &serde_json::to_vec(&plan).unwrap())
        .unwrap();
    let mut snapshot = row.snapshot.clone().unwrap();
    snapshot.state = State::Running;
    row.accept(id, false, &serde_json::to_vec(&snapshot).unwrap())
        .unwrap();
    assert!(row.finished_at.is_none() && row.retry_plan.is_none());
}

#[test]
fn old_poll_receipt_cannot_unlock_an_enqueued_retry_or_remove_its_record() {
    let (id, mut row) = task(Some(State::Failed), Protocol::Sftp);
    let source = row.source;
    let plan = retry_plan(id, &row);
    row.accept(id, true, &serde_json::to_vec(&plan).unwrap())
        .unwrap();
    let (mut panel, mut receiver) = panel_with_task(id, row);
    panel.retry(source, id, plan.revision);
    assert_eq!(
        receiver.try_recv().unwrap().operation,
        Op::TerminalUploadRetry as i32
    );
    let shared = panel.connection.as_ref().unwrap().shared.clone();
    {
        let mut view = shared.lock().unwrap();
        let row = view.rows.get_mut(&id).unwrap();
        // A query issued before the retry can still report the old failed state.
        let old = serde_json::to_vec(row.snapshot.as_ref().unwrap()).unwrap();
        row.pending = false;
        row.accept(id, false, &old).unwrap();
        row.record_response(Some(Op::TerminalUploadGet), Some(SftpStatus::Ok), true);
        assert!(row.retry_request == RetryRequestState::AwaitingReply);
        row.retry_plan = Some(plan);
    }
    panel.retry(source, id, 7);
    panel.remove_finished(source, id);
    assert!(receiver.try_recv().is_err());
    assert!(shared.lock().unwrap().rows.contains_key(&id));
}

#[test]
fn uncertain_retry_requires_an_authoritative_status_before_removal_or_resubmission() {
    let (id, mut row) = task(Some(State::Failed), Protocol::Sftp);
    let source = row.source;
    row.retry_request = RetryRequestState::AwaitingReply;
    row.record_response(Some(Op::TerminalUploadRetry), None, false);
    assert!(row.retry_request == RetryRequestState::Unknown);
    let (mut panel, mut receiver) = panel_with_task(id, row);
    panel.remove_finished(source, id);
    let shared = panel.connection.as_ref().unwrap().shared.clone();
    assert!(shared.lock().unwrap().rows.contains_key(&id));
    {
        let mut view = shared.lock().unwrap();
        let row = view.rows.get_mut(&id).unwrap();
        row.record_response(
            Some(Op::TerminalUploadRetryInspect),
            Some(SftpStatus::Ok),
            true,
        );
        assert!(row.retry_request == RetryRequestState::Unknown);
        row.record_response(Some(Op::TerminalUploadGet), None, false);
        assert!(row.retry_request == RetryRequestState::Unknown);
        let status = serde_json::to_vec(row.snapshot.as_ref().unwrap()).unwrap();
        row.accept(id, false, &status).unwrap();
        row.record_response(Some(Op::TerminalUploadGet), Some(SftpStatus::Ok), true);
        assert!(row.retry_request == RetryRequestState::Idle);
        assert!(
            row.retry_plan.is_none(),
            "fresh inspection is required for another retry"
        );
    }
    panel.remove_finished(source, id);
    assert!(shared.lock().unwrap().rows.is_empty());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn rejected_or_expired_retry_can_be_removed_but_malformed_success_stays_uncertain() {
    let (id, mut row) = task(Some(State::Failed), Protocol::Sftp);
    row.retry_request = RetryRequestState::AwaitingReply;
    row.record_response(Some(Op::TerminalUploadRetry), Some(SftpStatus::Ok), false);
    assert!(row.retry_request == RetryRequestState::Unknown);
    row.record_response(Some(Op::TerminalUploadGet), Some(SftpStatus::Ok), false);
    assert!(row.retry_request == RetryRequestState::Unknown);
    row.record_response(
        Some(Op::TerminalUploadGet),
        Some(SftpStatus::Unavailable),
        false,
    );
    assert!(row.retry_request == RetryRequestState::Idle);
    row.retry_request = RetryRequestState::AwaitingReply;
    row.record_response(
        Some(Op::TerminalUploadRetry),
        Some(SftpStatus::Conflict),
        false,
    );
    assert!(row.retry_request == RetryRequestState::Idle);
    let source = row.source;
    let (mut panel, mut receiver) = panel_with_task(id, row);
    panel.remove_finished(source, id);
    assert!(
        panel
            .connection
            .as_ref()
            .unwrap()
            .shared
            .lock()
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(receiver.try_recv().is_err());
}
#[test]
fn upload_progress_and_failure_fit_small_windows_with_real_fonts() {
    use crate::ui_layout_gate::{check_window, context, frame};
    for size in [egui::vec2(900.0, 600.0), egui::vec2(720.0, 480.0)] {
        let context = context();
        let id = SessionId::new();
        let source = SessionId::new();
        let snapshot = Snapshot {
            id,
            source,
            host: "developer@10.0.0.12:22".into(),
            directory: "/home/developer/projects/测试目录".into(),
            protocol: Protocol::Sftp,
            state: State::Failed,
            bytes: 1024,
            total_bytes: 2048,
            completed: 1,
            total_files: 2,
            current_name: "application.tar.gz".into(),
            bytes_per_second: 1024,
            detail: "目标存在同名文件，未覆盖。请检查目标目录。".into(),
        };
        let mut view = View::default();
        view.rows.insert(
            id,
            Row {
                source,
                route: None,
                snapshot: Some(snapshot),
                retry_plan: None,
                retry_request: RetryRequestState::Idle,
                start_rejected: false,
                error: None,
                finished_at: Some(Instant::now()),
                auto_close: true,
                pending: false,
            },
        );
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let (stop, _stopped) = tokio::sync::watch::channel(false);
        let mut panel = Panel {
            open: true,
            connection: Some(Connection {
                shared: Arc::new(Mutex::new(view)),
                sender,
                stop,
            }),
            ..Default::default()
        };
        let mut draw = |c: &egui::Context| panel.draw(c);
        for _ in 0..2 {
            frame(&context, size, &mut draw);
        }
        let tree = frame(&context, size, &mut draw);
        check_window(&context, size, "上传任务", &tree);
        assert!(panel.open, "failure must remain visible");
    }
}

#[test]
fn success_auto_hides_but_failure_cancel_and_opt_out_remain_visible() {
    for (state, auto_close, visible) in [
        (State::Succeeded, true, false),
        (State::Succeeded, false, true),
        (State::Failed, true, true),
        (State::Cancelled, true, true),
    ] {
        let id = SessionId::new();
        let source = SessionId::new();
        let mut rows = BTreeMap::new();
        rows.insert(
            id,
            Row {
                source,
                route: None,
                snapshot: Some(Snapshot {
                    id,
                    source,
                    host: "test@host:22".into(),
                    directory: "/srv/test".into(),
                    protocol: Protocol::Sftp,
                    state,
                    bytes: 1,
                    total_bytes: 1,
                    completed: 1,
                    total_files: 1,
                    current_name: String::new(),
                    bytes_per_second: 0,
                    detail: "测试结果".into(),
                }),
                retry_plan: None,
                retry_request: RetryRequestState::Idle,
                start_rejected: false,
                error: None,
                finished_at: Some(Instant::now() - Duration::from_secs(3)),
                auto_close,
                pending: false,
            },
        );
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let (stop, _) = tokio::sync::watch::channel(false);
        let mut panel = Panel {
            open: true,
            connection: Some(Connection {
                shared: Arc::new(Mutex::new(View {
                    generation: 1,
                    rows,
                })),
                sender,
                stop,
            }),
            ..Default::default()
        };
        let context = crate::ui_layout_gate::context();
        crate::ui_layout_gate::frame(&context, egui::vec2(900.0, 600.0), |c| panel.draw(c));
        assert_eq!(panel.open, visible);
        assert!(
            receiver.try_recv().is_err(),
            "closing progress must not send cancellation"
        );
    }
}
#[test]
fn failure_reopens_once_after_user_hides_progress() {
    let id = SessionId::new();
    let source = SessionId::new();
    let mut rows = BTreeMap::new();
    rows.insert(
        id,
        Row {
            source,
            route: None,
            snapshot: None,
            retry_plan: None,
            retry_request: RetryRequestState::Idle,
            start_rejected: false,
            error: Some("无法确认上传结果".into()),
            finished_at: None,
            auto_close: true,
            pending: false,
        },
    );
    let (sender, _) = tokio::sync::mpsc::channel(1);
    let (stop, _) = tokio::sync::watch::channel(false);
    let shared = Arc::new(Mutex::new(View {
        generation: 1,
        rows,
    }));
    let mut panel = Panel {
        connection: Some(Connection {
            shared: shared.clone(),
            sender,
            stop,
        }),
        ..Default::default()
    };
    assert!(panel.poll());
    assert!(panel.open);
    panel.open = false;
    shared.lock().unwrap().generation += 1;
    assert!(panel.poll());
    assert!(!panel.open);
}

fn task(state: Option<State>, protocol: Protocol) -> (SessionId, Row) {
    let id = SessionId::new();
    let source = SessionId::new();
    (
        id,
        Row {
            source,
            route: None,
            snapshot: state.map(|state| Snapshot {
                id,
                source,
                host: "test@host:22".into(),
                directory: "/srv/test".into(),
                protocol,
                state,
                bytes: 1,
                total_bytes: 2,
                completed: 0,
                total_files: 1,
                current_name: "upload.txt".into(),
                bytes_per_second: 0,
                detail: "上传状态".into(),
            }),
            retry_plan: None,
            retry_request: RetryRequestState::Idle,
            start_rejected: false,
            error: None,
            finished_at: state.filter(|s| s.finished()).map(|_| Instant::now()),
            auto_close: false,
            pending: false,
        },
    )
}

fn panel_with_task(id: SessionId, row: Row) -> (Panel, tokio::sync::mpsc::Receiver<SftpRequest>) {
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let (stop, _) = tokio::sync::watch::channel(false);
    (
        Panel {
            open: true,
            connection: Some(Connection {
                shared: Arc::new(Mutex::new(View {
                    generation: 1,
                    rows: BTreeMap::from([(id, row)]),
                })),
                sender,
                stop,
            }),
            ..Default::default()
        },
        receiver,
    )
}

fn task_button(ctx: &egui::Context, panel: &mut Panel, label: &str) -> egui::Pos2 {
    let size = egui::vec2(720.0, 480.0);
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    for _ in 0..3 {
        crate::ui_layout_gate::frame(ctx, size, |c| panel.draw(c));
    }
    let tree = crate::ui_layout_gate::frame(ctx, size, |c| panel.draw(c));
    let node = tree
        .nodes
        .iter()
        .find(|(_, n)| n.role() == egui::accesskit::Role::Button && n.label() == Some(label))
        .unwrap_or_else(|| panic!("missing task action: {label}"));
    let rect = crate::ui_layout_gate::bounds(&node.1);
    assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(rect));
    rect.center()
}

fn click_task_button(ctx: &egui::Context, panel: &mut Panel, pos: egui::Pos2) {
    for pressed in [true, false] {
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 480.0),
                )),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: Default::default(),
                    },
                ],
                ..Default::default()
            },
            |ui| panel.draw(ui.ctx()),
        );
        out.textures_delta.clear();
    }
}

#[test]
fn finished_upload_has_a_working_remove_action_without_remote_operations() {
    for protocol in [Protocol::Sftp, Protocol::Zmodem] {
        for state in [State::Failed, State::Cancelled, State::Succeeded] {
            let (id, row) = task(Some(state), protocol);
            let (mut panel, mut receiver) = panel_with_task(id, row);
            panel.noticed.insert((id, 1));
            let ctx = crate::ui_layout_gate::context();
            let pos = task_button(&ctx, &mut panel, "移除任务");
            click_task_button(&ctx, &mut panel, pos);
            assert!(
                panel
                    .connection
                    .as_ref()
                    .unwrap()
                    .shared
                    .lock()
                    .unwrap()
                    .rows
                    .is_empty()
            );
            assert!(
                !panel.open,
                "removing the last task closes the empty progress window"
            );
            assert!(panel.noticed.is_empty());
            assert!(
                receiver.try_recv().is_err(),
                "removal must not cancel, retry, or delete remote files"
            );
            panel.poll();
            assert!(!panel.open, "a removed failure must not reopen the window");
        }
    }
}

#[test]
fn rejected_start_can_be_removed_without_sending_a_daemon_request() {
    let (id, mut row) = task(None, Protocol::Sftp);
    let source = row.source;
    row.pending = true;
    row.apply_response(
        id,
        Some(Op::TerminalUploadStart),
        Ok(cshell_ipc::SftpResponse::with_status(
            SftpStatus::Unavailable,
            "请先连接目标 SSH 会话",
        )),
    );
    assert!(row.snapshot.is_none());
    assert_eq!(row.error.as_deref(), Some("请先连接目标 SSH 会话"));
    let (mut panel, mut receiver) = panel_with_task(id, row);
    panel.control(Op::TerminalUploadCancel, source, id);
    panel.control(Op::TerminalUploadGet, source, id);
    let ctx = crate::ui_layout_gate::context();
    let pos = task_button(&ctx, &mut panel, "移除任务");
    click_task_button(&ctx, &mut panel, pos);
    assert!(
        panel
            .connection
            .as_ref()
            .unwrap()
            .shared
            .lock()
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(!panel.open);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn unknown_start_cannot_be_removed_and_can_still_query_the_original_task() {
    for reply in [
        Err("请求超时".into()),
        Ok(cshell_ipc::SftpResponse::default()),
        Ok(cshell_ipc::SftpResponse::with_status(
            SftpStatus::Conflict,
            "上传请求已经提交",
        )),
    ] {
        let (id, mut row) = task(None, Protocol::Sftp);
        let source = row.source;
        row.apply_response(id, Some(Op::TerminalUploadStart), reply);
        assert!(row.error.is_some());
        row.apply_response(
            id,
            Some(Op::TerminalUploadGet),
            Ok(cshell_ipc::SftpResponse::with_status(
                SftpStatus::Unavailable,
                "上传任务不存在或不属于此 SSH 会话",
            )),
        );
        let (mut panel, mut receiver) = panel_with_task(id, row);
        panel.remove_finished(source, id);
        let ctx = crate::ui_layout_gate::context();
        let pos = task_button(&ctx, &mut panel, "刷新任务状态");
        click_task_button(&ctx, &mut panel, pos);
        let request = receiver.try_recv().unwrap();
        assert_eq!(request.operation, Op::TerminalUploadGet as i32);
        assert_eq!(request.transfer_id, id.as_uuid().as_bytes());
        assert!(receiver.try_recv().is_err());
    }
}

#[test]
fn reopening_uploads_exposes_the_original_directory_action_for_a_completed_task() {
    let (id, mut row) = task(Some(State::Succeeded), Protocol::Sftp);
    let source = row.source;
    row.auto_close = true;
    row.finished_at = Some(Instant::now() - Duration::from_secs(3));
    row.route = Some("gateway → target".into());
    let (mut panel, mut receiver) = panel_with_task(id, row);
    let ctx = crate::ui_layout_gate::context();
    crate::ui_layout_gate::frame(&ctx, egui::vec2(720.0, 480.0), |ctx| panel.draw(ctx));
    assert!(!panel.open);

    panel.show();
    let pos = task_button(&ctx, &mut panel, "在 SFTP 中打开目录");
    click_task_button(&ctx, &mut panel, pos);
    assert_eq!(panel.take_open_directory(), Some((source, id)));
    assert!(panel.take_open_directory().is_none());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn unfinished_and_uncertain_uploads_still_send_real_cancellation_to_the_original_session() {
    for state in [
        Some(State::Preparing),
        Some(State::Ready),
        Some(State::Running),
        Some(State::Paused),
        None,
    ] {
        for uncertain in [false, true] {
            let (id, mut row) = task(state, Protocol::Sftp);
            let source = row.source;
            if uncertain {
                row.error = Some("通信结果未确认".into());
            }
            let (mut panel, mut receiver) = panel_with_task(id, row);
            let ctx = crate::ui_layout_gate::context();
            let pos = task_button(&ctx, &mut panel, "取消上传");
            click_task_button(&ctx, &mut panel, pos);
            let request = receiver.try_recv().unwrap();
            assert_eq!(request.operation, Op::TerminalUploadCancel as i32);
            assert_eq!(request.session_id, source.as_uuid().as_bytes());
            assert_eq!(request.transfer_id, id.as_uuid().as_bytes());
            assert!(receiver.try_recv().is_err());
            assert!(
                panel
                    .connection
                    .as_ref()
                    .unwrap()
                    .shared
                    .lock()
                    .unwrap()
                    .rows
                    .contains_key(&id),
                "cancellation must await the daemon result"
            );
        }
    }
}

#[test]
fn removal_rechecks_identity_and_leaves_unfinished_or_unknown_tasks_intact() {
    let (done, row) = task(Some(State::Failed), Protocol::Sftp);
    let source = row.source;
    let (mut panel, mut receiver) = panel_with_task(done, row);
    let shared = panel.connection.as_ref().unwrap().shared.clone();
    let mut retained = Vec::new();
    for state in [
        Some(State::Preparing),
        Some(State::Running),
        Some(State::Paused),
        None,
    ] {
        let (id, row) = task(state, Protocol::Sftp);
        retained.push((id, row.source));
        shared.lock().unwrap().rows.insert(id, row);
    }
    panel.remove_finished(SessionId::new(), done);
    assert!(shared.lock().unwrap().rows.contains_key(&done));
    for (id, source) in &retained {
        panel.remove_finished(*source, *id);
    }
    assert_eq!(shared.lock().unwrap().rows.len(), retained.len() + 1);
    panel.remove_finished(source, done);
    assert_eq!(shared.lock().unwrap().rows.len(), retained.len());
    assert!(panel.open, "other upload progress must stay available");
    assert!(receiver.try_recv().is_err());
}

#[derive(Debug)]
struct NativeDrop(std::path::PathBuf);
impl egui::DroppedFile for NativeDrop {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        panic!("native drop must not read file contents on the UI thread")
    }
}
#[test]
fn native_drop_submits_once_to_the_target_and_rejects_local_modal_or_outside_drop() {
    for case in 0..4 {
        let ctx = crate::ui_layout_gate::context();
        let source = SessionId::new();
        let rect = egui::Rect::from_min_max(egui::pos2(80.0, 120.0), egui::pos2(650.0, 430.0));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let (stop, _) = tokio::sync::watch::channel(false);
        let mut panel = Panel {
            connection: Some(Connection {
                shared: Arc::new(Mutex::new(View::default())),
                sender,
                stop,
            }),
            ..Default::default()
        };
        let config = DesktopConnectionConfig::for_fixture(
            crate::daemon_connection::ResolvedDesktopEndpoint {
                endpoint: "terminal-upload-no-service".into(),
                instance_token: [1; 32],
                daemon_instance_id: [2; 16],
            },
        );
        let path = std::path::PathBuf::from(if cfg!(windows) {
            "C:/native-upload.bin"
        } else {
            "/tmp/native-upload.bin"
        });
        let mut draw = |ui: &mut egui::Ui| {
            ui.allocate_rect(rect, egui::Sense::hover());
            panel.drop_target(
                ui,
                rect,
                if case == 1 {
                    None
                } else {
                    Some((source, Some("gateway → target")))
                },
                Some(&config),
                case == 2,
                None,
            );
        };
        for _ in 0..2 {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 600.0),
                    )),
                    ..Default::default()
                },
                &mut draw,
            );
            out.textures_delta.clear();
        }
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                events: vec![egui::Event::PointerMoved(if case == 3 {
                    egui::pos2(20.0, 20.0)
                } else {
                    rect.center()
                })],
                dropped_files: vec![Arc::new(NativeDrop(path.clone()))],
                ..Default::default()
            },
            &mut draw,
        );
        out.textures_delta.clear();
        if case == 0 {
            let request = receiver.try_recv().unwrap();
            assert_eq!(request.operation, Op::TerminalUploadStart as i32);
            assert_eq!(request.session_id, source.as_uuid().as_bytes());
            let start: Start = serde_json::from_slice(&request.document_json).unwrap();
            assert_eq!(start.paths, vec![path.to_str().unwrap()]);
            assert!(request.confirmed);
        }
        assert!(
            receiver.try_recv().is_err(),
            "only one upload may be submitted per native drop"
        );
    }
}
