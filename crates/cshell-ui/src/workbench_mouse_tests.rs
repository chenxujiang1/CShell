use super::*;

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
