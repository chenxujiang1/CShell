mod terminal_settings;
use crate::profile_connection::{
    DesktopProfileCatalog, DesktopProfileView, ProfileClientCommand, ProfileImportFormat,
    ProfileImportSource,
};
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
use zeroize::{Zeroize, Zeroizing};

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
            return Self { cwd_kind: 1, ..Self::default() };
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
    ssh_options: cshell_domain::SshConnectionOptions,
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

pub struct ProfilePanel {
    pub open: bool,
    vault_panel: crate::vault_panel::VaultPanel,
    package_panel: crate::credential_package::CredentialPackagePanel,
    export_panel: crate::profile_export::ProfileExportPanel,
    seen_generation: u64,
    catalog: Option<DesktopProfileCatalog>,
    preview: Option<ProfileImportPreviewData>,
    preview_source: Option<ProfileImportSource>,
    import_notices: Vec<String>,
    import_digest: Option<[u8; 32]>,
    import_omissions_reviewed: bool,
    host_key_preview: Option<HostKeyPreviewData>,
    host_key_prompt: Option<crate::host_key_prompt::HostKeyPrompt>,
    local_shells: Vec<cshell_ipc::LocalShellData>,
    error: Option<String>,
    launch_error: Option<String>,
    cancelled_connection: Option<ProfileId>,
    status: String,
    credential_pending: bool,
    search: String,
    page: u8,
    editor_tab: u8,
    selected: Option<Selected>,
    pending_ssh_entry: bool,
    pending_new_ssh: bool,
    folder_draft: Option<ProfileFolder>,
    profile_draft: Option<ProfileDraft>,
    import_path: String,
    import_policy: ProfileImportPolicy,
    import_format: ProfileImportFormat,
    import_username: String,
}

