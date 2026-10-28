use super::*;
use crate::ui_layout_gate::{bounds, context};

pub(super) mod raster {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/cshell-ui/examples/support/raster.rs"
    ));
}

#[test]
#[ignore = "explicit offline UI artifact generation"]
fn render_auxiliary_tools_preview() {
    let directory = tempfile::tempdir().unwrap();
    let audit = DesktopSyncAudit::start(
        &cshell_ipc::RuntimePaths::prepare(directory.path().to_path_buf()).unwrap(),
    )
    .unwrap();
    let mut sync = DesktopSyncInput::with_audit(audit);
    let window = WorkspaceWindowId::new();
    let group = cshell_domain::TabGroupId::new();
    let tab = TabId::new();
    sync.open_compose(window, Some(tab));
    sync.compose.multiline = true;
    sync.compose
        .text
        .push_str("systemctl status nginx\njournalctl -u nginx -n 20");
    for (name, size, settings) in [
        ("ssh-tools", egui::vec2(900.0, 600.0), false),
        ("ssh-settings", egui::vec2(720.0, 480.0), true),
    ] {
        let ctx = context();
        if settings {sync.close_compose();}
        let mut model = cshell_ui::WorkbenchViewModel::default();
        let mut defaults = crate::zmodem_panel::Panel::default();
        defaults.open_defaults(false);
        defaults.sync_display(28);
        let mut textures = std::collections::HashMap::new();
        let mut primitives = vec![];
        for frame in 0..4 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    time: Some(frame as f64 * 0.25),
                    ..Default::default()
                },
                |ui| {
                    cshell_ui::draw_workbench_chrome(ui, &mut model);
                    sync.draw(ui, window, group, Some(tab), false);
                    let rect = cshell_ui::draw_workbench_content(ui, &mut model);
                    ui.painter().rect_filled(rect,0.0,egui::Color32::from_rgb(15,20,29));
                    ui.painter().with_clip_rect(rect).text(
                        rect.min + egui::vec2(20.0, 20.0),
                        egui::Align2::LEFT_TOP,
                        "终端辅助区预览 · 未连接服务器",
                        egui::FontId::proportional(14.0),
                        egui::Color32::GRAY,
                    );
                    if settings {
                        defaults.draw(ui.ctx(), None, &[]);
                    }
                },
            );
            for (id, deltas) in output.textures_delta.set.drain() {
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
            primitives = ctx.tessellate(output.shapes, 1.0);
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

fn render(
    ctx: &egui::Context,
    sync: &mut DesktopSyncInput,
    window: WorkspaceWindowId,
    tab: TabId,
    size: egui::Vec2,
    events: Vec<egui::Event>,
) -> (egui::accesskit::TreeUpdate, egui::Rect) {
    let mut remaining = egui::Rect::NOTHING;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            ..Default::default()
        },
        |ui| {
            sync.draw_compose(ui, window, Some(tab));
            remaining = ui.available_rect_before_wrap();
        },
    );
    output.textures_delta.clear();
    (output.platform_output.accesskit_update.unwrap(), remaining)
}

#[test]
fn dock_switches_tools_preserves_draft_and_escape_returns_focus_without_sending() {
    for size in [egui::vec2(720.0, 480.0), egui::vec2(900.0, 600.0)] {
        let directory = tempfile::tempdir().unwrap();
        let audit = DesktopSyncAudit::start(
            &cshell_ipc::RuntimePaths::prepare(directory.path().to_path_buf()).unwrap(),
        )
        .unwrap();
        let mut sync = DesktopSyncInput::with_audit(audit);
        let ctx = context();
        let window = WorkspaceWindowId::new();
        let tab = TabId::new();
        sync.open_compose(window, Some(tab));
        sync.compose.text.push_str("draft body");
        for _ in 0..3 {
            render(&ctx, &mut sync, window, tab, size, vec![]);
        }
        let (tree, terminal) = render(&ctx, &mut sync, window, tab, size, vec![]);
        assert!(terminal.height() >= size.y - 82.0, "{terminal:?}");
        assert!(ctx.memory(|m| m.focused()).is_some());
        for label in [
            "当前会话",
            "历史",
            "发送",
            "更多",
        ] {
            let node = &tree
                .nodes
                .iter()
                .find(|(_, n)| n.label() == Some(label))
                .expect(label)
                .1;
            assert!(
                egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(bounds(node)),
                "{label}"
            );
        }
        sync.open_quick_commands(window, Some(tab));
        assert_eq!(sync.compose.text.as_str(), "draft body");
        render(&ctx, &mut sync, window, tab, size, vec![]);
        assert!(sync.blocks_input());
        sync.compose.page = 2;
        for _ in 0..2 {
            render(&ctx, &mut sync, window, tab, size, vec![]);
        }
        let (tree, _) = render(&ctx, &mut sync, window, tab, size, vec![]);
        assert!(
            tree.nodes
                .iter()
                .any(|(_, n)| n.label() == Some("编辑正文 / 模板…"))
        );
        render(
            &ctx,
            &mut sync,
            window,
            tab,
            size,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!sync.compose_open(window));
        assert!(sync.compose.text.is_empty());
        assert!(sync.pending.is_none() && sync.pending_current.is_none());
        assert!(!sync.blocks_input());
        assert!(sync.take_focus_return());
        assert!(!sync.take_focus_return());
    }
}
