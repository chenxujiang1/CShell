use super::*;

fn compact_frame(ctx: &egui::Context, model: &mut WorkbenchViewModel, events: Vec<egui::Event>) -> (egui::Rect, egui::accesskit::TreeUpdate) {
    let mut body = egui::Rect::NOTHING;
    let mut out = ctx.run_ui(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0))),
        events, ..Default::default()
    }, |ui| {
        draw_workbench_chrome(ui, model);
        body = draw_workbench_content(ui, model);
    });
    out.textures_delta.clear();
    (body, out.platform_output.accesskit_update.unwrap())
}

fn click_status(ctx: &egui::Context, model: &mut WorkbenchViewModel, tree: &egui::accesskit::TreeUpdate, label: &str) {
    let bounds = tree.nodes.iter().find(|(_, node)| node.label() == Some(label)).unwrap().1.bounds().unwrap();
    let pos = egui::pos2(((bounds.x0 + bounds.x1) / 2.0) as f32, ((bounds.y0 + bounds.y1) / 2.0) as f32);
    assert!(pos.y > 550.0, "compact control must live in the status bar");
    for pressed in [true, false] {
        compact_frame(ctx, model, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
        }]);
    }
}

#[test]
fn hidden_sidebar_has_no_reserved_rail_and_status_button_restores_width() {
    let ctx = egui::Context::default(); ctx.enable_accesskit();
    let mut model = WorkbenchViewModel { sidebar_hidden: true, sidebar_width: Some(300.0), ..Default::default() };
    let (body, tree) = compact_frame(&ctx, &mut model, vec![]);
    assert!(body.left() < 16.0, "hidden sidebar still consumes {} pixels", body.left());
    click_status(&ctx, &mut model, &tree, "展开会话管理器");
    assert!(!model.sidebar_hidden);
    let (body, _) = compact_frame(&ctx, &mut model, vec![]);
    assert!(body.left() >= 300.0);
    assert_eq!(model.sidebar_width, Some(300.0));
}

#[test]
fn connection_diagnostics_is_an_explicit_status_bar_toggle() {
    let ctx = egui::Context::default(); ctx.enable_accesskit();
    let tab = TabId::new();
    let mut model = WorkbenchViewModel { selected_tab: Some(tab), ..Default::default() };
    for state in [SshConnectionState::Connecting, SshConnectionState::Connected, SshConnectionState::Disconnected] {
        model.ssh_tab_states.insert(tab, state);
        compact_frame(&ctx, &mut model, vec![]);
        assert!(!model.connection_diagnostics_open);
    }
    let (_, tree) = compact_frame(&ctx, &mut model, vec![]);
    click_status(&ctx, &mut model, &tree, "连接诊断");
    assert!(model.connection_diagnostics_open);
    let (_, tree) = compact_frame(&ctx, &mut model, vec![]);
    click_status(&ctx, &mut model, &tree, "连接诊断");
    assert!(!model.connection_diagnostics_open);
}

#[test]
fn temporary_zoom_status_resets_without_changing_global_font() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut model = WorkbenchViewModel {
        terminal_font_pixels: Some(28),
        terminal_zoom_pixels: Some(36),
        ..Default::default()
    };
    let size = egui::vec2(900.0, 600.0);
    let mut tree = None;
    for _ in 0..4 {
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                ..Default::default()
            },
            |ui| {
                draw_workbench_chrome(ui, &mut model);
            },
        );
        out.textures_delta.clear();
        tree = out.platform_output.accesskit_update;
    }
    let tree = tree.unwrap();
    let node = &tree
        .nodes
        .iter()
        .find(|(_, n)| n.label() == Some("36 px · 临时 ↺"))
        .expect("temporary zoom button")
        .1;
    let rect = node.bounds().unwrap();
    let pos = egui::pos2(
        ((rect.x0 + rect.x1) / 2.0) as f32,
        ((rect.y0 + rect.y1) / 2.0) as f32,
    );
    assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains(pos));
    for pressed in [true, false] {
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| {
                draw_workbench_chrome(ui, &mut model);
            },
        );
        out.textures_delta.clear();
    }
    assert_eq!(model.terminal_zoom_pixels, None);
    assert_eq!(model.terminal_font_pixels, Some(28));
}
