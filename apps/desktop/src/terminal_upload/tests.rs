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
                snapshot: Some(snapshot),
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
            snapshot: None,
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
                if case == 1 { None } else { Some(source) },
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
        drop(draw);
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
