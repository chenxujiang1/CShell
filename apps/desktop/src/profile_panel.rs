use crate::profile_connection::{DesktopProfileCatalog, DesktopProfileView, ProfileClientCommand};
use cshell_domain::{
    FolderId, LocalConnectionRecord, LocalWorkingDirectory, ProfileFolder, ProfileId, ProfileKind,
    ProfileRecord, SshAgentBackend, SshAuthMethod, SshConnectionRecord, SshRoute,
    TerminalOverrides,
};
use cshell_ipc::{
    HostKeyPreviewData, ProfileChange, ProfileFolderData, ProfileImportAction,
    ProfileImportItemKind, ProfileImportPolicy, ProfileImportPreviewData, ProfileRecordData,
    SshConnectionData, profile_change,
};
use std::collections::{BTreeMap, BTreeSet};
use zeroize::Zeroize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Selected {
    Folder(FolderId),
    Profile(ProfileId),
}

#[derive(Default)]
struct LocalDraft {
    program: String,
    args: Vec<String>,
    cwd_kind: u8,
    cwd_path: String,
    env: Vec<(String, String)>,
    close_policy: cshell_domain::LocalClosePolicy,
    launch_cwd: String,
    launch_env: Vec<(String, String)>,
}

impl LocalDraft {
    fn from_connection(connection: Option<&LocalConnectionRecord>) -> Self {
        let Some(connection) = connection else {
            return Self::default();
        };
        Self {
            program: connection.program.clone(),
            args: connection.args.clone(),
            cwd_kind: match connection.cwd {
                LocalWorkingDirectory::Inherit => 0,
                LocalWorkingDirectory::Home => 1,
                LocalWorkingDirectory::Explicit { .. } => 2,
            },
            cwd_path: match &connection.cwd {
                LocalWorkingDirectory::Explicit { path } => path.clone(),
                _ => String::new(),
            },
            env: connection
                .env_overrides
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            close_policy: connection.close_policy,
            ..Self::default()
        }
    }
    fn record(&self, profile_id: ProfileId) -> Result<LocalConnectionRecord, String> {
        let env_overrides: BTreeMap<_, _> = self.env.iter().cloned().collect();
        if env_overrides.len() != self.env.len() {
            return Err("Environment names must be unique".into());
        }
        let record = LocalConnectionRecord {
            profile_id,
            close_policy: self.close_policy,
            program: self.program.clone(),
            args: self.args.clone(),
            cwd: match self.cwd_kind {
                1 => LocalWorkingDirectory::Home,
                2 => LocalWorkingDirectory::Explicit {
                    path: self.cwd_path.clone(),
                },
                _ => LocalWorkingDirectory::Inherit,
            },
            env_overrides,
        };
        cshell_application::validate_local_connection(&record)
            .map_err(|error| error.to_string())?;
        Ok(record)
    }

    fn launch_options(
        &self,
        saved: &LocalConnectionRecord,
    ) -> Result<cshell_ipc::LocalLaunchOptions, String> {
        let env_overrides: BTreeMap<_, _> = self.launch_env.iter().cloned().collect();
        if env_overrides.len() != self.launch_env.len() {
            return Err("Environment names must be unique".into());
        }
        let cwd_path = (!self.launch_cwd.is_empty()).then(|| self.launch_cwd.clone());
        let mut checked = saved.clone();
        checked.env_overrides.clone_from(&env_overrides);
        if let Some(path) = &cwd_path {
            checked.cwd = LocalWorkingDirectory::Explicit { path: path.clone() };
        }
        cshell_application::validate_local_connection(&checked)
            .map_err(|error| error.to_string())?;
        Ok(cshell_ipc::LocalLaunchOptions {
            cwd_path,
            env_overrides,
        })
    }
}

struct ProfileDraft {
    record: ProfileRecord,
    tags: String,
    ssh_host: String,
    ssh_port: u16,
    ssh_username: String,
    auth_method: SshAuthMethod,
    private_key_path: String,
    certificate_path: String,
    agent_backend: SshAgentBackend,
    agent_identity: String,
    route_kind: u8,
    proxy_host: String,
    proxy_port: u16,
    jump_profile_id: Option<ProfileId>,
    password: String,
    key_passphrase: String,
    host_key_confirmation: String,
    had_ssh_connection: bool,
    had_local_connection: bool,
    local: LocalDraft,
}

impl ProfileDraft {
    fn route(&self) -> SshRoute {
        match self.route_kind {
            1 => SshRoute::Socks5 {
                host: self.proxy_host.trim().into(),
                port: self.proxy_port,
            },
            2 => SshRoute::HttpConnect {
                host: self.proxy_host.trim().into(),
                port: self.proxy_port,
            },
            3 => SshRoute::Jump {
                profile_id: self.jump_profile_id.unwrap_or(self.record.id),
            },
            _ => SshRoute::Direct,
        }
    }
}

