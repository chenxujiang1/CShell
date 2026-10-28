//! Geometry checks use the production font/style and do not connect to a daemon.
use egui::{Context, Rect, Vec2, accesskit};

pub(crate) fn context() -> Context {
    let context = Context::default();
    crate::ui_fonts::install(&context);
    cshell_ui::install_workbench_style(&context);
    context.enable_accesskit();
    context
}

pub(crate) fn frame(
    context: &Context,
    size: Vec2,
    mut draw: impl FnMut(&Context),
) -> accesskit::TreeUpdate {
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        },
        |ui| draw(ui.ctx()),
    );
    output.textures_delta.clear();
    output
        .platform_output
        .accesskit_update
        .expect("accessible UI")
}

pub(crate) fn bounds(node: &accesskit::Node) -> Rect {
    let b = node.bounds().expect("widget bounds");
    Rect::from_min_max(
        egui::pos2(b.x0 as f32, b.y0 as f32),
        egui::pos2(b.x1 as f32, b.y1 as f32),
    )
}

pub(crate) fn check_window(
    context: &Context,
    size: Vec2,
    title: &str,
    tree: &accesskit::TreeUpdate,
) {
    let rect = context
        .memory(|m| m.area_rect(egui::Id::new(Some(title))))
        .expect("window area");
    let viewport = Rect::from_min_size(egui::Pos2::ZERO, size).expand(1.0);
    assert!(
        viewport.contains_rect(rect),
        "{title}: {rect:?} outside {viewport:?}"
    );
    for (_, node) in &tree.nodes {
        if matches!(
            node.role(),
            accesskit::Role::TextInput
                | accesskit::Role::MultilineTextInput
                | accesskit::Role::PasswordInput
                | accesskit::Role::Button
                | accesskit::Role::CheckBox
                | accesskit::Role::RadioButton
        ) {
            let field = bounds(node);
            assert!(
                field.min.x >= rect.min.x && field.max.x <= rect.max.x + 1.0,
                "{title}: {:?} field outside window: {field:?} / {rect:?}",
                node.label()
            );
        }
    }
}

#[test]
fn shared_task_definition_fits_a_narrow_control_column() {
    let context = context();
    let size = egui::vec2(900.0, 600.0);
    let mut editor = crate::task_definition_editor::TaskDefinitionEditor::default();
    let mut draw = |c: &Context| {
        cshell_ui::form::dialog(c, "任务定义布局检查")
            .default_size([380.0, 500.0])
            .show(c, |ui| editor.draw(ui, false, false));
    };
    for _ in 0..2 {
        frame(&context, size, &mut draw);
    }
    let tree = frame(&context, size, &mut draw);
    check_window(&context, size, "任务定义布局检查", &tree);
}

#[test]
fn auxiliary_dialogs_fit_the_smallest_supported_workbench() {
    for size in [egui::vec2(900.0, 600.0), egui::vec2(720.0, 480.0)] {
        let mut vault = crate::vault_panel::VaultPanel::default();
        vault.open = true;
        let mut package = crate::credential_package::CredentialPackagePanel::default();
        package.open = true;
        let mut ca = crate::host_ca_panel::HostCaPanel::default();
        ca.open = true;
        let mut provider = crate::ssh_provider_panel::SshProviderPanel::default();
        provider.open = true;
        let mut batch = crate::profile_batch_panel::ProfileBatchPanel::default();
        batch.open = true;
        let mut policy = crate::terminal_policy_panel::TerminalPolicyPanel::default();
        policy.open = true;
        let mut tunnel = crate::tunnel_panel::TunnelPanel::default();
        tunnel.open = true;
        let mut workspace = crate::workspace_recovery_panel::WorkspaceRecoveryPanel::default();
        workspace.open = true;
        let document = cshell_domain::WorkspaceDocument::default();
        let sessions = Default::default();
        let mut cases: Vec<(&str, Box<dyn FnMut(&Context)>)> = vec![
            (
                "凭据管理",
                Box::new(|c| {
                    let _ = vault.draw(c, None);
                }),
            ),
            (
                "Encrypted credential package",
                Box::new(|c| {
                    let _ = package.draw(c, None, None);
                }),
            ),
            ("OpenSSH CA 与证书", Box::new(|c| ca.draw(c, None))),
            ("SSH 能力", Box::new(|c| provider.draw(c, None))),
            (
                "Profile 批量编辑与继承来源",
                Box::new(|c| {
                    batch.draw(c, None, None, None);
                }),
            ),
            (
                "标签 / Host / 临时终端继承",
                Box::new(|c| policy.draw(c, None, None, None, None)),
            ),
            (
                "SSH 隧道管理",
                Box::new(|c| tunnel.draw(c, None, None, None)),
            ),
            (
                "工作区恢复 / 布局导入导出",
                Box::new(|c| workspace.draw(c, None, &document, &sessions, None)),
            ),
        ];
        for (title, draw) in &mut cases {
            let context = context();
            for _ in 0..2 {
                frame(&context, size, &mut *draw);
            }
            let tree = frame(&context, size, &mut *draw);
            check_window(&context, size, title, &tree);
        }
    }
}
