use crate::profile_connection::{DesktopProfileCatalog, DesktopProfileView, ProfileClientCommand};
use cshell_domain::ProfileId;
use cshell_ipc::{
    CredentialPackagePreview, ProfileImportAction, ProfileImportPolicy, VaultOperation as Op,
    VaultRequest,
};
use std::collections::BTreeSet;
use zeroize::{Zeroize, Zeroizing};

pub struct CredentialPackagePanel {
    pub open: bool,
    path: String,
    importing: bool,
    selected: BTreeSet<ProfileId>,
    include_keys: bool,
    policy: ProfileImportPolicy,
    password: Zeroizing<String>,
    confirmation: Zeroizing<String>,
    preview: Option<CredentialPackagePreview>,
    seen_result: u64,
    status: String,
}

impl Default for CredentialPackagePanel {
    fn default() -> Self {
        Self {
            open: false,
            path: String::new(),
            importing: false,
            selected: BTreeSet::new(),
            include_keys: false,
            policy: ProfileImportPolicy::Fail,
            password: Zeroizing::new(String::new()),
            confirmation: Zeroizing::new(String::new()),
            preview: None,
            seen_result: 0,
            status: String::new(),
        }
    }
}
impl CredentialPackagePanel {
    pub fn clear(&mut self) {
        self.password.zeroize();
        self.confirmation.zeroize();
        self.preview = None;
    }
    pub fn sync(&mut self, view: &DesktopProfileView) {
        if self.seen_result != view.package_generation {
            self.seen_result = view.package_generation;
            self.preview = if self.open
                && self.importing
                && view.package_source.as_ref().is_some_and(|(path, policy)| {
                    path == self.path.trim() && *policy == self.policy as i32
                }) {
                view.package_preview.clone()
            } else {
                None
            };
            self.status = view.package_status.clone();
            if view.package_preview.is_some() && self.preview.is_none() {
                self.clear();
                self.status = "Preview source changed; preview the package again".into();
            }
        }
        if self.preview.as_ref().is_some_and(|preview| {
            view.catalog.as_ref().map(|c| c.revision) != Some(preview.profile_revision)
                || !view.vault.as_ref().is_some_and(|v| {
                    v.unlocked
                        && v.revision == preview.vault_revision
                        && v.lock_generation == preview.lock_generation
                })
        }) {
            self.clear();
            self.status = "Saved state changed; preview the package again".into();
        }
    }
    pub fn draw(
        &mut self,
        context: &egui::Context,
        catalog: Option<&DesktopProfileCatalog>,
        vault: Option<&cshell_ipc::VaultResponse>,
    ) -> Option<ProfileClientCommand> {
        if !self.open {
            return None;
        }
        let mut open = self.open;
        let mut command = None;
        cshell_ui::form::dialog(&context, "Encrypted credential package").open(&mut open).default_width(620.0).show(context, |ui| {
            ui.label("Use an independent package password. Keep the public Profile JSON separately.");
            ui.label("Format v1: Argon2id + AES-256-GCM. Losing the package password prevents recovery.");
            ui.label(&self.status);
            let previous = self.importing;
            ui.horizontal_wrapped(|ui| { ui.selectable_value(&mut self.importing, false, "Export credentials"); ui.selectable_value(&mut self.importing, true, "Import credentials"); });
            if previous != self.importing { self.clear(); }
            ui.label(if self.importing { "Absolute package file path" } else { "Absolute destination path (new file)" });
            if ui.text_edit_singleline(&mut self.path).changed() { self.preview = None; }
            let build = |operation| { let mut request = VaultRequest::new(operation); request.expected_profile_revision = catalog.map_or(0, |c| c.revision); request.expected_vault_revision = vault.map_or(0, |v| v.revision); request.expected_lock_generation = vault.map_or(0, |v| v.lock_generation); request };
            let path_valid = std::path::Path::new(self.path.trim()).is_absolute();
            let available = vault.is_some_and(|v| !v.exists || v.unlocked);
            if !self.importing {
                ui.label("Select up to 32 saved SSH Profiles. Passwords and key passphrases are included when present.");
                if let Some(catalog) = catalog {
                    self.selected.retain(|id| catalog.ssh_connections.iter().any(|t| t.profile_id == *id));
                    egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                        for profile in &catalog.profiles {
                            let Some(target) = catalog.ssh_connections.iter().find(|t| t.profile_id == profile.id) else { continue; };
                            let mut selected = self.selected.contains(&profile.id);
                            if ui.checkbox(&mut selected, &profile.name).changed() { if selected { self.selected.insert(profile.id); } else { self.selected.remove(&profile.id); } }
                            ui.small(format!("{}@{}:{}", target.username, target.host, target.port));
                        }
                    });
                }
                ui.checkbox(&mut self.include_keys, "Include private keys already imported into encrypted Vault");
                ui.label("External key files are not included. Agent credentials and trust records are not transferable.");
                ui.label("New package password (8–1024 bytes)");
                crate::secret_edit::edit(ui, &mut self.password, "package-export-password", 1024);
                ui.label("Confirm package password");
                crate::secret_edit::edit(ui, &mut self.confirmation, "package-export-confirmation", 1024);
                let ready = available && catalog.is_some() && path_valid && !self.selected.is_empty() && self.selected.len() <= cshell_ipc::MAX_CREDENTIAL_PACKAGE_PROFILES && self.password.len() >= 8 && *self.password == *self.confirmation;
                if ui.add_enabled(ready, egui::Button::new("Export encrypted package")).clicked() {
                    let mut request = build(Op::ExportPackage); request.secret = self.password.as_bytes().to_vec(); request.selected_profiles = self.selected.iter().map(|id| id.as_uuid().as_bytes().to_vec()).collect(); request.include_private_keys = self.include_keys;
                    self.clear(); command = Some(ProfileClientCommand::CredentialPackage { path: self.path.trim().into(), request });
                }
            } else {
                ui.label("First import the public Profile JSON, then create and unlock encrypted Vault. Target, authentication and complete jump route must match.");
                ui.label("The preview lasts 2 minutes. Confirmation requires the package password again. Restored Profiles use encrypted Vault; native keychain records are retained.");
                let before = self.policy;
                cshell_ui::form::combo(ui, "Existing credential group").truncate().selected_text(format!("{:?}", self.policy)).show_ui(ui, |ui| { cshell_ui::form::menu(ui); for policy in [ProfileImportPolicy::Fail, ProfileImportPolicy::Skip, ProfileImportPolicy::Replace] { ui.selectable_value(&mut self.policy, policy, format!("{policy:?}")); } });
                if before != self.policy { self.preview = None; }
                ui.label(if self.preview.is_some() { "Re-enter package password to confirm" } else { "Package password" });
                crate::secret_edit::edit(ui, &mut self.password, "package-import-password", 1024);
                let ready = catalog.is_some() && vault.is_some_and(|v| v.exists && v.unlocked) && path_valid && !self.password.is_empty();
                if ui.add_enabled(ready, egui::Button::new("Preview credential import")).clicked() {
                    let mut request = build(Op::PreviewPackage); request.secret = self.password.as_bytes().to_vec(); request.package_policy = self.policy as i32;
                    self.clear(); command = Some(ProfileClientCommand::CredentialPackage { path: self.path.trim().into(), request });
                }
                if let Some(preview) = &self.preview {
                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for item in &preview.items {
                            ui.label(format!("{} — {} — {:?}", item.name, item.target, ProfileImportAction::try_from(item.action).ok()));
                            let kinds: Vec<_> = item.kinds.iter().map(|kind| match kind { 1 => "password", 2 => "key passphrase", 3 => "private key", _ => "unknown" }).collect();
                            ui.label(kinds.join(", ")); ui.label(&item.reason);
                            if item.kinds.contains(&2) && !item.kinds.contains(&3) { ui.label("Private key is not in this package; import it separately before connecting."); }
                            ui.separator();
                        }
                    });
                    if ui.add_enabled(ready && preview.can_commit && preview.token.len() == 32, egui::Button::new("Confirm credential import")).clicked() {
                        let mut request = build(Op::CommitPackage); request.expected_profile_revision = preview.profile_revision; request.expected_vault_revision = preview.vault_revision; request.expected_lock_generation = preview.lock_generation; request.preview_token = preview.token.clone(); request.package_policy = self.policy as i32; request.secret = self.password.as_bytes().to_vec();
                        self.clear(); command = Some(ProfileClientCommand::CredentialPackage { path: String::new(), request });
                    }
                }
            }
        });
        if !open {
            self.clear();
        }
        self.open = open;
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draw(
        panel: &mut CredentialPackagePanel,
        context: &egui::Context,
        catalog: &DesktopProfileCatalog,
        vault: &cshell_ipc::VaultResponse,
        events: Vec<egui::Event>,
    ) -> (egui::accesskit::TreeUpdate, Option<ProfileClientCommand>) {
        let mut command = None;
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 1400.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                command = panel.draw(ui.ctx(), Some(catalog), Some(vault));
            },
        );
        output.textures_delta.clear();
        (
            output
                .platform_output
                .accesskit_update
                .unwrap_or_else(|| panic!("missing tree")),
            command,
        )
    }
    fn click(tree: &egui::accesskit::TreeUpdate, label: &str) -> Vec<egui::Event> {
        let bounds = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .unwrap_or_else(|| panic!("missing {label}"))
            .1
            .bounds()
            .unwrap_or_else(|| panic!("missing bounds"));
        let pos = egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
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
    fn actual_export_click_requires_selection_and_clears_password_and_confirmation()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let id = ProfileId::new();
        let catalog = DesktopProfileCatalog {
            revision: 5,
            defaults: Default::default(),
            folders: Vec::new(),
            profiles: vec![cshell_domain::ProfileRecord {
                id,
                name: "Selected server".into(),
                kind: cshell_domain::ProfileKind::Ssh,
                folder_id: None,
                tags: Default::default(),
                favorite: false,
                terminal: Default::default(),
            }],
            ssh_connections: vec![cshell_domain::SshConnectionRecord {
                options: Default::default(),
                profile_id: id,
                host: "example.test".into(),
                port: 22,
                username: "alice".into(),
                auth_method: Default::default(),
                private_key_path: None,
                certificate_path: None,
                agent_backend: Default::default(),
                agent_identity: None,
                route: Default::default(),
            }],
            local_connections: Vec::new(),
            themes: Vec::new(),
        };
        let vault = cshell_ipc::VaultResponse {
            revision: 6,
            lock_generation: 7,
            ..Default::default()
        };
        let mut panel = CredentialPackagePanel {
            open: true,
            path: temp
                .path()
                .join("credentials.cshcpk")
                .to_str()
                .ok_or("path")?
                .into(),
            password: Zeroizing::new("SECRET_SENTINEL".into()),
            confirmation: Zeroizing::new("SECRET_SENTINEL".into()),
            ..Default::default()
        };
        let context = egui::Context::default();
        context.enable_accesskit();
        draw(&mut panel, &context, &catalog, &vault, Vec::new());
        let (tree, _) = draw(&mut panel, &context, &catalog, &vault, Vec::new());
        assert!(
            draw(
                &mut panel,
                &context,
                &catalog,
                &vault,
                click(&tree, "Export encrypted package")
            )
            .1
            .is_none()
        );
        let (tree, _) = draw(&mut panel, &context, &catalog, &vault, Vec::new());
        draw(
            &mut panel,
            &context,
            &catalog,
            &vault,
            click(&tree, "Selected server"),
        );
        let (tree, _) = draw(&mut panel, &context, &catalog, &vault, Vec::new());
        let command = draw(
            &mut panel,
            &context,
            &catalog,
            &vault,
            click(&tree, "Export encrypted package"),
        )
        .1
        .ok_or("missing command")?;
        assert!(!format!("{command:?}").contains("SECRET_SENTINEL"));
        let ProfileClientCommand::CredentialPackage { request, .. } = command else {
            return Err("wrong command".into());
        };
        assert_eq!(
            request.selected_profiles,
            vec![id.as_uuid().as_bytes().to_vec()]
        );
        assert_eq!(request.expected_profile_revision, 5);
        assert_eq!(request.expected_vault_revision, 6);
        assert_eq!(request.expected_lock_generation, 7);
        assert!(!request.include_private_keys);
        assert_eq!(request.secret, b"SECRET_SENTINEL");
        assert!(panel.password.is_empty());
        assert!(panel.confirmation.is_empty());
        panel.importing = true;
        panel.preview = Some(CredentialPackagePreview {
            token: vec![9; 32],
            profile_revision: 5,
            vault_revision: 6,
            lock_generation: 7,
            can_commit: true,
            items: vec![cshell_ipc::CredentialPackageItem {
                name: "server".into(),
                kinds: vec![1],
                ..Default::default()
            }],
        });
        let unlocked = cshell_ipc::VaultResponse {
            exists: true,
            unlocked: true,
            revision: 6,
            lock_generation: 7,
            ..Default::default()
        };
        let (tree, _) = draw(&mut panel, &context, &catalog, &unlocked, Vec::new());
        assert!(
            draw(
                &mut panel,
                &context,
                &catalog,
                &unlocked,
                click(&tree, "Confirm credential import")
            )
            .1
            .is_none()
        );
        panel.password = Zeroizing::new("SECRET_SENTINEL".into());
        let (tree, _) = draw(&mut panel, &context, &catalog, &unlocked, Vec::new());
        let command = draw(
            &mut panel,
            &context,
            &catalog,
            &unlocked,
            click(&tree, "Confirm credential import"),
        )
        .1
        .ok_or("missing confirmation")?;
        let ProfileClientCommand::CredentialPackage { request, .. } = command else {
            return Err("wrong confirmation".into());
        };
        assert_eq!(request.operation, Op::CommitPackage as i32);
        assert_eq!(request.preview_token, vec![9; 32]);
        assert_eq!(request.expected_profile_revision, 5);
        assert_eq!(request.expected_vault_revision, 6);
        assert_eq!(request.expected_lock_generation, 7);
        assert!(panel.preview.is_none());
        assert!(panel.password.is_empty());
        Ok(())
    }
    #[test]
    fn preview_requires_password_reentry_and_focus_clear_does_not_restore_old_token() {
        let mut panel = CredentialPackagePanel {
            password: Zeroizing::new("SECRET_SENTINEL".into()),
            preview: Some(CredentialPackagePreview {
                token: vec![1; 32],
                items: Vec::new(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let view = DesktopProfileView {
            package_generation: 1,
            package_preview: panel.preview.clone(),
            ..Default::default()
        };
        panel.sync(&view);
        assert!(panel.preview.is_none());
        assert!(panel.password.is_empty());
        panel.clear();
        panel.sync(&view);
        assert!(panel.preview.is_none());
    }
}
