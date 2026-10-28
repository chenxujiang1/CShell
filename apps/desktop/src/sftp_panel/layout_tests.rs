use super::*;
use crate::ui_layout_gate::{check_window, context, frame};

#[test]
fn all_sftp_pages_fit_with_long_paths_and_names() {
    for size in [egui::vec2(900.0, 600.0), egui::vec2(720.0, 480.0)] {
        let context = context();
        let session = SessionId::new();
        let path = format!("/home/operator/{}/file.txt", "中文目录/".repeat(25));
        let entry = cshell_ipc::SftpDirectoryEntry {
            name: "很长的中文文件名".repeat(20),
            path: path.clone(),
            kind: 1,
            ..Default::default()
        };
        let view = SftpView {
            session_id: Some(session),
            local_directory: Some(path.clone()),
            remote_loaded_directory: Some(path.clone()),
            entries: vec![entry.clone(); 30],
            local_entries: vec![entry; 30],
            queue: Some(std::sync::Arc::new(
                cshell_application::SftpQueueSnapshot::initial(session),
            )),
            ..Default::default()
        };
        let mut panel = SftpPanel::default();
        panel.open = true;
        for page in 0..4 {
            panel.page = page;
            for _ in 0..2 {
                frame(&context, size, |c| {
                    assert!(panel.draw(c, Some(session), Some(&view)).is_none());
                });
            }
            let tree = frame(&context, size, |c| {
                assert!(panel.draw(c, Some(session), Some(&view)).is_none());
            });
            check_window(&context, size, "SFTP 文件", &tree);
        }
    }
}

#[test]
fn native_content_fills_its_own_viewport_and_keeps_controls_in_bounds() {
    use crate::ui_layout_gate::bounds;
    for size in [
        egui::vec2(720.0, 480.0),
        egui::vec2(1560.0, 900.0),
        egui::vec2(2200.0, 1200.0),
    ] {
        let context = context();
        let session = SessionId::new();
        let view = SftpView {
            session_id: Some(session),
            local_directory: Some("C:/Users/operator".into()),
            remote_loaded_directory: Some("/home/operator".into()),
            queue: Some(std::sync::Arc::new(
                cshell_application::SftpQueueSnapshot::initial(session),
            )),
            ..Default::default()
        };
        let mut panel = SftpPanel::default();
        for page in 0..4 {
            panel.page = page;
            for _ in 0..3 {
                let mut output = context.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ui| {
                        egui::Frame::NONE.inner_margin(10).show(ui, |ui| {
                            assert!(
                                panel
                                    .draw_contents(ui, Some(session), Some(&view))
                                    .is_none()
                            );
                        });
                    },
                );
                output.textures_delta.clear();
                for (_, node) in &output.platform_output.accesskit_update.unwrap().nodes {
                    if matches!(
                        node.role(),
                        egui::accesskit::Role::Button | egui::accesskit::Role::TextInput
                    ) {
                        let rect = bounds(node);
                        assert!(
                            rect.min.x >= 0.0 && rect.max.x <= size.x + 1.0,
                            "native SFTP page {page}, {size:?}: {:?} out of bounds {rect:?}",
                            node.label()
                        );
                    }
                }
            }
        }
    }
}
