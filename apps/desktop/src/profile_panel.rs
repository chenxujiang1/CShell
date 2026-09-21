use crate::profile_connection::{DesktopProfileCatalog, DesktopProfileView, ProfileClientCommand};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, TerminalOverrides,
};
use cshell_ipc::{
    ProfileChange, ProfileFolderData, ProfileImportAction, ProfileImportItemKind,
    ProfileImportPolicy, ProfileImportPreviewData, ProfileRecordData, profile_change,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Selected {
    Folder(FolderId),
    Profile(ProfileId),
}

#[derive(Debug)]
struct ProfileDraft {
    record: ProfileRecord,
    tags: String,
}

#[derive(Debug)]
pub struct ProfilePanel {
    pub open: bool,
    seen_generation: u64,
    catalog: Option<DesktopProfileCatalog>,
    preview: Option<ProfileImportPreviewData>,
    preview_source: Option<(String, ProfileImportPolicy)>,
    error: Option<String>,
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
            error: None,
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
        }
        self.catalog = view.catalog.clone();
        self.preview = view.preview.clone();
        self.preview_source = view.preview_source.clone();
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
        egui::Window::new("Profiles and folders")
            .open(&mut open)
            .default_size([760.0, 580.0])
            .resizable(true)
            .show(context, |ui| {
                ui.label(&self.status);
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
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
                                ui.horizontal(|ui| {
                                    if ui.button("Save Profile").clicked() {
                                        draft.record.tags = draft
                                            .tags
                                            .split(',')
                                            .map(str::trim)
                                            .filter(|tag| !tag.is_empty())
                                            .map(str::to_owned)
                                            .collect();
                                        command = Some(ProfileClientCommand::Apply {
                                            expected_revision: catalog.revision,
                                            changes: vec![ProfileChange {
                                                change: Some(
                                                    profile_change::Change::UpsertProfile(
                                                        ProfileRecordData::from(&draft.record),
                                                    ),
                                                ),
                                            }],
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
        self.selected = Some(Selected::Profile(record.id));
        self.profile_draft = Some(ProfileDraft {
            tags: record.tags.iter().cloned().collect::<Vec<_>>().join(", "),
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
