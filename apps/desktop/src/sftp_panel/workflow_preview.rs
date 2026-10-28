//! Fixed, offline production-control previews. No connections or credential access.
use super::*;
use cshell_application::*;
use std::{collections::HashMap, sync::Arc};
mod raster {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/cshell-ui/examples/support/raster.rs"
    ));
}

#[test]
#[ignore = "explicit fixed offline workflow preview"]
fn render_workflow_polish_preview() {
    for (name, size) in [
        ("workflow-connection-diagnostics",egui::vec2(1000.0,700.0)),
        ("workflow-connection-diagnostics-small",egui::vec2(720.0,480.0)),
        ("workflow-connection-waiting-small",egui::vec2(720.0,480.0)),
        ("workflow-command-dock", egui::vec2(1000.0, 700.0)),
        ("workflow-command-dock-small", egui::vec2(720.0, 480.0)),
        ("workflow-sftp-review", egui::vec2(1000.0, 700.0)),
        ("workflow-sftp-review-small", egui::vec2(720.0, 480.0)),
        ("workflow-sftp-presets", egui::vec2(1000.0, 700.0)),
        ("workflow-sftp-presets-small", egui::vec2(720.0, 480.0)),
        ("workflow-command-center", egui::vec2(900.0, 600.0)),
        ("workflow-keyboard-settings", egui::vec2(720.0, 480.0)),
        ("workflow-sftp-editor", egui::vec2(900.0, 600.0)),
        ("workflow-sftp-editor-small", egui::vec2(720.0, 480.0)),
        ("workflow-sftp-queue", egui::vec2(900.0, 600.0)),
        ("workflow-sftp-queue-small", egui::vec2(720.0, 480.0)),
        ("workflow-sftp-sync", egui::vec2(1000.0, 700.0)),
        ("workflow-sftp-sync-small", egui::vec2(720.0, 480.0)),
        ("workflow-command-history", egui::vec2(1000.0, 700.0)),
        ("workflow-command-history-small", egui::vec2(720.0, 480.0)),
    ] {
        let ctx = crate::ui_layout_gate::context();
        let session = SessionId::new();
        let peer = cshell_domain::ClipboardTrust {
            host: cshell_domain::ClipboardHost::new("linux.example.test", 22).unwrap(),
            username: "operator".into(),
            host_key_sha256: "SHA256:offline-preview".into(),
            route: String::new(),
        };
        let text = "# Offline preview / 离线示例\nserver {\n    listen 8080;\n    server_name example.test;\n    root /srv/www;\n}\n";
        let content = sftp_digest(text.as_bytes());
        let mut view = SftpView {
            session_id: Some(session),
            peer: Some(peer.clone()),
            connection_profile: Some(cshell_domain::ProfileId::new()),
            connection_phase: crate::sftp_connection::ConnectionPhase::Connected,
            edit: Some(Arc::new(SftpEditDocument {
                native_transfer: None,
                base_permissions: Some(0o644),
                peer,
                state: SftpEditState::Ready,
                queued_draft: None,
                format: "cshell.sftp-edit".into(),
                version: 1,
                id: SessionId::new(),
                session,
                revision: 1,
                remote_path: "/etc/nginx/conf.d/example.conf".into(),
                local_path: std::env::temp_dir()
                    .join("offline-editor-preview")
                    .to_string_lossy()
                    .into(),
                base: sftp_digest(b"# Original version\n"),
                current: content,
                queued_item: None,
                closed: false,
                detail: "Offline production-control preview".into(),
            })),
            edit_text: Some(Arc::new(text.into())),
            edit_writable: true,
            queue_writable: true,
            queue: Some(Arc::new(SftpQueueSnapshot::initial(session))),
            ..Default::default()
        };
        let mut panel = SftpPanel::default();
        panel.bound_session_id = Some(session);
        panel.page = 2;
        if name.starts_with("workflow-sftp-queue") {
            panel.page = 0;
            view.edit = None;
            view.local_directory = Some("C:/Users/operator/Downloads".into());
            view.remote_loaded_directory = Some("/srv/www".into());
            view.local_entries = vec![cshell_ipc::SftpDirectoryEntry {
                name: "project".into(),
                path: "C:/Users/operator/Downloads/project".into(),
                kind: 2,
                ..Default::default()
            }];
            let id = SessionId::new();
            view.trees.insert(
                id,
                SftpTreeSnapshot {
                    id,
                    session,
                    direction: SftpQueueDirection::Upload,
                    state: TerminalUploadState::Running,
                    completed: 24,
                    total_files: 80,
                    bytes: 6 * 1024 * 1024,
                    total_bytes: 20 * 1024 * 1024,
                    bytes_per_second: 2 * 1024 * 1024,
                    detail: "离线示例：正在上传目录，未连接服务器".into(),
                    retryable: false,
                    label: "project".into(),
                    destination: "/srv/www".into(),
                },
            );
        }
        if name.starts_with("workflow-sftp-sync") || name.starts_with("workflow-sftp-presets") {
            panel.page = 0;
            view.edit = None;
            view.local_directory = Some("C:/Users/operator/project".into());
            view.remote_loaded_directory = Some("/srv/project".into());
            let id = SessionId::new();
            let local = SftpSyncMetadata {
                directory: false,
                size: 2048,
                modified: Some(1792960000),
            };
            view.sync_preview = Some(Arc::new(SftpSyncPreview {
                id,
                session,
                options: SftpSyncOptions {
                    local: view.local_directory.clone().unwrap(),
                    remote: view.remote_loaded_directory.clone().unwrap(),
                    direction: SftpQueueDirection::Upload,
                    verification: Default::default(),
                    compare_content: true,
                    excludes: Vec::new(),
                    preserve_modified: false,
                },
                entries: vec![
                    SftpSyncEntry {
                        relative: "app/config.toml".into(),
                        local: Some(local.clone()),
                        remote: None,
                        difference: SftpSyncDifference::LocalOnly,
                    },
                    SftpSyncEntry {
                        relative: "app/main.py".into(),
                        local: Some(local.clone()),
                        remote: Some(SftpSyncMetadata {
                            size: 1024,
                            ..local.clone()
                        }),
                        difference: SftpSyncDifference::Changed,
                    },
                    SftpSyncEntry {
                        relative: "logs/server.log".into(),
                        local: None,
                        remote: Some(local),
                        difference: SftpSyncDifference::RemoteOnly,
                    },
                ],
            }));
            panel.preview_directory_sync(session, id);
        }
        if name.starts_with("workflow-sftp-review") {
            let doc = view.edit.as_ref().unwrap();
            view.edit_review = Some(Arc::new(SftpEditReview {
                session,
                id: doc.id,
                revision: doc.revision,
                base_content: doc.base,
                base: Some("# Original version\n".into()),
                remote_content: sftp_digest(b"# Server version\nlisten 9090;\n"),
                remote_permissions: Some(0o644),
                remote: "# Server version\nlisten 9090;\n".into(),
                backups: vec![],
                selected_backup: None,
                backup_text: None,
            }));
            panel.advanced.preview_editor_review(doc.id);
        }
        let diagnostic=crate::connection_diagnostics::preview(if name.contains("waiting"){cshell_domain::ConnectionOutcome::Running}else{cshell_domain::ConnectionOutcome::Failed});
        let (mut shell, shell_view) = crate::shell_history_panel::preview(session);
        let mut dock = crate::command_workspace::Dock::default();
        dock.toggle();
        let mut events = vec![];
        let mut center = crate::command_center::State::default();
        if name == "workflow-command-center" {
            center.show();
        } else if name == "workflow-keyboard-settings" {
            center.show_settings();
        }
        let mut model = cshell_ui::WorkbenchViewModel {
            can_sftp: true,
            can_reconnect_ssh: true,
            can_disconnect_ssh: true,
            daemon_connected: true,
            ..Default::default()
        };
        let mut textures = HashMap::new();
        let mut primitives = vec![];
        for frame in 0..10 {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    events: std::mem::take(&mut events),
                    time: Some(frame as f64 * 0.2),
                    ..Default::default()
                },
                |ui| {
                    ui.painter()
                        .rect_filled(ui.max_rect(), 0.0, cshell_ui::theme::BACKGROUND);
                    if name.starts_with("workflow-connection") {
                        cshell_ui::draw_workbench_chrome(ui,&mut model);
                        let _=crate::connection_diagnostics::draw(ui,Some(&diagnostic),name.contains("waiting"),true);
                        let rect=cshell_ui::draw_workbench_content(ui,&mut model);
                        ui.painter().text(rect.min+egui::vec2(16.0,16.0),egui::Align2::LEFT_TOP,"离线连接诊断示例 · 未连接用户服务器",egui::FontId::proportional(14.0),cshell_ui::theme::MUTED);
                    } else if name.starts_with("workflow-command-dock") {
                        dock.draw(ui, |ui| shell.draw(ui, Some(session), &shell_view, None));
                        let rect = cshell_ui::draw_workbench(ui, &mut model);
                        ui.painter().text(
                            rect.min + egui::vec2(20.0, 20.0),
                            egui::Align2::LEFT_TOP,
                            "operator@linux:~$",
                            egui::FontId::monospace(28.0),
                            cshell_ui::theme::TEXT,
                        );
                    } else if name.starts_with("workflow-command-history") {
                        cshell_ui::form::dialog(ui.ctx(), "命令历史与输出 · 离线示例")
                            .default_width(850.0)
                            .show(ui.ctx(), |ui| {
                                shell.draw(ui, Some(session), &shell_view, None)
                            });
                    } else if name.starts_with("workflow-sftp") {
                        egui::Frame::NONE.inner_margin(10).show(ui, |ui| {
                            let _ = panel.draw_contents(ui, Some(session), Some(&view));
                        });
                    } else {
                        let rect = cshell_ui::draw_workbench(ui, &mut model);
                        ui.painter().text(
                            rect.min + egui::vec2(20.0, 20.0),
                            egui::Align2::LEFT_TOP,
                            "离线工作台预览 · 未连接服务器",
                            egui::FontId::proportional(14.0),
                            cshell_ui::theme::MUTED,
                        );
                        center.draw(ui.ctx(), &mut model);
                    }
                },
            );
            if frame == 2 && name.starts_with("workflow-sftp-presets") {
                if let Some(tree) = &out.platform_output.accesskit_update
                    && let Some((_, node)) = tree
                        .nodes
                        .iter()
                        .find(|(_, n)| n.label() == Some("方案与规则"))
                {
                    let pos = crate::ui_layout_gate::bounds(node).center();
                    events = vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ];
                }
            }
            if name.starts_with("workflow-sftp-review") {
                std::thread::sleep(std::time::Duration::from_millis(10));
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
        raster::write(
            &path,
            size.x as usize,
            size.y as usize,
            primitives,
            &textures,
        )
        .unwrap();
    }
}