impl std::fmt::Debug for ProfilePanel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfilePanel")
            .field("open", &self.open)
            .field("seen_generation", &self.seen_generation)
            .field("selected", &self.selected)
            .field("has_error", &self.error.is_some())
            .field("has_launch_error", &self.launch_error.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for ProfilePanel {
    fn default() -> Self {
        Self {
            open: false,
            vault_panel: crate::vault_panel::VaultPanel::default(),
            package_panel: crate::credential_package::CredentialPackagePanel::default(),
            export_panel: crate::profile_export::ProfileExportPanel::default(),
            seen_generation: 0,
            catalog: None,
            preview: None,
            preview_source: None,
            import_notices: Vec::new(),
            import_digest: None,
            import_omissions_reviewed: false,
            host_key_preview: None,
            host_key_prompt: None,
            local_shells: Vec::new(),
            error: None,
            launch_error: None,
            cancelled_connection: None,
            status: String::new(),
            credential_pending: false,
            search: String::new(),
            page: 0,
            editor_tab: 0,
            selected: None,
            pending_ssh_entry: false,
            pending_new_ssh: false,
            folder_draft: None,
            profile_draft: None,
            import_path: String::new(),
            import_policy: ProfileImportPolicy::Fail,
            import_format: ProfileImportFormat::Catalog,
            import_username: std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
                .unwrap_or_default(),
        }
    }
}

impl ProfilePanel {
    pub fn is_dialog_open(&self) -> bool { self.open || self.host_key_prompt.is_some() || self.vault_panel.open || self.package_panel.open }
    pub fn open_credentials(&mut self) -> ProfileClientCommand {
        self.vault_panel.open = true;
        ProfileClientCommand::Vault(cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::Status))
    }
    pub fn owns_native_key_picker(&self) -> bool { self.vault_panel.owns_native_key_picker() }

    pub fn clear_secrets(&mut self) {
        if let Some(draft) = &mut self.profile_draft {
            draft.password.zeroize();
            draft.key_passphrase.zeroize();
        }
        self.vault_panel.clear();
        self.package_panel.clear();
    }
    pub fn open_new_ssh_profile(&mut self) {
        self.open = true;
        self.pending_ssh_entry = false;
        self.pending_new_ssh = self.catalog.is_none();
        if !self.pending_new_ssh {
            self.new_ssh_profile();
        }
    }

    pub fn open_profile_editor(&mut self, id: ProfileId) -> bool {
        let Some(profile) = self.catalog.as_ref().and_then(|catalog| {
            catalog
                .profiles
                .iter()
                .find(|profile| profile.id == id)
                .cloned()
        }) else {
            return false;
        };
        self.open = true;
        self.select_profile(profile);
        true
    }

    pub fn open_ssh_connections(&mut self) {
        self.open = true;
        self.pending_new_ssh = false;
        if self.catalog.is_none() {
            self.pending_ssh_entry = true;
        } else {
            self.select_ssh_entry();
        }
    }

    fn select_ssh_entry(&mut self) {
        self.pending_ssh_entry = false;
        let saved = self.catalog.as_ref().and_then(|catalog| {
            catalog.ssh_connections.iter().find_map(|connection| {
                catalog
                    .profiles
                    .iter()
                    .find(|profile| profile.id == connection.profile_id)
                    .cloned()
            })
        });
        if let Some(profile) = saved {
            self.select_profile(profile);
        } else {
            self.new_ssh_profile();
        }
    }

    fn new_ssh_profile(&mut self) {
        self.select_profile(ProfileRecord {
            id: ProfileId::new(),
            name: String::new(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::new(),
            favorite: false,
            terminal: TerminalOverrides::default(),
        });
    }

    pub fn set_launch_error(&mut self, error: Option<String>, profile_id: Option<ProfileId>) -> bool {
        if self.launch_error == error {
            return false;
        }
        if error.is_some() {
            // Connection failures remain visible in their tab; configuration
            // and host-key issues alone may request a settings/confirmation UI.
            // Recovery stays in the connection flow; editing settings is explicit.

            if error.as_deref().is_some_and(|error| error.starts_with("HostKeyUnknown:")) {
                if let Some(id) = profile_id
                    && self.selected != Some(Selected::Profile(id))
                    && !self.open_profile_editor(id)
                {
                    self.selected = None;
                    self.profile_draft = None;
                }
                self.page = 0;
                self.editor_tab = 3;
                if let Some(target) = self.catalog.as_ref().and_then(|catalog| {
                    catalog.ssh_connections.iter().find(|target| Some(target.profile_id) == profile_id)
                        .map(|target| (target.clone(), catalog.revision))
                }) {
                    self.host_key_prompt = Some(crate::host_key_prompt::HostKeyPrompt::new(target.0, target.1, true));
                    self.open = false;
                }
            }
        }
        self.launch_error = error;
        true
    }

    pub fn finish_open(&mut self) {
        self.open = false;
        self.launch_error = None;
        self.clear_secrets();
    }
    pub(crate) fn take_cancelled_connection(&mut self)->Option<ProfileId> {self.cancelled_connection.take()}

    pub fn sync(&mut self, view: &DesktopProfileView) -> bool {
        if self.seen_generation == view.generation {
            return false;
        }
        self.seen_generation = view.generation;
        self.credential_pending = view.credential_pending;
        self.package_panel.sync(view);
        let generation_changed = self
            .vault_panel
            .response
            .as_ref()
            .map(|response| response.lock_generation)
            != view.vault.as_ref().map(|response| response.lock_generation);
        let became_locked = self
            .vault_panel
            .response
            .as_ref()
            .is_some_and(|r| r.unlocked)
            && !view.vault.as_ref().is_some_and(|r| r.unlocked);
        self.vault_panel.response = view.vault.clone();
        self.vault_panel.error = view.vault_error.clone();
        self.vault_panel.set_operation_state(view.vault_mutations_pending > 0, view.vault_notice.clone());
        if became_locked || generation_changed {
            self.clear_secrets();
        }
        let previous_selection = self.selected;
        let catalog_changed = self.catalog.as_ref().map(|catalog| catalog.revision)
            != view.catalog.as_ref().map(|catalog| catalog.revision)
            || self.catalog.is_some() != view.catalog.is_some();
        if catalog_changed {
            self.selected = None;
            self.folder_draft = None;
            self.profile_draft = None;
            self.host_key_preview = None;
        }
        self.local_shells = view.local_shells.clone();
        self.catalog = view.catalog.clone();
        self.preview = view.preview.clone();
        self.preview_source = view.preview_source.clone();
        if self.import_digest != view.import_digest {
            self.import_omissions_reviewed = false;
        }
        self.import_digest = view.import_digest;
        self.import_notices = view.import_notices.clone();
        self.host_key_preview = view.host_key_preview.clone();
        self.error = view.error.clone();
        self.status = view.status.clone();
        if self.pending_new_ssh && self.catalog.is_some() {
            self.pending_new_ssh = false;
            self.new_ssh_profile();
        } else if self.pending_ssh_entry && self.catalog.is_some() {
            self.select_ssh_entry();
        } else if catalog_changed {
            match previous_selection {
                Some(Selected::Profile(id)) => {
                    if let Some(profile) = self.catalog.as_ref().and_then(|catalog| {
                        catalog
                            .profiles
                            .iter()
                            .find(|profile| profile.id == id)
                            .cloned()
                    }) {
                        self.select_profile(profile);
                    }
                }
                Some(Selected::Folder(id)) => {
                    if let Some(folder) = self.catalog.as_ref().and_then(|catalog| {
                        catalog
                            .folders
                            .iter()
                            .find(|folder| folder.id == id)
                            .cloned()
                    }) {
                        self.select_folder(folder);
                    }
                }
                None => {}
            }
        }
        if let Some(prompt) = &mut self.host_key_prompt { prompt.sync(view); }
        true
    }

    pub fn request_failed(&mut self) {
        let message = "请求尚未提交，后台可能仍在处理上一项操作。请稍后重试。";
        if let Some(prompt) = &mut self.host_key_prompt { prompt.fail(message); }
        self.credential_pending = false;
        self.vault_panel.request_failed();
        self.error = Some(message.into());
    }

    pub fn draw(&mut self, context: &egui::Context) -> Option<ProfileClientCommand> {
        if let Some(prompt) = &mut self.host_key_prompt {
            return match prompt.draw(context) {
                Some(crate::host_key_prompt::Action::Request(command)) => Some(command),
                Some(crate::host_key_prompt::Action::Connect(id)) => {
                    self.host_key_prompt = None;
                    Some(ProfileClientCommand::OpenProfile(id))
                }
                Some(crate::host_key_prompt::Action::Dismiss) => { self.cancelled_connection=prompt.cancelled_connection_profile();self.host_key_prompt = None; None }
                None => None,
            };
        }
        let saved = self
            .catalog
            .as_ref()
            .and_then(|catalog| match self.selected {
                Some(Selected::Profile(id))
                    if catalog
                        .ssh_connections
                        .iter()
                        .any(|target| target.profile_id == id) =>
                {
                    Some((id, catalog.revision))
                }
                _ => None,
            });
        if let Some(request) = self.vault_panel.draw(context, saved) {
            return Some(ProfileClientCommand::Vault(request));
        }
        if let Some(command) = self.package_panel.draw(
            context,
            self.catalog.as_ref(),
            self.vault_panel.response.as_ref(),
        ) {
            return Some(command);
        }
        if !self.open {
            return None;
        }
        let mut open = self.open;
        let mut command = None;
        let mut clear_selection = false;
        let catalog = self.catalog.clone();
        let host_key_preview = self.host_key_preview.clone();
        let local_shells = self.local_shells.clone();

        cshell_ui::form::dialog(context, "连接管理")
            .open(&mut open)
            .default_size([860.0, 620.0])
            .vscroll(false)
            .show(context, |ui| {
                ui.horizontal_wrapped(|ui| {
                    cshell_ui::form::tab(ui, &mut self.page, 0, "连接配置");
                    cshell_ui::form::tab(ui, &mut self.page, 1, "导入");
                    cshell_ui::form::tab(ui, &mut self.page, 2, "导出");
                    ui.separator();
                    ui.menu_button("凭据管理", |ui| {
                        cshell_ui::form::menu(ui);
                        if ui.button("Vault 与凭据…").clicked() {
                            self.vault_panel.open = true;
                            command = Some(ProfileClientCommand::Vault(
                                cshell_ipc::VaultRequest::new(cshell_ipc::VaultOperation::Status)
                                    .for_profile(saved.map_or_else(Vec::new, |(id, _)| {
                                        id.as_uuid().as_bytes().to_vec()
                                    })),
                            ));
                            ui.close();
                        }
                        if ui.button("加密凭据包…").clicked() {
                            self.package_panel.open = true;
                            ui.close();
                        }
                    });
                    if ui.button("刷新").clicked() {
                        command = Some(ProfileClientCommand::Refresh);
                    }
                });
                if let Some(error) = self.error.as_ref().or(self.launch_error.as_ref()) {
                    let message = if error.starts_with("HostKeyUnknown:") {
                        "尚未信任服务器主机密钥。请预览指纹并在弹框中确认。"
                    } else { error.as_str() };
                    ui.colored_label(cshell_ui::theme::DANGER, message);
                } else if self.status == "配置和密码已保存" || self.status.starts_with("密码已") || self.status == "服务器主机密钥已保存" {
                    ui.colored_label(cshell_ui::theme::SUCCESS, &self.status);
                }
                if self.credential_pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在保存或移除凭据，请稍候…");
                    });
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    ui.disable();
                }
                ui.separator();
                if self.page == 1 {
                    egui::ScrollArea::vertical()
                        .id_salt("profile-import-page")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if let Some(value) =
                                self.draw_import_body(ui, catalog.as_ref().map(|c| c.revision))
                            {
                                command = Some(value);
                            }
                        });
                    return;
                }
                if self.page == 2 {
                    egui::ScrollArea::vertical()
                        .id_salt("profile-export-page")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if let Some(value) = self.export_panel.draw_catalog(
                                ui,
                                catalog.as_ref().map(|c| c.revision),
                                catalog.as_ref(),
                            ) {
                                command = Some(value);
                            }
                        });
                    return;
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.button("新建 SSH").clicked() {
                        self.new_ssh_profile();
                    }
                    if ui.button("新建本地终端").clicked() {
                        self.new_ssh_profile();
                        if let Some(draft) = &mut self.profile_draft {
                            draft.record.kind = ProfileKind::Local;
                        }
                    }
                    if ui.button("新建文件夹").clicked() {
                        self.select_folder(ProfileFolder {
                            id: FolderId::new(),
                            name: String::new(),
                            parent_id: None,
                            terminal: TerminalOverrides::default(),
                        });
                    }
                });
                ui.separator();
                let Some(catalog) = &catalog else {
                    ui.label("正在加载连接配置…");
                    return;
                };
                // Reserve the footer before measuring either column.
                let body_height = (ui.available_height() - 104.0).max(140.0);
                ui.horizontal_top(|ui| {
                    let nav_width = (ui.available_width() * 0.25).clamp(140.0, 200.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(nav_width, body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_max_width(nav_width);
                            ui.add(
                                egui::TextEdit::singleline(&mut self.search)
                                    .hint_text("搜索名称或标签")
                                    .desired_width(nav_width),
                            );
                            egui::ScrollArea::vertical()
                                .id_salt("connection-list")
                                .max_height((body_height - 36.0).max(80.0))
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.label(egui::RichText::new("文件夹").small().strong().color(cshell_ui::theme::MUTED));
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
                                    ui.add_space(10.0);
                                    ui.label(egui::RichText::new("连接配置").small().strong().color(cshell_ui::theme::MUTED));
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
                        },
                    );
                    ui.add_sized([1.0, body_height], egui::Separator::default().vertical());
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            if let Some(draft) = &mut self.profile_draft {
                                ui.horizontal_wrapped(|ui| {
                                    for (tab, label) in [
                                        (0, "连接与认证"),
                                        (2, "代理 / 跳板"),
                                        (3, "主机密钥"),
                                        (4, "终端 / 分类"),
                                        (5, "连接选项"),
                                    ] {
                                        ui.add_enabled_ui(
                                            draft.record.kind == ProfileKind::Ssh
                                                || matches!(tab, 0 | 4),
                                            |ui| {
                                                cshell_ui::form::tab(
                                                    ui,
                                                    &mut self.editor_tab,
                                                    tab,
                                                    label,
                                                );
                                            },
                                        );
                                    }
                                });
                                if draft.record.kind == ProfileKind::Local
                                    && !matches!(self.editor_tab, 0 | 4)
                                {
                                    self.editor_tab = 0;
                                }
                                ui.separator();
                                egui::ScrollArea::vertical()
                                    .id_salt((
                                        "profile-editor-page",
                                        draft.record.id,
                                        self.editor_tab,
                                    ))
                                    .max_height((body_height - 76.0).max(80.0))
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        if let Some(value) = draw_profile_page(
                                            ui,
                                            draft,
                                            catalog,
                                            &local_shells,
                                            host_key_preview.as_ref(),
                                            self.editor_tab,
                                        ) {
                                            command = Some(value);
                                        }
                                    });
                            } else if let Some(draft) = &mut self.folder_draft {
                                egui::ScrollArea::vertical()
                                    .id_salt("folder-editor")
                                    .max_height(body_height)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.heading("文件夹");
                                        cshell_ui::form::text(ui, "名称", &mut draft.name);
                                        folder_picker(
                                            ui,
                                            "上级文件夹",
                                            &mut draft.parent_id,
                                            &catalog.folders,
                                            Some(draft.id),
                                        );
                                        terminal_fields(ui, &mut draft.terminal, &catalog.themes);
                                    });
                            } else {
                                ui.label("选择一个连接或文件夹，或点击“新建 SSH”。");
                            }
                        },
                    );
                });
                ui.separator();
                // Primary actions remain visible when a settings page scrolls.
                if let Some(draft) = &mut self.profile_draft {
                    let (value, removed) = draw_profile_actions(ui, draft, catalog);
                    if value.is_some() {
                        command = value;
                    }
                    clear_selection |= removed;
                } else if let Some(draft) = &mut self.folder_draft {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("保存文件夹").clicked() {
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
                            && ui.button("删除").clicked()
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
                }
            });
        self.open = open;
        if !open {
            self.clear_secrets();
        }
        if clear_selection {
            self.selected = None;
            self.profile_draft = None;
            self.folder_draft = None;
        }
        if command.as_ref().is_some_and(ProfileClientCommand::is_credential_mutation) {
            self.credential_pending = true;
        }
        if let Some(ProfileClientCommand::PreviewHostKey { profile_id, expected_revision }) = &command
            && let Some(target) = self.catalog.as_ref().and_then(|catalog| catalog.ssh_connections.iter().find(|target| target.profile_id == *profile_id))
        {
            let mut prompt = crate::host_key_prompt::HostKeyPrompt::new(target.clone(), *expected_revision, false);
            prompt.preview_started();
            self.host_key_prompt = Some(prompt);
        }
        command
    }

    fn draw_import_body(
        &mut self,
        ui: &mut egui::Ui,
        catalog_revision: Option<u64>,
    ) -> Option<ProfileClientCommand> {
        let mut command = None;
        let prior_format = self.import_format;
        cshell_ui::form::combo(ui, "Import format")
            .truncate()
            .selected_text(match self.import_format {
                ProfileImportFormat::Catalog => "CShell Profile JSON",
                ProfileImportFormat::OpenSsh => "OpenSSH config",
                ProfileImportFormat::PuttyRegistry => "PuTTY .reg (SSH metadata)",
                ProfileImportFormat::SecureCrtIni => "SecureCRT session .ini (SSH2 metadata)",
                ProfileImportFormat::XshellSession => "Xshell session .xsh (SSH metadata)",
                ProfileImportFormat::SecureCrtDirectory => "SecureCRT Sessions directory",
                ProfileImportFormat::XshellDirectory => "Xshell sessions directory",
                ProfileImportFormat::ForeignManifest => "Mixed migration manifest JSON",
                ProfileImportFormat::MobaXtermUnicode => "MobaXterm .mxtsessions (Unicode)",
                ProfileImportFormat::MobaXtermWindows1252 => {
                    "MobaXterm .mxtsessions (Windows-1252)"
                }
                ProfileImportFormat::Theme => "Theme JSON / TOML / XCS",
            })
            .show_ui(ui, |ui| {
                cshell_ui::form::menu(ui);
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::Catalog,
                    "CShell Profile JSON",
                );
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::OpenSsh,
                    "OpenSSH config",
                );
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::Theme,
                    "Theme JSON / TOML / XCS",
                );
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::PuttyRegistry,
                    "PuTTY .reg (SSH metadata)",
                );
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::SecureCrtIni,
                    "SecureCRT session .ini (SSH2 metadata)",
                );
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::XshellSession,
                    "Xshell session .xsh (SSH metadata)",
                );
                for (format, label) in [
                    (
                        ProfileImportFormat::SecureCrtDirectory,
                        "SecureCRT Sessions directory",
                    ),
                    (
                        ProfileImportFormat::XshellDirectory,
                        "Xshell sessions directory",
                    ),
                    (
                        ProfileImportFormat::ForeignManifest,
                        "Mixed migration manifest JSON",
                    ),
                ] {
                    ui.selectable_value(&mut self.import_format, format, label);
                }
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::MobaXtermUnicode,
                    "MobaXterm .mxtsessions (Unicode)",
                );
                ui.selectable_value(
                    &mut self.import_format,
                    ProfileImportFormat::MobaXtermWindows1252,
                    "MobaXterm .mxtsessions (Windows-1252)",
                );
            });
        if prior_format != self.import_format {
            self.preview = None;
        }
        if self.import_format == ProfileImportFormat::Theme {
            ui.label("Theme JSON / TOML / .xcs：最多 256 KiB、128 个主题。XCS 亮色映射至 ANSI 8～15，光标采用前景、选择采用蓝色；独立 text(bold) 不进入 v1 调色板。预览后再提交，内置名称不能替换。");
        }
        if matches!(
            self.import_format,
            ProfileImportFormat::OpenSsh
                | ProfileImportFormat::PuttyRegistry
                | ProfileImportFormat::SecureCrtIni
                | ProfileImportFormat::XshellSession
                | ProfileImportFormat::MobaXtermUnicode
                | ProfileImportFormat::MobaXtermWindows1252
                | ProfileImportFormat::SecureCrtDirectory
                | ProfileImportFormat::XshellDirectory
                | ProfileImportFormat::ForeignManifest
        ) {
            ui.horizontal_wrapped(|ui| {
                ui.label("Default account (when User is absent)");
                if ui.text_edit_singleline(&mut self.import_username).changed() {
                    self.preview = None;
                }
            });
            if self.import_format == ProfileImportFormat::OpenSsh {
                ui.label("Explicit Host aliases, wildcard defaults and linked named ProxyJump aliases are supported (up to eight jumps). Comma-separated jump lists, Include, Match, commands and unsupported options stop the import.");
                ui.label("Key paths are references only; ~/ uses this computer's home directory. Without IdentityFile, authentication uses the agent. Host keys and credentials are not imported.");
            } else if self.import_format.batch_input().is_some() {
                if self.import_format == ProfileImportFormat::ForeignManifest {
                    ui.label("Explicit migration manifest v1: only listed relative files below its directory are read. Each entry selects its format/encoding and optional logical folder prefix. No nested manifests or encoding guessing.");
                } else {
                    ui.label("Select a stable session backup directory. Relative directories become logical folders; only the selected format's files are converted. Default/informational files, unrelated ordinary files and valid non-SSH sessions are counted as skipped.");
                }
                ui.label("Limits: 4096 entries, 512 source files, 1 MiB per file, 8 MiB total input, 512 SSH sessions, 256 folders and 8 levels. Links/reparse points, ambiguous names, changed sources and invalid SSH settings stop the batch. Close the source app before copying its backup.");
                ui.label("Review the complete batch and each source's omissions before submitting. Passwords, keys and trust are omitted; bind CShell credentials afterward. SecureCRT XML, Xshell XTS/encrypted archives and complete vendor configurations are unsupported.");
            } else if self.import_format == ProfileImportFormat::PuttyRegistry {
                ui.label("Exported PuTTY Sessions subtree only, UTF-8 or UTF-16LE BOM. Converts SSH metadata and TERM; passwords, keys, host trust and other source settings are omitted. Review omissions and bind credentials afterward.");
                ui.label("Active proxies, remote commands, environment/port/X11/agent forwarding, no-PTY/no-shell and SSH-1 modes reject the batch. Non-SSH and Default Settings entries are skipped. No registry access or connection occurs.");
            } else if matches!(
                self.import_format,
                ProfileImportFormat::MobaXtermUnicode | ProfileImportFormat::MobaXtermWindows1252
            ) {
                ui.label("Exported Bookmarks sections only, up to 512 sessions and 256 logical folders. Select Unicode for UTF-8 / UTF-16LE BOM, or explicitly select Windows-1252 for legacy exports. No encoding guessing or full application INI import.");
                ui.label("Converts supported SSH name/host/port/account and folder paths; skips other session types. Missing/<default>/disabled accounts need your explicit default account. Passwords, keys, trust, TERM and other settings are omitted; rebind credentials before opening.");
                ui.label("Unsupported packed layouts, commands, gateways/proxies, SSH remote environments, macros, SSH-1, locale sending and active X11/agent forwarding reject the batch. Disable/clear these source settings before exporting. No vendor process, linked file or connection is used.");
            } else if self.import_format == ProfileImportFormat::XshellSession {
                ui.label("One plain Xshell .xsh session, UTF-8 or UTF-16LE BOM. Name comes from the filename; requires explicit SSH protocol, host and decimal port. Account comes from CONNECTION:AUTHENTICATION or your default account.");
                ui.label("Credentials, key references, trust, TERM/emulation and other settings are omitted. Nonempty proxy/jump/tunnel/script/command families and inheritance reject conversion. No archive, directory scan, vendor process or connection is used.");
            } else {
                ui.label("One SecureCRT session INI, UTF-8 or UTF-16LE BOM. Name comes from the file stem. Converts SSH2 host/port/account; source emulation/TERM, credentials, keys, trust and other settings are omitted.");
                ui.label("Only S/D scalars and bounded Z blocks are parsed. Nonempty script/command/route/forwarding families reject conversion, including dormant references. Default/Global configuration and XML exports are unsupported. No vendor process or connection runs.");
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(
                if matches!(
                    self.import_format,
                    ProfileImportFormat::SecureCrtDirectory | ProfileImportFormat::XshellDirectory
                ) {
                    "目录"
                } else {
                    "File"
                },
            );
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.import_path)
                        .desired_width(ui.available_width())
                        .hint_text("Path to the selected file or session backup directory"),
                )
                .changed()
            {
                self.preview = None;
            }
        });
        let prior_policy = self.import_policy;
        cshell_ui::form::combo(ui, "On conflict")
            .truncate()
            .selected_text(match self.import_policy {
                ProfileImportPolicy::Fail => "Stop",
                ProfileImportPolicy::Skip => "Skip",
                ProfileImportPolicy::Replace => "Replace",
            })
            .show_ui(ui, |ui| {
                cshell_ui::form::menu(ui);
                ui.selectable_value(&mut self.import_policy, ProfileImportPolicy::Fail, "Stop");
                ui.selectable_value(&mut self.import_policy, ProfileImportPolicy::Skip, "Skip");
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
            self.import_omissions_reviewed = false;
            self.preview = None;
            command = Some(ProfileClientCommand::PreviewImport(ProfileImportSource {
                path: self.import_path.clone(),
                policy: self.import_policy,
                format: self.import_format,
                default_username: self.import_username.clone(),
            }));
        }
        if let Some(preview) = self.preview.as_ref().filter(|_| {
            self.preview_source.as_ref().is_some_and(|source| {
                source.path == self.import_path
                    && source.policy == self.import_policy
                    && source.format == self.import_format
                    && source.default_username == self.import_username
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
                            Ok(ProfileImportItemKind::Theme) => "主题",
                            Err(_) => "Item",
                        };
                        ui.label(format!("{kind} {}: {action}", item.name));
                        if let Some(theme) = item
                            .theme
                            .clone()
                            .and_then(|data| cshell_domain::ThemeRecord::try_from(data).ok())
                        {
                            draw_palette(ui, &theme.palette);
                        }
                        if !item.details.is_empty() {
                            ui.label(&item.details);
                        }
                    }
                });
            let current = catalog_revision == Some(preview.base_revision);
            let foreign = self.import_format.reviewed_metadata();
            if foreign {
                for notice in &self.import_notices {
                    ui.colored_label(cshell_ui::theme::WARNING, notice);
                }
                ui.checkbox(&mut self.import_omissions_reviewed,"I reviewed converted targets and omitted settings; credentials and trust remain managed by CShell");
            }
            if ui
                .add_enabled(
                    preview.can_commit
                        && current
                        && (!foreign
                            || (self.import_omissions_reviewed && self.import_digest.is_some())),
                    egui::Button::new("Commit import"),
                )
                .clicked()
            {
                command = if foreign {
                    self.import_digest.map(|document_sha256| {
                        ProfileClientCommand::CommitReviewedImport {
                            expected_revision: preview.base_revision,
                            document_sha256,
                        }
                    })
                } else {
                    Some(ProfileClientCommand::CommitImport {
                        expected_revision: preview.base_revision,
                    })
                };
                self.import_omissions_reviewed = false;
            }
            if !current {
                ui.colored_label(cshell_ui::theme::WARNING, "Catalog changed; preview again.");
            }
        }
        command
    }

    fn select_profile(&mut self, record: ProfileRecord) {
        if self.selected != Some(Selected::Profile(record.id)) {
            self.editor_tab = 0;
        }
        self.page = 0;
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
            ssh_options: connection.map_or_else(cshell_domain::SshConnectionOptions::new_profile, |value| value.options.clone()),
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

fn draw_profile_page(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    catalog: &DesktopProfileCatalog,
    local_shells: &[cshell_ipc::LocalShellData],
    host_key_preview: Option<&HostKeyPreviewData>,
    tab: u8,
) -> Option<ProfileClientCommand> {
    let mut command = None;
    match tab {
        0 => {
            ui.heading("连接信息");
            cshell_ui::form::text(ui, "名称", &mut draft.record.name);
            ui.horizontal_wrapped(|ui| {
                ui.label("类型");
                ui.selectable_value(&mut draft.record.kind, ProfileKind::Ssh, "SSH");
                ui.selectable_value(&mut draft.record.kind, ProfileKind::Local, "本地");
            });

            ui.separator();
            if draft.record.kind == ProfileKind::Ssh {
                cshell_ui::form::text(ui, "主机", &mut draft.ssh_host);
                ui.horizontal_wrapped(|ui| {
                    ui.label("端口");
                    ui.add(egui::DragValue::new(&mut draft.ssh_port).range(1..=65535));
                });
                ui.separator();
                command=authentication::draw(ui,draft,catalog);
            } else {
                if draw_local_fields(ui, &mut draft.local, local_shells) {
                    command = Some(ProfileClientCommand::DiscoverLocalShells);
                }
                ui.collapsing("Open once with overrides", |ui| {
ui.label("Uses the saved Profile. These overrides are not saved; New Shell uses the saved configuration.");
cshell_ui::form::text(ui, "Directory (empty uses Profile)", &mut draft.local.launch_cwd);
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
Err(error) => { ui.colored_label(cshell_ui::theme::WARNING, error); }
}
}
});
            }
        }
        1 => {command=authentication::draw(ui,draft,catalog);}
        2 => {
            ui.heading("代理与跳板");
            cshell_ui::form::combo(ui, "连接方式")
                .truncate()
                .selected_text(match draft.route_kind {
                    1 => "SOCKS5 代理",
                    2 => "HTTP CONNECT 代理",
                    3 => "跳板连接",
                    _ => "直接连接",
                })
                .show_ui(ui, |ui| {
                    cshell_ui::form::menu(ui);
                    ui.selectable_value(&mut draft.route_kind, 0, "直接连接");
                    ui.selectable_value(&mut draft.route_kind, 1, "SOCKS5 代理");
                    ui.selectable_value(&mut draft.route_kind, 2, "HTTP CONNECT 代理");
                    ui.selectable_value(&mut draft.route_kind, 3, "跳板连接");
                });
            if matches!(draft.route_kind, 1 | 2) {
                ui.label("使用无认证代理；仍会严格核对目标 SSH 服务器的主机密钥。");
                cshell_ui::form::text(ui, "代理主机", &mut draft.proxy_host);
                ui.horizontal_wrapped(|ui| {
                    ui.label("端口");
                    ui.add(egui::DragValue::new(&mut draft.proxy_port).range(1..=65535));
                });
            } else if draft.route_kind == 3 {
                let selected = draft
                    .jump_profile_id
                    .and_then(|id| catalog.profiles.iter().find(|item| item.id == id))
                    .map_or("选择跳板配置", |item| item.name.as_str());
                cshell_ui::form::combo(ui, "跳板配置")
                    .truncate()
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        cshell_ui::form::menu(ui);
                        for candidate in &catalog.ssh_connections {
                            if candidate.profile_id == draft.record.id {
                                continue;
                            }
                            if let Some(profile) = catalog
                                .profiles
                                .iter()
                                .find(|item| item.id == candidate.profile_id)
                            {
                                ui.selectable_value(
                                    &mut draft.jump_profile_id,
                                    Some(profile.id),
                                    &profile.name,
                                );
                            }
                        }
                    });
                ui.label("Each jump uses its own saved authentication and verified host key. Up to eight jumps; cycles are refused. Confirm host keys from the outermost jump toward the target. Agent forwarding is not enabled.");
                if let Some(candidate) = draft
                    .jump_profile_id
                    .and_then(|id| catalog.ssh_connections.iter().find(|c| c.profile_id == id))
                {
                    match cshell_domain::ssh_jump_chain(candidate, &catalog.ssh_connections) {
                        Ok(mut chain)
                            if chain.len() < cshell_domain::MAX_SSH_JUMP_HOPS
                                && !chain.iter().any(|c| c.profile_id == draft.record.id) =>
                        {
                            chain.push(candidate);
                            ui.label(format!(
                                "Connection order: {} → target",
                                chain
                                    .iter()
                                    .map(|c| format!("{}@{}:{}", c.username, c.host, c.port))
                                    .collect::<Vec<_>>()
                                    .join(" → ")
                            ));
                        }
                        _ => {
                            ui.colored_label(
                                cshell_ui::theme::DANGER,
                                "This jump would create a cycle or exceed the eight-hop limit.",
                            );
                        }
                    }
                }
            }
        }
        5 => {
            ui.heading("连接选项");
            ui.checkbox(&mut draft.ssh_options.keepalive_enabled, "保持连接活跃（SSH 保活）");
            ui.add_enabled_ui(draft.ssh_options.keepalive_enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("保活间隔（秒）");
                    ui.add(egui::DragValue::new(&mut draft.ssh_options.keepalive_interval_secs).range(1..=3600));
                    ui.label("允许连续无响应次数");
                    ui.add(egui::DragValue::new(&mut draft.ssh_options.keepalive_max).range(1..=10));
                });
            });
            ui.weak("空闲时发送 SSH 协议保活请求，不向终端输入字符。");
            ui.separator();
            ui.checkbox(&mut draft.ssh_options.auto_reconnect, "断线自动重连");
            ui.add_enabled_ui(draft.ssh_options.auto_reconnect, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("重试间隔（秒）");
                    ui.add(egui::DragValue::new(&mut draft.ssh_options.reconnect_delay_secs).range(1..=300));
                    ui.label("最大重试次数");
                    ui.add(egui::DragValue::new(&mut draft.ssh_options.reconnect_attempts).range(1..=20));
                });
            });
            ui.weak("仅意外断线时重连。手动断开、关闭标签或退出远端 shell 不会重连。重连会打开新的远端 shell，不重放之前的输入。");
            ui.weak("保存后对新连接生效。");
            ui.separator();
            ui.label("SFTP 校验策略");
            crate::zmodem_settings::verification_override_selector(ui,&mut draft.ssh_options.sftp_verification);
            ui.weak(format!("当前生效：{}",crate::zmodem_settings::verification_label(draft.ssh_options.sftp_verification.unwrap_or_else(||crate::zmodem_settings::load().sftp_verification))));
            ui.separator();
            ui.label("ZMODEM / sz 下载目录");
            let mut directory=draft.ssh_options.zmodem_download_directory.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut directory).desired_width(ui.available_width()).hint_text("继承全局默认（留空）")).changed() {
                draft.ssh_options.zmodem_download_directory=(!directory.trim().is_empty()).then_some(directory);
            }
            ui.weak(format!("生效目录：{}",crate::zmodem_settings::resolve(&crate::zmodem_settings::load(),draft.ssh_options.zmodem_download_directory.as_deref()).display()));
            ui.weak("留空时实时继承 工具 → 全局默认设置；填写绝对路径则仅用于此会话。");
        }
        3 => {
            ui.heading("服务器主机密钥");
            ui.label("默认阻止未知或已变化的服务器主机密钥。请先保存常规配置，再预览并核对指纹。");
            let saved_key_target = catalog.ssh_connections.iter().any(|item| {
                item.profile_id == draft.record.id
                    && item.host == draft.ssh_host.trim()
                    && item.port == draft.ssh_port
                    && item.route == draft.route()
            });
            if ui
                .add_enabled(saved_key_target, egui::Button::new("预览服务器主机密钥"))
                .clicked()
            {
                command = Some(ProfileClientCommand::PreviewHostKey {
                    profile_id: draft.record.id,
                    expected_revision: catalog.revision,
                });
            }
            if let Some(preview) = host_key_preview.filter(|preview| {
                preview.profile_id == draft.record.id.as_uuid().as_bytes()
                    && preview.host == draft.ssh_host.trim()
                    && preview.port == u32::from(draft.ssh_port)
                    && saved_key_target
            }) {
                ui.label(format!("密钥算法：{}", preview.algorithm));
                ui.label(format!("SHA256 指纹：{}", preview.fingerprint));
                ui.label(format!("公钥：{}", preview.public_key_line));
                ui.label("重新预览可打开主机密钥确认弹框。");
            }
        }
        _ => {
            ui.heading("分类与终端设置");
            folder_picker(
                ui,
                "文件夹",
                &mut draft.record.folder_id,
                &catalog.folders,
                None,
            );
            ui.checkbox(&mut draft.record.favorite, "收藏");
            cshell_ui::form::text(ui, "标签", &mut draft.tags);
            cshell_ui::environment::selector(ui,&mut draft.tags);
            terminal_fields(ui, &mut draft.record.terminal, &catalog.themes);
            terminal_settings::draw(ui,catalog,&draft.record);
        }
    }
    command
}