impl std::fmt::Debug for ProfileDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileDraft")
            .field("profile_id", &self.record.id)
            .field("auth_method", &self.auth_method)
            .field("password", &"[REDACTED]")
            .field("key_passphrase", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Drop for ProfileDraft {
    fn drop(&mut self) {
        self.password.zeroize();
        self.key_passphrase.zeroize();
    }
}

#[derive(Debug)]
pub struct ProfilePanel {
    pub open: bool,
    seen_generation: u64,
    catalog: Option<DesktopProfileCatalog>,
    preview: Option<ProfileImportPreviewData>,
    preview_source: Option<(String, ProfileImportPolicy)>,
    host_key_preview: Option<HostKeyPreviewData>,
    local_shells: Vec<cshell_ipc::LocalShellData>,
    error: Option<String>,
    launch_error: Option<String>,
    status: String,
    search: String,
    selected: Option<Selected>,
    folder_draft: Option<ProfileFolder>,
    profile_draft: Option<ProfileDraft>,
    import_path: String,
    import_policy: ProfileImportPolicy,
}

impl Default for ProfilePanel {
    fn default() -> Self {
        Self {
            open: false,
            seen_generation: 0,
            catalog: None,
            preview: None,
            preview_source: None,
            host_key_preview: None,
            local_shells: Vec::new(),
            error: None,
            launch_error: None,
            status: String::new(),
            search: String::new(),
            selected: None,
            folder_draft: None,
            profile_draft: None,
            import_path: String::new(),
            import_policy: ProfileImportPolicy::Fail,
        }
    }
}

impl ProfilePanel {
    pub fn set_launch_error(&mut self, error: Option<String>) -> bool {
        if self.launch_error == error {
            return false;
        }
        self.launch_error = error;
        true
    }

    pub fn sync(&mut self, view: &DesktopProfileView) -> bool {
        if self.seen_generation == view.generation {
            return false;
        }
        self.seen_generation = view.generation;
        if self.catalog.as_ref().map(|catalog| catalog.revision)
            != view.catalog.as_ref().map(|catalog| catalog.revision)
        {
            self.selected = None;
            self.folder_draft = None;
            self.profile_draft = None;
            self.host_key_preview = None;
        }
        self.local_shells = view.local_shells.clone();
        self.catalog = view.catalog.clone();
        self.preview = view.preview.clone();
        self.preview_source = view.preview_source.clone();
        self.host_key_preview = view.host_key_preview.clone();
        self.error = view.error.clone();
        self.status = view.status.clone();
        true
    }

    pub fn draw(&mut self, context: &egui::Context) -> Option<ProfileClientCommand> {
        if !self.open {
            return None;
        }
        let mut open = self.open;
        let mut command = None;
        let mut clear_selection = false;
        let catalog = self.catalog.clone();
        let host_key_preview = self.host_key_preview.clone();
        let local_shells = self.local_shells.clone();
        egui::Window::new("Profiles and folders")
            .open(&mut open)
            .default_size([760.0, 580.0])
            .resizable(true)
            .show(context, |ui| {
                ui.label(&self.status);
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if let Some(error) = &self.launch_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("Profile launch: {error}"));
                }
                ui.horizontal(|ui| {
                    if ui.button("Refresh").clicked() {
                        command = Some(ProfileClientCommand::Refresh);
                    }
                    if ui.button("New Profile").clicked() {
                        let record = ProfileRecord {
                            id: ProfileId::new(),
                            name: String::new(),
                            kind: ProfileKind::Ssh,
                            folder_id: None,
                            tags: BTreeSet::new(),
                            favorite: false,
                            terminal: TerminalOverrides::default(),
                        };
                        self.select_profile(record);
                    }
                    if ui.button("New folder").clicked() {
                        self.select_folder(ProfileFolder {
                            id: FolderId::new(),
                            name: String::new(),
                            parent_id: None,
                            terminal: TerminalOverrides::default(),
                        });
                    }
                });
                ui.separator();
                if let Some(catalog) = &catalog {
                    ui.label(format!(
                        "Catalog revision {} · {} folders · {} Profiles · default theme {}",
                        catalog.revision,
                        catalog.folders.len(),
                        catalog.profiles.len(),
                        catalog.defaults.theme
                    ));
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.search)
                                    .hint_text("Search names or tags"),
                            );
                            egui::ScrollArea::vertical()
                                .max_height(250.0)
                                .show(ui, |ui| {
                                    ui.heading("Folders");
                                    for folder in &catalog.folders {
                                        if !matches_search(&folder.name, &self.search) {
                                            continue;
                                        }
                                        if ui
                                            .selectable_label(
                                                self.selected == Some(Selected::Folder(folder.id)),
                                                &folder.name,
                                            )
                                            .clicked()
                                        {
                                            self.select_folder(folder.clone());
                                        }
                                    }
                                    ui.heading("Profiles");
                                    for profile in &catalog.profiles {
                                        if !matches_search(&profile.name, &self.search)
                                            && !profile
                                                .tags
                                                .iter()
                                                .any(|tag| matches_search(tag, &self.search))
                                        {
                                            continue;
                                        }
                                        let label = format!(
                                            "{}{}",
                                            if profile.favorite { "★ " } else { "" },
                                            profile.name
                                        );
                                        if ui
                                            .selectable_label(
                                                self.selected
                                                    == Some(Selected::Profile(profile.id)),
                                                label,
                                            )
                                            .clicked()
                                        {
                                            self.select_profile(profile.clone());
                                        }
                                    }
                                });
                        });
                        ui.separator();
                        ui.vertical(|ui| {
                            if let Some(draft) = &mut self.profile_draft {
                                ui.heading("Profile editor");
                                ui.horizontal(|ui| {
                                    ui.label("Name");
                                    ui.text_edit_singleline(&mut draft.record.name);
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Kind");
                                    ui.selectable_value(
                                        &mut draft.record.kind,
                                        ProfileKind::Ssh,
                                        "SSH",
                                    );
                                    ui.selectable_value(
                                        &mut draft.record.kind,
                                        ProfileKind::Local,
                                        "Local",
                                    );
                                });
                                folder_picker(
                                    ui,
                                    "Folder",
                                    &mut draft.record.folder_id,
                                    &catalog.folders,
                                    None,
                                );
                                ui.checkbox(&mut draft.record.favorite, "Favorite");
                                ui.horizontal(|ui| {
                                    ui.label("Tags");
                                    ui.text_edit_singleline(&mut draft.tags);
                                });
                                terminal_fields(ui, &mut draft.record.terminal);
                                if draft.record.kind == ProfileKind::Ssh {
                                    ui.horizontal(|ui| {
                                        ui.label("Host");
                                        ui.text_edit_singleline(&mut draft.ssh_host);
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Port");
                                        ui.add(
                                            egui::DragValue::new(&mut draft.ssh_port)
                                                .range(1..=65535),
                                        );
                                        ui.label("User");
                                        ui.text_edit_singleline(&mut draft.ssh_username);
                                    });
                                    let saved_target = catalog.profiles.iter().any(|item| item.id == draft.record.id)
                                        && catalog.ssh_connections.iter().any(|item| item.profile_id == draft.record.id);
                                    ui.label("Unknown or changed SSH host keys are blocked by default.");
                                    let saved_key_target = catalog.ssh_connections.iter().any(|item| {
                                        item.profile_id == draft.record.id
                                            && item.host == draft.ssh_host.trim()
                                            && item.port == draft.ssh_port
                                            && item.route == draft.route()
                                    });
                                    if ui.add_enabled(saved_key_target, egui::Button::new("Preview first host key")).clicked() {
                                        draft.host_key_confirmation.clear();
                                        command = Some(ProfileClientCommand::PreviewHostKey {
                                            profile_id: draft.record.id,
                                            expected_revision: catalog.revision,
                                        });
                                    }
                                    if let Some(preview) = host_key_preview.as_ref().filter(|preview| {
                                        preview.profile_id == draft.record.id.as_uuid().as_bytes()
                                            && preview.host == draft.ssh_host.trim()
                                            && preview.port == u32::from(draft.ssh_port)
                                            && saved_key_target
                                    }) {
                                        ui.label(format!("Host key algorithm: {}", preview.algorithm));
                                        ui.label(format!("SHA256 fingerprint: {}", preview.fingerprint));
                                        ui.label(format!("Public key: {}", preview.public_key_line));
                                        ui.label("Compare this fingerprint through a trusted channel. Type the exact SHA256 fingerprint below to import it.");
                                        ui.horizontal(|ui| {
                                            ui.label("Confirmed fingerprint");
                                            ui.text_edit_singleline(&mut draft.host_key_confirmation);
                                        });
                                        if ui.add_enabled(draft.host_key_confirmation == preview.fingerprint, egui::Button::new("Confirm and import host key")).clicked() {
                                            command = Some(ProfileClientCommand::ConfirmHostKey {
                                                profile_id: draft.record.id,
                                                expected_revision: catalog.revision,
                                                token: preview.token.clone(),
                                                fingerprint: std::mem::take(&mut draft.host_key_confirmation),
                                            });
                                        }
                                    }
                                    egui::ComboBox::from_label("Connection route")
                                        .selected_text(match draft.route_kind { 1 => "SOCKS5 proxy", 2 => "HTTP CONNECT proxy", 3 => "Single jump Profile", _ => "Direct" })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut draft.route_kind, 0, "Direct");
                                            ui.selectable_value(&mut draft.route_kind, 1, "SOCKS5 proxy");
                                            ui.selectable_value(&mut draft.route_kind, 2, "HTTP CONNECT proxy");
                                            ui.selectable_value(&mut draft.route_kind, 3, "Single jump Profile");
                                        });
                                    if matches!(draft.route_kind, 1 | 2) {
                                        ui.label("Unauthenticated proxy. SSH target host keys are still strictly verified.");
                                        ui.horizontal(|ui| {
                                            ui.label("Proxy host"); ui.text_edit_singleline(&mut draft.proxy_host);
                                            ui.label("Port"); ui.add(egui::DragValue::new(&mut draft.proxy_port).range(1..=65535));
                                        });
                                    } else if draft.route_kind == 3 {
                                        let selected = draft.jump_profile_id.and_then(|id| catalog.profiles.iter().find(|item| item.id == id)).map_or("Choose jump Profile", |item| item.name.as_str());
                                        egui::ComboBox::from_label("Jump Profile").selected_text(selected).show_ui(ui, |ui| {
                                            for candidate in &catalog.ssh_connections {
                                                if candidate.profile_id == draft.record.id || matches!(candidate.route, SshRoute::Jump { .. }) { continue; }
                                                if let Some(profile) = catalog.profiles.iter().find(|item| item.id == candidate.profile_id) {
                                                    ui.selectable_value(&mut draft.jump_profile_id, Some(profile.id), &profile.name);
                                                }
                                            }
                                        });
                                        ui.label("The jump uses its own saved authentication. Confirm the jump host key before previewing the target. Agent forwarding is not enabled.");
                                    }
                                    egui::ComboBox::from_label("Authentication")
                                        .selected_text(match draft.auth_method {
                                            SshAuthMethod::Password => "Password",
                                            SshAuthMethod::PrivateKey => "Private key file",
                                            SshAuthMethod::Certificate => "OpenSSH certificate",
                                            SshAuthMethod::Agent => "SSH agent",
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut draft.auth_method, SshAuthMethod::Password, "Password");
                                            ui.selectable_value(&mut draft.auth_method, SshAuthMethod::PrivateKey, "Private key file");
                                            ui.selectable_value(&mut draft.auth_method, SshAuthMethod::Certificate, "OpenSSH certificate");
                                            ui.selectable_value(&mut draft.auth_method, SshAuthMethod::Agent, "SSH agent");
                                        });
                                    match draft.auth_method {
                                        SshAuthMethod::Password => {
                                            ui.label("Password is stored in the system keychain, never in the Profile database.");
                                            ui.horizontal(|ui| {
                                                ui.label("Password");
                                                ui.add(egui::TextEdit::singleline(&mut draft.password).password(true).char_limit(4096));
                                            });
                                            ui.horizontal(|ui| {
                                                if ui.add_enabled(saved_target && !draft.password.is_empty(), egui::Button::new("Save password")).clicked() {
                                                    command = Some(ProfileClientCommand::SetPassword {
                                                        profile_id: draft.record.id,
                                                        expected_revision: catalog.revision,
                                                        password: std::mem::take(&mut draft.password).into_bytes(),
                                                    });
                                                }
                                                if ui.add_enabled(saved_target, egui::Button::new("Remove password")).clicked() {
                                                    draft.password.clear();
                                                    command = Some(ProfileClientCommand::DeletePassword {
                                                        profile_id: draft.record.id,
                                                        expected_revision: catalog.revision,
                                                    });
                                                }
                                            });
                                        }
                                        SshAuthMethod::PrivateKey | SshAuthMethod::Certificate => {
                                            ui.label("Use an absolute path to a key file readable by the daemon. Only its path is saved.");
                                            ui.horizontal(|ui| {
                                                ui.label("Private key path");
                                                ui.text_edit_singleline(&mut draft.private_key_path);
                                            });
                                            if draft.auth_method == SshAuthMethod::Certificate {
                                                ui.horizontal(|ui| {
                                                    ui.label("Certificate path");
                                                    ui.text_edit_singleline(&mut draft.certificate_path);
                                                });
                                            }
                                            ui.label("For encrypted keys, save the passphrase after saving the Profile. The passphrase stays in the system keychain.");
                                            let saved_key_target = catalog.ssh_connections.iter().any(|item| {
                                                item.profile_id == draft.record.id
                                                    && item.auth_method == draft.auth_method
                                                    && item.private_key_path.as_deref()
                                                        == Some(draft.private_key_path.trim())
                                            });
                                            ui.horizontal(|ui| {
                                                ui.label("Key passphrase");
                                                ui.add(egui::TextEdit::singleline(&mut draft.key_passphrase).password(true).char_limit(4096));
                                            });
                                            ui.horizontal(|ui| {
                                                if ui.add_enabled(saved_key_target && !draft.key_passphrase.is_empty(), egui::Button::new("Save key passphrase")).clicked() {
                                                    command = Some(ProfileClientCommand::SetKeyPassphrase {
                                                        profile_id: draft.record.id,
                                                        expected_revision: catalog.revision,
                                                        passphrase: std::mem::take(&mut draft.key_passphrase).into_bytes(),
                                                    });
                                                }
                                                if ui.add_enabled(saved_key_target, egui::Button::new("Remove key passphrase")).clicked() {
                                                    draft.key_passphrase.clear();
                                                    command = Some(ProfileClientCommand::DeleteKeyPassphrase {
                                                        profile_id: draft.record.id,
                                                        expected_revision: catalog.revision,
                                                    });
                                                }
                                            });
                                        }
                                        SshAuthMethod::Agent => {
                                            egui::ComboBox::from_label("Agent backend")
                                                .selected_text(match draft.agent_backend {
                                                    SshAgentBackend::Auto => "Auto",
                                                    SshAgentBackend::OpenSsh => "OpenSSH",
                                                    SshAgentBackend::Pageant => "Pageant",
                                                })
                                                .show_ui(ui, |ui| {
                                                    ui.selectable_value(&mut draft.agent_backend, SshAgentBackend::Auto, "Auto");
                                                    ui.selectable_value(&mut draft.agent_backend, SshAgentBackend::OpenSsh, "OpenSSH");
                                                    ui.selectable_value(&mut draft.agent_backend, SshAgentBackend::Pageant, "Pageant");
                                                });
                                            ui.horizontal(|ui| {
                                                ui.label("Identity SHA256 (optional)");
                                                ui.text_edit_singleline(&mut draft.agent_identity);
                                            });
                                        }
                                    }
                                    if ui.add_enabled(saved_target, egui::Button::new("Open saved SSH Profile")).clicked() {
                                        command = Some(ProfileClientCommand::OpenProfile(draft.record.id));
                                    }
                                }
                                if draft.record.kind == ProfileKind::Local {
                                    if draw_local_fields(ui, &mut draft.local, &local_shells) {
                                        command = Some(ProfileClientCommand::DiscoverLocalShells);
                                    }
                                    let saved_local = catalog.local_connections.iter().any(|item| item.profile_id == draft.record.id);
                                    if ui.add_enabled(saved_local, egui::Button::new("Open saved Local Profile")).clicked() {
                                        command = Some(ProfileClientCommand::OpenProfile(draft.record.id));
                                    }
                                    ui.collapsing("Open once with overrides", |ui| {
                                        ui.label("Uses the saved Profile. These overrides are not saved; New Shell uses the saved configuration.");
                                        ui.horizontal(|ui| {
                                            ui.label("Directory (empty uses Profile)");
                                            ui.text_edit_singleline(&mut draft.local.launch_cwd);
                                        });
                                        ui.label("Use non-secret values. Environment overrides are sent to the local process.");
                                        draw_environment_rows(ui, &mut draft.local.launch_env);
                                        if let Some(saved) = catalog.local_connections.iter().find(|item| item.profile_id == draft.record.id) {
                                            match draft.local.launch_options(saved) {
                                                Ok(options) => {
                                                    if ui.button("Open with these overrides").clicked() {
                                                        command = Some(ProfileClientCommand::OpenLocal(draft.record.id, options));
                                                        draft.local.launch_cwd.clear();
                                                        draft.local.launch_env.clear();
                                                    }
                                                }
                                                Err(error) => { ui.colored_label(egui::Color32::YELLOW, error); }
                                            }
                                        }
                                    });
                                }
                                let local_target = draft.local.record(draft.record.id);
                                if draft.record.kind == ProfileKind::Local && let Err(error) = &local_target {
                                    ui.colored_label(egui::Color32::YELLOW, error);
                                }
                                let target_incomplete = draft.record.kind == ProfileKind::Ssh
                                    && (draft.ssh_host.trim().is_empty()
                                        != draft.ssh_username.trim().is_empty());
                                if target_incomplete {
                                    ui.colored_label(
                                        egui::Color32::YELLOW,
                                        "Enter both host and user, or clear both.",
                                    );
                                }
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(
                                            !target_incomplete && (draft.record.kind != ProfileKind::Local || local_target.is_ok()),
                                            egui::Button::new("Save Profile"),
                                        )
                                        .clicked()
                                    {
                                        draft.record.tags = draft
                                            .tags
                                            .split(',')
                                            .map(str::trim)
                                            .filter(|tag| !tag.is_empty())
                                            .map(str::to_owned)
                                            .collect();
                                        let mut changes = vec![ProfileChange {
                                            change: Some(profile_change::Change::UpsertProfile(
                                                ProfileRecordData::from(&draft.record),
                                            )),
                                        }];
                                        if draft.record.kind == ProfileKind::Ssh
                                            && !draft.ssh_host.trim().is_empty()
                                        {
                                            let target = SshConnectionRecord {
                                                profile_id: draft.record.id,
                                                host: draft.ssh_host.trim().to_owned(),
                                                port: draft.ssh_port,
                                                username: draft.ssh_username.trim().to_owned(),
                                                auth_method: draft.auth_method,
                                                private_key_path: optional_text(&draft.private_key_path),
                                                certificate_path: optional_text(&draft.certificate_path),
                                                agent_backend: draft.agent_backend,
                                                agent_identity: optional_text(&draft.agent_identity),
                                                route: draft.route(),
                                            };
                                            changes.push(ProfileChange {
                                                change: Some(
                                                    profile_change::Change::UpsertSshConnection(
                                                        SshConnectionData::from(&target),
                                                    ),
                                                ),
                                            });
                                        } else if draft.had_ssh_connection {
                                            changes.push(ProfileChange {
                                                change: Some(
                                                    profile_change::Change::RemoveSshConnection(
                                                        draft
                                                            .record
                                                            .id
                                                            .as_uuid()
                                                            .as_bytes()
                                                            .to_vec(),
                                                    ),
                                                ),
                                            });
                                        }
                                        if draft.record.kind == ProfileKind::Local {
                                            if let Ok(target) = &local_target {
                                                changes.push(ProfileChange { change: Some(profile_change::Change::UpsertLocalConnection(cshell_ipc::LocalConnectionData::from(target))) });
                                            }
                                        } else if draft.had_local_connection {
                                            changes.push(ProfileChange { change: Some(profile_change::Change::RemoveLocalConnection(draft.record.id.as_uuid().as_bytes().to_vec())) });
                                        }
                                        command = Some(ProfileClientCommand::Apply {
                                            expected_revision: catalog.revision,
                                            changes,
                                        });
                                    }
                                    if catalog
                                        .profiles
                                        .iter()
                                        .any(|item| item.id == draft.record.id)
                                        && ui.button("Delete").clicked()
                                    {
                                        command = Some(ProfileClientCommand::Apply {
                                            expected_revision: catalog.revision,
                                            changes: vec![ProfileChange {
                                                change: Some(
                                                    profile_change::Change::RemoveProfile(
                                                        draft
                                                            .record
                                                            .id
                                                            .as_uuid()
                                                            .as_bytes()
                                                            .to_vec(),
                                                    ),
                                                ),
                                            }],
                                        });
                                        clear_selection = true;
                                    }
                                });
                            } else if let Some(draft) = &mut self.folder_draft {
                                ui.heading("Folder editor");
                                ui.horizontal(|ui| {
                                    ui.label("Name");
                                    ui.text_edit_singleline(&mut draft.name);
                                });
                                folder_picker(
                                    ui,
                                    "Parent",
                                    &mut draft.parent_id,
                                    &catalog.folders,
                                    Some(draft.id),
                                );
                                terminal_fields(ui, &mut draft.terminal);
                                ui.horizontal(|ui| {
                                    if ui.button("Save folder").clicked() {
                                        command = Some(ProfileClientCommand::Apply {
                                            expected_revision: catalog.revision,
                                            changes: vec![ProfileChange {
                                                change: Some(profile_change::Change::UpsertFolder(
                                                    ProfileFolderData::from(&*draft),
                                                )),
                                            }],
                                        });
                                    }
                                    if catalog.folders.iter().any(|item| item.id == draft.id)
                                        && ui.button("Delete").clicked()
                                    {
                                        command = Some(ProfileClientCommand::Apply {
                                            expected_revision: catalog.revision,
                                            changes: vec![ProfileChange {
                                                change: Some(profile_change::Change::RemoveFolder(
                                                    draft.id.as_uuid().as_bytes().to_vec(),
                                                )),
                                            }],
                                        });
                                        clear_selection = true;
                                    }
                                });
                            } else {
                                ui.label("Select a Profile or folder to edit.");
                            }
                        });
                    });
                }
                ui.separator();
                ui.collapsing("Import CShell Profile JSON", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("File");
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut self.import_path)
                                    .desired_width(400.0)
                                    .hint_text("Path to JSON file"),
                            )
                            .changed()
                        {
                            self.preview = None;
                        }
                    });
                    let prior_policy = self.import_policy;
                    egui::ComboBox::from_label("On conflict")
                        .selected_text(match self.import_policy {
                            ProfileImportPolicy::Fail => "Stop",
                            ProfileImportPolicy::Skip => "Skip",
                            ProfileImportPolicy::Replace => "Replace",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.import_policy,
                                ProfileImportPolicy::Fail,
                                "Stop",
                            );
                            ui.selectable_value(
                                &mut self.import_policy,
                                ProfileImportPolicy::Skip,
                                "Skip",
                            );
                            ui.selectable_value(
                                &mut self.import_policy,
                                ProfileImportPolicy::Replace,
                                "Replace",
                            );
                        });
                    if self.import_policy != prior_policy {
                        self.preview = None;
                    }
                    if ui.button("Preview import").clicked() {
                        command = Some(ProfileClientCommand::PreviewImport {
                            path: self.import_path.clone(),
                            policy: self.import_policy,
                        });
                    }
                    if let Some(preview) = self.preview.as_ref().filter(|_| {
                        self.preview_source.as_ref().is_some_and(|(path, policy)| {
                            path == &self.import_path && *policy == self.import_policy
                        })
                    }) {
                        ui.label(format!(
                            "{} changes from revision {}",
                            preview.change_count, preview.base_revision
                        ));
                        egui::ScrollArea::vertical()
                            .max_height(130.0)
                            .show(ui, |ui| {
                                for item in &preview.items {
                                    let action = match ProfileImportAction::try_from(item.action) {
                                        Ok(ProfileImportAction::Create) => "Create",
                                        Ok(ProfileImportAction::Skip) => "Skip",
                                        Ok(ProfileImportAction::Replace) => "Replace",
                                        Ok(ProfileImportAction::Conflict) => "Conflict",
                                        Err(_) => "Unknown",
                                    };
                                    let kind = match ProfileImportItemKind::try_from(item.kind) {
                                        Ok(ProfileImportItemKind::Folder) => "Folder",
                                        Ok(ProfileImportItemKind::Profile) => "Profile",
                                        Err(_) => "Item",
                                    };
                                    ui.label(format!("{kind} {}: {action}", item.name));
                                }
                            });
                        let current = catalog
                            .as_ref()
                            .is_some_and(|value| value.revision == preview.base_revision);
                        if ui
                            .add_enabled(
                                preview.can_commit && current,
                                egui::Button::new("Commit import"),
                            )
                            .clicked()
                        {
                            command = Some(ProfileClientCommand::CommitImport {
                                expected_revision: preview.base_revision,
                            });
                        }
                        if !current {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "Catalog changed; preview again.",
                            );
                        }
                    }
                });
            });
        self.open = open;
        if clear_selection {
            self.selected = None;
            self.profile_draft = None;
            self.folder_draft = None;
        }
        command
    }

    fn select_profile(&mut self, record: ProfileRecord) {
        let connection = self.catalog.as_ref().and_then(|catalog| {
            catalog
                .ssh_connections
                .iter()
                .find(|connection| connection.profile_id == record.id)
        });
        let local = self.catalog.as_ref().and_then(|catalog| {
            catalog
                .local_connections
                .iter()
                .find(|connection| connection.profile_id == record.id)
        });
        self.selected = Some(Selected::Profile(record.id));
        self.profile_draft = Some(ProfileDraft {
            tags: record.tags.iter().cloned().collect::<Vec<_>>().join(", "),
            ssh_host: connection.map_or_else(String::new, |value| value.host.clone()),
            ssh_port: connection.map_or(22, |value| value.port),
            ssh_username: connection.map_or_else(String::new, |value| value.username.clone()),
            auth_method: connection.map_or(SshAuthMethod::Password, |value| value.auth_method),
            private_key_path: connection
                .and_then(|value| value.private_key_path.clone())
                .unwrap_or_default(),
            certificate_path: connection
                .and_then(|value| value.certificate_path.clone())
                .unwrap_or_default(),
            agent_backend: connection.map_or(SshAgentBackend::Auto, |value| value.agent_backend),
            agent_identity: connection
                .and_then(|value| value.agent_identity.clone())
                .unwrap_or_default(),
            password: String::new(),
            key_passphrase: String::new(),
            host_key_confirmation: String::new(),
            route_kind: connection.map_or(0, |target| match target.route {
                SshRoute::Direct => 0,
                SshRoute::Socks5 { .. } => 1,
                SshRoute::HttpConnect { .. } => 2,
                SshRoute::Jump { .. } => 3,
            }),
            proxy_host: connection
                .and_then(|target| match &target.route {
                    SshRoute::Socks5 { host, .. } | SshRoute::HttpConnect { host, .. } => {
                        Some(host.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default(),
            proxy_port: connection
                .and_then(|target| match target.route {
                    SshRoute::Socks5 { port, .. } | SshRoute::HttpConnect { port, .. } => {
                        Some(port)
                    }
                    _ => None,
                })
                .unwrap_or(1080),
            jump_profile_id: connection.and_then(|target| match target.route {
                SshRoute::Jump { profile_id } => Some(profile_id),
                _ => None,
            }),
            had_ssh_connection: connection.is_some(),
            had_local_connection: local.is_some(),
            local: LocalDraft::from_connection(local),
            record,
        });
        self.folder_draft = None;
    }

    fn select_folder(&mut self, folder: ProfileFolder) {
        self.selected = Some(Selected::Folder(folder.id));
        self.folder_draft = Some(folder);
        self.profile_draft = None;
    }
}

fn matches_search(value: &str, search: &str) -> bool {
    value.to_lowercase().contains(&search.trim().to_lowercase())
}

fn folder_picker(
    ui: &mut egui::Ui,
    label: &str,
    selected: &mut Option<FolderId>,
    folders: &[ProfileFolder],
    exclude: Option<FolderId>,
) {
    let name = selected
        .and_then(|id| {
            folders
                .iter()
                .find(|folder| folder.id == id)
                .map(|folder| folder.name.as_str())
        })
        .unwrap_or("Root");
    egui::ComboBox::from_label(label)
        .selected_text(name)
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, "Root");
            for folder in folders {
                if Some(folder.id) != exclude {
                    ui.selectable_value(selected, Some(folder.id), &folder.name);
                }
            }
        });
}

