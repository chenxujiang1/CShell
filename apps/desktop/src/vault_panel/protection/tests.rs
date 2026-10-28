#![allow(clippy::unwrap_used)]
use super::*;
fn draw(
    c: &egui::Context,
    p: &mut VaultPanel,
    events: Vec<egui::Event>,
) -> (egui::accesskit::TreeUpdate, Option<VaultRequest>) {
    let mut request = None;
    let mut output = c.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 480.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            egui::Window::new("全局凭据保护测试")
                .default_width(600.0)
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(330.0)
                        .show(ui, |ui| {
                            request = p.draw_protection(ui);
                        });
                });
        },
    );
    output.textures_delta.clear();
    (output.platform_output.accesskit_update.unwrap(), request)
}
fn click(tree: &egui::accesskit::TreeUpdate, label: &str) -> Vec<egui::Event> {
    let rect = tree
        .nodes
        .iter()
        .find(|(_, n)| n.label() == Some(label) && n.role() == egui::accesskit::Role::Button)
        .unwrap()
        .1
        .bounds()
        .unwrap();
    let pos = egui::pos2(
        (rect.x0 + rect.x1) as f32 / 2.0,
        (rect.y0 + rect.y1) as f32 / 2.0,
    );
    vec![
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
    ]
}
#[test]
fn global_mode_switch_requests_keep_all_credentials_and_clear_local_passwords() {
    for system in [true, false] {
        let c = crate::ui_layout_gate::context();
        let mut p = VaultPanel {
            response: Some(VaultResponse {
                exists: true,
                unlocked: true,
                system,
                revision: 7,
                lock_generation: 9,
                ..Default::default()
            }),
            protection_target: Some(!system),
            password: Zeroizing::new("test-master-password".into()),
            confirmation: Zeroizing::new("test-master-password".into()),
            ..Default::default()
        };
        for _ in 0..3 {
            draw(&c, &mut p, vec![]);
        }
        let (tree, _) = draw(&c, &mut p, vec![]);
        crate::ui_layout_gate::check_window(
            &c,
            egui::vec2(720.0, 480.0),
            "全局凭据保护测试",
            &tree,
        );
        let (_, request) = draw(&c, &mut p, click(&tree, "应用保护方式（保留所有凭据）"));
        let request = request.unwrap();
        assert_eq!(
            request.operation,
            if system {
                Op::ProtectWithPassword
            } else {
                Op::ProtectWithSystem
            } as i32
        );
        assert_eq!(request.expected_vault_revision, 7);
        assert_eq!(request.expected_lock_generation, 9);
        assert!(request.profile_id.is_empty());
        assert!(!request.confirm_rebuild);
        assert_eq!(request.secret, b"test-master-password");
        assert!(p.password.is_empty() && p.confirmation.is_empty());
    }
}
#[test]
fn new_library_defaults_to_system_protection_and_locked_state_stays_inside_global_settings() {
    let c = crate::ui_layout_gate::context();
    let mut p = VaultPanel {
        response: Some(VaultResponse::default()),
        ..Default::default()
    };
    for _ in 0..3 {
        draw(&c, &mut p, vec![]);
    }
    let (tree, _) = draw(&c, &mut p, vec![]);
    assert!(
        tree.nodes
            .iter()
            .any(|(_, n)| n.label() == Some("创建系统钥匙串保护的凭据库"))
    );
    let (_, request) = draw(&c, &mut p, click(&tree, "创建系统钥匙串保护的凭据库"));
    assert_eq!(request.unwrap().operation, Op::CreateSystem as i32);
}