fn draw_profile_actions(
    ui: &mut egui::Ui,
    draft: &mut ProfileDraft,
    catalog: &DesktopProfileCatalog,
) -> (Option<ProfileClientCommand>, bool) {
    let mut command = None;
    let mut clear_selection = false;
    let local_target = draft.local.record(draft.record.id);
    if draft.record.kind == ProfileKind::Local
        && let Err(error) = &local_target
    {
        ui.colored_label(cshell_ui::theme::WARNING, error);
    }
    let target_incomplete = draft.record.kind == ProfileKind::Ssh
        && ((draft.ssh_host.trim().is_empty() && !draft.ssh_username.trim().is_empty())
            || (!draft.password.is_empty() && (draft.ssh_host.trim().is_empty() || draft.ssh_username.trim().is_empty())));
    if target_incomplete {
        ui.colored_label(cshell_ui::theme::WARNING, "请填写主机。若要在配置中保存密码，请同时填写用户名；也可留空账号密码，在连接时填写。");
    }
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(
                !target_incomplete
                    && (draft.record.kind != ProfileKind::Local || local_target.is_ok()),
                egui::Button::new(egui::RichText::new("保存配置").color(cshell_ui::theme::ACCENT))
                    .fill(cshell_ui::theme::ACCENT_FILL),
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
            if draft.record.kind == ProfileKind::Ssh && !draft.ssh_host.trim().is_empty() {
                let target = SshConnectionRecord {
                    options: draft.ssh_options.clone(),
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
                    change: Some(profile_change::Change::UpsertSshConnection(
                        SshConnectionData::from(&target),
                    )),
                });
            } else if draft.had_ssh_connection {
                changes.push(ProfileChange {
                    change: Some(profile_change::Change::RemoveSshConnection(
                        draft.record.id.as_uuid().as_bytes().to_vec(),
                    )),
                });
            }
            if draft.record.kind == ProfileKind::Local {
                if let Ok(target) = &local_target {
                    changes.push(ProfileChange {
                        change: Some(profile_change::Change::UpsertLocalConnection(
                            cshell_ipc::LocalConnectionData::from(target),
                        )),
                    });
                }
            } else if draft.had_local_connection {
                changes.push(ProfileChange {
                    change: Some(profile_change::Change::RemoveLocalConnection(
                        draft.record.id.as_uuid().as_bytes().to_vec(),
                    )),
                });
            }
            command = Some(if draft.record.kind == ProfileKind::Ssh
                && !draft.password.is_empty()
            {
                ProfileClientCommand::ApplyWithPassword {
                    expected_revision: catalog.revision,
                    changes,
                    profile_id: draft.record.id,
                    password: Zeroizing::new(std::mem::take(&mut draft.password).into_bytes()),
                }
            } else {
                ProfileClientCommand::Apply {
                    expected_revision: catalog.revision,
                    changes,
                }
            });
        }
        let saved_target = catalog
            .ssh_connections
            .iter()
            .any(|c| c.profile_id == draft.record.id)
            || catalog
                .local_connections
                .iter()
                .any(|c| c.profile_id == draft.record.id);
        if ui
            .add_enabled(saved_target, egui::Button::new("连接已保存配置"))
            .clicked()
        {
            command = Some(ProfileClientCommand::OpenProfile(draft.record.id));
        }
        if catalog
            .profiles
            .iter()
            .any(|item| item.id == draft.record.id)
            && ui.button("删除").clicked()
        {
            command = Some(ProfileClientCommand::Apply {
                expected_revision: catalog.revision,
                changes: vec![ProfileChange {
                    change: Some(profile_change::Change::RemoveProfile(
                        draft.record.id.as_uuid().as_bytes().to_vec(),
                    )),
                }],
            });
            clear_selection = true;
        }
    });

    (command, clear_selection)
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
        .unwrap_or("根目录");
    cshell_ui::form::combo(ui, label)
        .truncate()
        .selected_text(name)
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(selected, None, "根目录");
            for folder in folders {
                if Some(folder.id) != exclude {
                    ui.selectable_value(selected, Some(folder.id), &folder.name);
                }
            }
        });
}