fn terminal_fields(ui: &mut egui::Ui, terminal: &mut TerminalOverrides) {
    let mut terminal_type = terminal.terminal_type.clone().unwrap_or_default();
    let mut theme = terminal.theme.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label("Terminal type");
        if ui.text_edit_singleline(&mut terminal_type).changed() {
            terminal.terminal_type = optional_text(&terminal_type);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Theme");
        if ui.text_edit_singleline(&mut theme).changed() {
            terminal.theme = optional_text(&theme);
        }
    });
    egui::ComboBox::from_label("Logging")
        .selected_text(match terminal.logging {
            None => "Inherit",
            Some(true) => "On",
            Some(false) => "Off",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut terminal.logging, None, "Inherit");
            ui.selectable_value(&mut terminal.logging, Some(true), "On");
            ui.selectable_value(&mut terminal.logging, Some(false), "Off");
        });
}

fn optional_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn draw_local_fields(
    ui: &mut egui::Ui,
    draft: &mut LocalDraft,
    shells: &[cshell_ipc::LocalShellData],
) -> bool {
    ui.heading("Local program");
    egui::ComboBox::from_label("When closing this view")
        .selected_text(match draft.close_policy {
            cshell_domain::LocalClosePolicy::KeepAlive => "Keep session running",
            cshell_domain::LocalClosePolicy::TerminateOnViewClose => "Terminate the process",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(
                &mut draft.close_policy,
                cshell_domain::LocalClosePolicy::KeepAlive,
                "Keep session running",
            );
            ui.selectable_value(
                &mut draft.close_policy,
                cshell_domain::LocalClosePolicy::TerminateOnViewClose,
                "Terminate the process",
            );
        });
    ui.label("Applies when explicitly closing a view. Disconnects and application exit keep the session running.");
    let mut refresh = false;
    ui.horizontal(|ui| {
        egui::ComboBox::from_label("Installed shells")
            .selected_text("Choose a shell")
            .show_ui(ui, |ui| {
                for shell in shells {
                    if ui.selectable_label(false, &shell.name).clicked() {
                        draft.program.clone_from(&shell.program);
                        draft.args.clone_from(&shell.args);
                    }
                }
            });
        refresh = ui.button("Refresh shells").clicked();
    });
    ui.horizontal(|ui| {
        ui.label("Program");
        ui.text_edit_singleline(&mut draft.program);
    });
    ui.label("Arguments are passed separately, exactly as entered. No command-line splitting.");
    ui.collapsing("Arguments", |ui| {
        let mut remove = None;
        for (index, arg) in draft.args.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}", index + 1));
                ui.text_edit_singleline(arg);
                if ui.button("Remove").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            draft.args.remove(index);
        }
        if ui
            .add_enabled(draft.args.len() < 256, egui::Button::new("Add argument"))
            .clicked()
        {
            draft.args.push(String::new());
        }
    });
    egui::ComboBox::from_label("Working directory")
        .selected_text(match draft.cwd_kind {
            1 => "Home",
            2 => "Explicit directory",
            _ => "Inherit daemon directory",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut draft.cwd_kind, 0, "Inherit daemon directory");
            ui.selectable_value(&mut draft.cwd_kind, 1, "Home");
            ui.selectable_value(&mut draft.cwd_kind, 2, "Explicit directory");
        });
    if draft.cwd_kind == 2 {
        ui.horizontal(|ui| {
            ui.label("Directory");
            ui.text_edit_singleline(&mut draft.cwd_path);
        });
    }
    ui.label("The directory applies to the local process. For WSL, set its Linux directory with WSL arguments.");
    ui.collapsing("Environment overrides", |ui| {
        ui.label(
            "Values are saved as plain configuration. Use these fields for non-secret values.",
        );
        draw_environment_rows(ui, &mut draft.env);
    });
    refresh
}

fn draw_environment_rows(ui: &mut egui::Ui, env: &mut Vec<(String, String)>) {
    ui.label("CSHELL_ environment names are reserved.");
    let mut remove = None;
    for (index, (key, value)) in env.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.text_edit_singleline(key);
            ui.label("=");
            ui.text_edit_singleline(value);
            if ui.button("Remove").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        env.remove(index);
    }
    if ui
        .add_enabled(
            env.len() < 128,
            egui::Button::new("Add environment override"),
        )
        .clicked()
    {
        env.push((String::new(), String::new()));
    }
}
