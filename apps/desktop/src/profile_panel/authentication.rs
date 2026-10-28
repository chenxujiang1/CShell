use super::*;
use cshell_domain::SshAuthStep as Step;
fn label(step: Step) -> &'static str {
    match step {
        Step::KeyboardInteractive => "键盘交互验证（验证码 / 密码）",
        Step::Password => "密码认证",
        Step::PrivateKey => "私钥文件",
        Step::Certificate => "OpenSSH 证书",
        Step::Agent => "SSH Agent",
    }
}
pub(super) fn draw(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    catalog: &DesktopProfileCatalog,
) -> Option<ProfileClientCommand> {
    let mut command = None;
    ui.heading("账号与认证");
    cshell_ui::form::text(ui, "用户名（可留空）", &mut draft.ssh_username);
    ui.weak("账号和密码都可留空保存；连接时再填写，并可选择保存。");
    if draft.ssh_options.authentication_order.is_empty() {
        draft.ssh_options.authentication_order = match draft.auth_method {
            SshAuthMethod::Password if draft.ssh_options.keyboard_interactive => {
                vec![Step::KeyboardInteractive, Step::Password]
            }
            SshAuthMethod::Password => vec![Step::Password],
            SshAuthMethod::PrivateKey => vec![Step::PrivateKey],
            SshAuthMethod::Certificate => vec![Step::Certificate],
            SshAuthMethod::Agent => vec![Step::Agent],
        };
    }
    if !draft.ssh_options.authentication_order.iter().any(|m| matches!(m, Step::Password | Step::KeyboardInteractive)) { draft.password.zeroize(); }
    let saved = catalog.ssh_connections.iter().any(|target| {
        target.profile_id == draft.record.id
            && target.host == draft.ssh_host.trim()
            && target.port == draft.ssh_port
            && target.username == draft.ssh_username.trim()
            && target.auth_method == draft.auth_method
            && target.route == draft.route()
    });
    if draft
        .ssh_options
        .authentication_order
        .iter()
        .any(|m| matches!(m, Step::Password | Step::KeyboardInteractive))
    {
        ui.label("密码（可留空）");
        crate::secret_edit::edit(ui, &mut draft.password, "profile-password", 4096);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    saved && !draft.ssh_username.trim().is_empty() && !draft.password.is_empty(),
                    egui::Button::new("保存密码"),
                )
                .clicked()
            {
                command = Some(ProfileClientCommand::SetPassword {
                    profile_id: draft.record.id,
                    expected_revision: catalog.revision,
                    password: Zeroizing::new(std::mem::take(&mut draft.password).into_bytes()),
                });
            }
            if ui
                .add_enabled(saved, egui::Button::new("移除密码"))
                .clicked()
            {
                draft.password.zeroize();
                command = Some(ProfileClientCommand::DeletePassword {
                    profile_id: draft.record.id,
                    expected_revision: catalog.revision,
                });
            }
        });
    }
    let summary = draft
        .ssh_options
        .authentication_order
        .iter()
        .map(|m| label(*m))
        .collect::<Vec<_>>()
        .join(" → ");
    ui.label(format!("认证顺序：{summary}"));
    ui.collapsing("调整认证方式与顺序", |ui| {
        ui.weak("按顺序尝试，服务器未启用的方式自动跳过。使用箭头调整顺序。");
        let mut rows = draft.ssh_options.authentication_order.clone();
        for step in [
            Step::KeyboardInteractive,
            Step::Password,
            Step::PrivateKey,
            Step::Certificate,
            Step::Agent,
        ] {
            if !rows.contains(&step) {
                rows.push(step);
            }
        }
        for step in rows {
            ui.push_id(step as u8, |ui| {
                ui.horizontal(|ui| {
                    let position = draft
                        .ssh_options
                        .authentication_order
                        .iter()
                        .position(|m| *m == step);
                    let mut enabled = position.is_some();
                    if ui
                        .add_enabled(
                            position.is_none() || draft.ssh_options.authentication_order.len() > 1,
                            egui::Checkbox::new(&mut enabled, label(step)),
                        )
                        .changed()
                    {
                        if enabled {
                            draft.ssh_options.authentication_order.push(step);
                        } else {
                            draft
                                .ssh_options
                                .authentication_order
                                .retain(|m| *m != step);
                        }
                    }
                    if let Some(index) = draft
                        .ssh_options
                        .authentication_order
                        .iter()
                        .position(|m| *m == step)
                    {
                        if ui
                            .add_enabled(index > 0, egui::Button::new("↑"))
                            .on_hover_text("提前认证")
                            .clicked()
                        {
                            draft
                                .ssh_options
                                .authentication_order
                                .swap(index, index - 1);
                        }
                        if ui
                            .add_enabled(
                                index + 1 < draft.ssh_options.authentication_order.len(),
                                egui::Button::new("↓"),
                            )
                            .on_hover_text("延后认证")
                            .clicked()
                        {
                            draft
                                .ssh_options
                                .authentication_order
                                .swap(index, index + 1);
                        }
                    }
                });
            });
        }
    });
    draft.ssh_options.keyboard_interactive = draft
        .ssh_options
        .authentication_order
        .contains(&Step::KeyboardInteractive);
    if draft
        .ssh_options
        .authentication_order
        .iter()
        .any(|m| matches!(m, Step::PrivateKey | Step::Certificate))
    {
        ui.separator();
        cshell_ui::form::text(ui, "私钥绝对路径", &mut draft.private_key_path);
        if draft
            .ssh_options
            .authentication_order
            .contains(&Step::Certificate)
        {
            cshell_ui::form::text(ui, "证书绝对路径", &mut draft.certificate_path);
        }
        ui.label("私钥口令");
        crate::secret_edit::edit(ui, &mut draft.key_passphrase, "profile-key-phrase", 4096);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    saved && !draft.key_passphrase.is_empty(),
                    egui::Button::new("保存私钥口令"),
                )
                .clicked()
            {
                command = Some(ProfileClientCommand::SetKeyPassphrase {
                    profile_id: draft.record.id,
                    expected_revision: catalog.revision,
                    passphrase: Zeroizing::new(
                        std::mem::take(&mut draft.key_passphrase).into_bytes(),
                    ),
                });
            }
            if ui
                .add_enabled(saved, egui::Button::new("移除私钥口令"))
                .clicked()
            {
                draft.key_passphrase.zeroize();
                command = Some(ProfileClientCommand::DeleteKeyPassphrase {
                    profile_id: draft.record.id,
                    expected_revision: catalog.revision,
                });
            }
        });
    }
    if draft
        .ssh_options
        .authentication_order
        .contains(&Step::Agent)
    {
        cshell_ui::form::combo(ui, "Agent 类型")
            .selected_text(format!("{:?}", draft.agent_backend))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.agent_backend, SshAgentBackend::Auto, "Auto");
                ui.selectable_value(
                    &mut draft.agent_backend,
                    SshAgentBackend::OpenSsh,
                    "OpenSSH",
                );
                ui.selectable_value(
                    &mut draft.agent_backend,
                    SshAgentBackend::Pageant,
                    "Pageant",
                );
            });
        cshell_ui::form::text(ui, "身份指纹 SHA256（可选）", &mut draft.agent_identity);
    }
    command
}