fn terminal_fields(
    ui: &mut egui::Ui,
    terminal: &mut TerminalOverrides,
    themes: &[cshell_domain::ThemeRecord],
) {
    ui.weak("留空或选择继承时，使用上级分组设置；分组未设置则使用全局默认。");
    if ui.add_enabled(*terminal!=TerminalOverrides::default(),egui::Button::new("全部恢复继承")).clicked(){*terminal=TerminalOverrides::default();}
    let mut terminal_type = terminal.terminal_type.clone().unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        ui.label("终端类型");
        if ui.text_edit_singleline(&mut terminal_type).changed() {
            terminal.terminal_type = optional_text(&terminal_type);
        }
    });
    cshell_ui::form::combo(ui, "主题")
        .truncate()
        .selected_text(match terminal.theme.as_deref() {
            None => "继承上级 / 全局".to_owned(),
            Some("default" | "dark") => "Dark".to_owned(),
            Some("light") => "Light".to_owned(),
            Some(other) if themes.iter().any(|theme| theme.name == other) => other.to_owned(),
            Some(other) => format!("未知主题：{other}（使用深色）"),
        })
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(&mut terminal.theme, None, "继承");
            ui.selectable_value(&mut terminal.theme, Some("dark".into()), "Dark");
            ui.selectable_value(&mut terminal.theme, Some("light".into()), "Light");
            for theme in themes {
                ui.selectable_value(&mut terminal.theme, Some(theme.name.clone()), &theme.name);
            }
        });
    cshell_ui::form::combo(ui, "日志")
        .truncate()
        .selected_text(match terminal.logging {
            None => "继承上级 / 全局",
            Some(true) => "开启",
            Some(false) => "关闭",
        })
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(&mut terminal.logging, None, "继承");
            ui.selectable_value(&mut terminal.logging, Some(true), "开启");
            ui.selectable_value(&mut terminal.logging, Some(false), "关闭");
        });
}

fn draw_palette(ui: &mut egui::Ui, palette: &cshell_domain::TerminalPalette) {
    fn swatch(ui: &mut egui::Ui, [r, g, b]: [u8; 3], text: &str) {
        let color = egui::Color32::from_rgb(r, g, b);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2.0, color);
        response.on_hover_text(format!("{text}: #{r:02x}{g:02x}{b:02x}"));
    }
    ui.horizontal_wrapped(|ui| {
        for (label, color) in [
            ("Foreground", palette.foreground),
            ("Background", palette.background),
            ("Cursor", palette.cursor),
            ("Selection", palette.selection),
        ] {
            ui.label(label);
            swatch(ui, color, label);
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (index, color) in palette.ansi.iter().copied().enumerate() {
            swatch(ui, color, &format!("ANSI {index}"));
        }
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
    ui.heading("本地程序");
    cshell_ui::form::combo(ui, "关闭视图时")
        .truncate()
        .selected_text(match draft.close_policy {
            cshell_domain::LocalClosePolicy::KeepAlive => "保持会话运行",
            cshell_domain::LocalClosePolicy::TerminateOnViewClose => "终止进程",
        })
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(
                &mut draft.close_policy,
                cshell_domain::LocalClosePolicy::KeepAlive,
                "保持会话运行",
            );
            ui.selectable_value(
                &mut draft.close_policy,
                cshell_domain::LocalClosePolicy::TerminateOnViewClose,
                "终止进程",
            );
        });
    ui.label("Applies when explicitly closing a view. Disconnects and application exit keep the session running.");
    let mut refresh = false;
    ui.horizontal_wrapped(|ui| {
        cshell_ui::form::combo(ui, "已安装的终端")
            .truncate()
            .selected_text("选择终端")
            .show_ui(ui, |ui| {
                cshell_ui::form::menu(ui);
                for shell in shells {
                    if ui.selectable_label(false, &shell.name).clicked() {
                        draft.program.clone_from(&shell.program);
                        draft.args.clone_from(&shell.args);
                    }
                }
            });
        refresh = ui.button("刷新列表").clicked();
    });
    cshell_ui::form::text(ui, "程序路径", &mut draft.program);
    ui.label("Arguments are passed separately, exactly as entered. No command-line splitting.");
    ui.collapsing("启动参数", |ui| {
        let mut remove = None;
        for (index, arg) in draft.args.iter_mut().enumerate() {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{}", index + 1));
                ui.text_edit_singleline(arg);
                if ui.button("移除").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            draft.args.remove(index);
        }
        if ui
            .add_enabled(draft.args.len() < 256, egui::Button::new("添加参数"))
            .clicked()
        {
            draft.args.push(String::new());
        }
    });
    cshell_ui::form::combo(ui, "工作目录")
        .truncate()
        .selected_text(match draft.cwd_kind {
            1 => "主目录",
            2 => "指定目录",
            _ => "继承 daemon 目录",
        })
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(&mut draft.cwd_kind, 0, "继承 daemon 目录");
            ui.selectable_value(&mut draft.cwd_kind, 1, "主目录");
            ui.selectable_value(&mut draft.cwd_kind, 2, "指定目录");
        });
    if draft.cwd_kind == 2 {
        cshell_ui::form::text(ui, "目录", &mut draft.cwd_path);
    }
    ui.label("The directory applies to the local process. For WSL, set its Linux directory with WSL arguments.");
    ui.collapsing("环境变量", |ui| {
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
        ui.horizontal_wrapped(|ui| {
            ui.text_edit_singleline(key);
            ui.label("=");
            ui.text_edit_singleline(value);
            if ui.button("移除").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        env.remove(index);
    }
    if ui
        .add_enabled(env.len() < 128, egui::Button::new("添加环境变量"))
        .clicked()
    {
        env.push((String::new(), String::new()));
    }
}

mod authentication;
#[cfg(test)]
mod layout_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn main_page_credentials_entry_blocks_terminal_without_opening_profile_editor() {
        let mut panel = ProfilePanel::default();
        let command = panel.open_credentials();
        assert!(!panel.open);
        assert!(panel.is_dialog_open());
        assert!(matches!(command, ProfileClientCommand::Vault(ref request) if request.operation == cshell_ipc::VaultOperation::Status as i32 && request.profile_id.is_empty()));
    }
    #[test]
    fn openssh_import_buttons_capture_source_and_reject_changed_settings() {
        use crate::profile_connection::{ProfileImportFormat, ProfileImportSource};
        fn draw(
            panel: &mut ProfilePanel,
            context: &egui::Context,
            events: Vec<egui::Event>,
            revision: u64,
        ) -> (egui::accesskit::TreeUpdate, Option<ProfileClientCommand>) {
            let mut command = None;
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 1000.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| command = panel.draw_import_body(ui, Some(revision)),
            );
            output.textures_delta.clear();
            (
                output
                    .platform_output
                    .accesskit_update
                    .unwrap_or_else(|| panic!("missing accessibility update")),
                command,
            )
        }
        fn click(update: &egui::accesskit::TreeUpdate, label: &str) -> Vec<egui::Event> {
            let bounds = update
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == egui::accesskit::Role::Button && node.label() == Some(label)
                })
                .unwrap_or_else(|| panic!("missing import button: {label}"))
                .1
                .bounds()
                .unwrap_or_else(|| panic!("missing import button bounds"));
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
        let context = egui::Context::default();
        context.enable_accesskit();
        let source = ProfileImportSource {
            path: "config".into(),
            policy: cshell_ipc::ProfileImportPolicy::Skip,
            format: ProfileImportFormat::OpenSsh,
            default_username: "operator".into(),
        };
        let mut panel = ProfilePanel {
            import_path: source.path.clone(),
            import_policy: source.policy,
            import_format: source.format,
            import_username: source.default_username.clone(),
            ..Default::default()
        };
        draw(&mut panel, &context, vec![], 8);
        let (update, _) = draw(&mut panel, &context, vec![], 8);
        let (_, command) = draw(&mut panel, &context, click(&update, "Preview import"), 8);
        let Some(ProfileClientCommand::PreviewImport(captured)) = command else {
            panic!("preview command");
        };
        assert_eq!(captured, source);
        panel.preview_source = Some(source);
        panel.preview = Some(cshell_ipc::ProfileImportPreviewData {
            base_revision: 8,
            can_commit: true,
            change_count: 2,
            items: Vec::new(),
        });
        let (update, _) = draw(&mut panel, &context, vec![], 8);
        assert!(
            draw(&mut panel, &context, click(&update, "Commit import"), 9)
                .1
                .is_none()
        );
        let (update, _) = draw(&mut panel, &context, vec![], 8);
        assert!(matches!(
            draw(&mut panel, &context, click(&update, "Commit import"), 8).1,
            Some(ProfileClientCommand::CommitImport {
                expected_revision: 8
            })
        ));
        panel.import_username = "changed".into();
        let (update, _) = draw(&mut panel, &context, vec![], 8);
        assert!(
            !update
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Commit import"))
        );
        panel.import_username = "operator".into();
        panel.import_format = ProfileImportFormat::Theme;
        let (update, _) = draw(&mut panel, &context, vec![], 8);
        let (_, command) = draw(&mut panel, &context, click(&update, "Preview import"), 8);
        assert!(
            matches!(command, Some(ProfileClientCommand::PreviewImport(source)) if source.format == ProfileImportFormat::Theme)
        );
        panel.import_format = ProfileImportFormat::Catalog;
        let (update, _) = draw(&mut panel, &context, vec![], 8);
        assert!(
            !update
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Commit import"))
        );
    }

    #[test]
    fn panel_debug_hides_user_text_and_error_payloads() {
        let marker = "SECRET-PANEL-MARKER";
        let panel = super::ProfilePanel {
            error: Some(marker.into()),
            launch_error: Some(marker.into()),
            status: marker.into(),
            search: marker.into(),
            import_path: marker.into(),
            ..Default::default()
        };
        assert!(!format!("{panel:?}").contains(marker));
        assert!(format!("{panel:?}").contains("has_error: true"));
    }
    use super::{ProfilePanel, Selected};
    use crate::profile_connection::{
        DesktopProfileCatalog, DesktopProfileView, ProfileClientCommand,
    };
    use cshell_domain::{
        ProfileKind, SshAgentBackend, SshAuthMethod, SshConnectionRecord, SshRoute,
    };

    #[test]
    fn new_ssh_entry_survives_catalog_loading() {
        let mut panel = ProfilePanel::default();
        panel.open_new_ssh_profile();
        assert!(panel.open);
        assert!(panel.pending_new_ssh);
        assert!(panel.profile_draft.is_none());

        let view = DesktopProfileView {
            generation: 1,
            catalog: Some(DesktopProfileCatalog {
                themes: Vec::new(),
                revision: 1,
                defaults: Default::default(),
                folders: vec![],
                profiles: vec![],
                ssh_connections: vec![],
                local_connections: vec![],
            }),
            ..DesktopProfileView::default()
        };
        assert!(panel.sync(&view));
        assert!(!panel.pending_new_ssh);
        assert_eq!(
            panel.profile_draft.as_ref().map(|draft| draft.record.kind),
            Some(ProfileKind::Ssh)
        );
    }

    #[test]
    fn ssh_entry_opens_a_new_draft_then_keeps_the_saved_profile_selected() {
        let mut panel = ProfilePanel::default();
        panel.open_ssh_connections();
        assert!(panel.open);
        assert!(panel.pending_ssh_entry);

        let mut view = DesktopProfileView {
            generation: 1,
            catalog: Some(DesktopProfileCatalog {
                themes: Vec::new(),
                revision: 1,
                defaults: Default::default(),
                folders: vec![],
                profiles: vec![],
                ssh_connections: vec![],
                local_connections: vec![],
            }),
            ..DesktopProfileView::default()
        };
        assert!(panel.sync(&view));
        let draft = panel
            .profile_draft
            .as_ref()
            .unwrap_or_else(|| panic!("SSH entry must create a draft"));
        assert_eq!(draft.record.kind, ProfileKind::Ssh);
        let profile = draft.record.clone();
        assert_eq!(panel.selected, Some(Selected::Profile(profile.id)));

        let catalog = view.catalog.as_mut().unwrap_or_else(|| panic!("catalog"));
        catalog.revision = 2;
        catalog.profiles.push(profile.clone());
        catalog.ssh_connections.push(SshConnectionRecord {
            options: Default::default(),
            profile_id: profile.id,
            host: "example.test".into(),
            port: 22,
            username: "alice".into(),
            auth_method: SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: SshAgentBackend::Auto,
            agent_identity: None,
            route: SshRoute::Direct,
        });
        view.generation = 2;
        assert!(panel.sync(&view));
        assert_eq!(panel.selected, Some(Selected::Profile(profile.id)));
        assert_eq!(
            panel
                .profile_draft
                .as_ref()
                .map(|draft| draft.ssh_host.as_str()),
            Some("example.test")
        );
        panel.open_ssh_connections();
        assert_eq!(panel.selected, Some(Selected::Profile(profile.id)));

        let context = egui::Context::default();
        context.enable_accesskit();
        let mut draw = |events| {
            let mut command = None;
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1_200.0, 760.0),
                    )),
                    events,
                    ..egui::RawInput::default()
                },
                |ui| command = panel.draw(ui.ctx()),
            );
            output.textures_delta.clear();
            let update = output
                .platform_output
                .accesskit_update
                .unwrap_or_else(|| panic!("Profile window should be accessible"));
            (command, update)
        };
        let _first = draw(vec![]);
        let (_, update) = draw(vec![]);
        let button = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("连接已保存配置")
            })
            .map(|(_, node)| node)
            .unwrap_or_else(|| panic!("saved SSH connect button should be visible"));
        let bounds = button
            .bounds()
            .unwrap_or_else(|| panic!("saved SSH connect button should have bounds"));
        let position = egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        let (command, _) = draw(vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        assert!(matches!(command, Some(ProfileClientCommand::OpenProfile(id)) if id == profile.id));
    }
}
