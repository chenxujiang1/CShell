use crate::profile_connection::{DesktopProfileCatalog, ProfileClientCommand};
use cshell_domain::ProfileId;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

#[derive(Default)]
pub struct ProfileExportPanel {
    path: String,
    include_local_context: bool,
    content: ExportContent,
    selected: BTreeSet<ProfileId>,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum ExportContent {
    #[default]
    Profiles,
    Themes,
    OpenSsh,
}
impl ExportContent {
    fn label(self) -> &'static str {
        match self {
            Self::Profiles => "Profiles",
            Self::Themes => "Theme palettes",
            Self::OpenSsh => "OpenSSH config",
        }
    }
}

impl ProfileExportPanel {
    pub fn draw_catalog(
        &mut self,
        ui: &mut egui::Ui,
        revision: Option<u64>,
        catalog: Option<&DesktopProfileCatalog>,
    ) -> Option<ProfileClientCommand> {
        cshell_ui::form::combo(ui, "Export content")
            .truncate()
            .selected_text(self.content.label())
            .show_ui(ui, |ui| {
                cshell_ui::form::menu(ui);
                for content in [
                    ExportContent::Profiles,
                    ExportContent::Themes,
                    ExportContent::OpenSsh,
                ] {
                    ui.selectable_value(&mut self.content, content, content.label());
                }
            });
        if self.content == ExportContent::Themes {
            ui.label("Export saved custom terminal palettes. Builtin Dark and Light remain available on the recipient.");
        } else if self.content == ExportContent::OpenSsh {
            ui.label("Select saved SSH targets (up to 512 including automatically included jump targets).");
            ui.label("Use the new standalone file with ssh -F <file> <alias>. Stable aliases use Profile UUIDs; names appear in comments.");
            ui.label("Direct and single-hop routes only. Pageant, Windows Auto and pinned-agent identities cannot be converted; select OpenSSH agent explicitly. Local Profiles, Vault credentials and trust are excluded; referenced key files must exist locally.");
            if let Some(catalog) = catalog {
                self.selected.retain(|id| {
                    catalog
                        .ssh_connections
                        .iter()
                        .any(|target| target.profile_id == *id)
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Select all SSH targets").clicked() {
                        self.selected = catalog
                            .ssh_connections
                            .iter()
                            .map(|target| target.profile_id)
                            .collect();
                    }
                    if ui.button("Clear SSH selection").clicked() {
                        self.selected.clear();
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt("openssh-export-selection")
                    .max_height(200.0)
                    .show(ui, |ui| {
                        for profile in &catalog.profiles {
                            let Some(target) = catalog
                                .ssh_connections
                                .iter()
                                .find(|target| target.profile_id == profile.id)
                            else {
                                continue;
                            };
                            let mut selected = self.selected.contains(&profile.id);
                            if ui.checkbox(&mut selected, &profile.name).changed() {
                                if selected {
                                    self.selected.insert(profile.id);
                                } else {
                                    self.selected.remove(&profile.id);
                                }
                            }
                            ui.small(format!(
                                "{}@{}:{} — {}",
                                target.username,
                                target.host,
                                target.port,
                                cshell_application::openssh_export_alias(profile.id)
                            ));
                        }
                    });
                ui.label(format!(
                    "{} targets selected; dependencies are checked during export",
                    self.selected.len()
                ));
            }
        } else {
            ui.label(
                "Export the saved Profile catalog, SSH key references and local programs as JSON.",
            );
            ui.label(
                "Vault credentials and key contents are excluded. Theme names are references.",
            );
            ui.checkbox(&mut self.include_local_context, "Include local arguments and environment values (may contain secrets; stored as plaintext)");
        }
        ui.label("Absolute destination path; existing files will not be overwritten.");
        ui.text_edit_singleline(&mut self.path);
        let ready = revision.is_some()
            && Path::new(self.path.trim()).is_absolute()
            && (self.content != ExportContent::OpenSsh
                || (catalog.is_some()
                    && !self.selected.is_empty()
                    && self.selected.len() <= cshell_application::MAX_OPENSSH_EXPORT_HOSTS));
        if ui
            .add_enabled(
                ready,
                egui::Button::new(match self.content {
                    ExportContent::Themes => "Export Themes",
                    ExportContent::Profiles => "Export Profiles",
                    ExportContent::OpenSsh => "Export OpenSSH",
                }),
            )
            .clicked()
        {
            if self.content == ExportContent::Themes {
                return Some(ProfileClientCommand::ExportThemes {
                    path: self.path.trim().to_owned(),
                    expected_revision: revision?,
                });
            }
            if self.content == ExportContent::OpenSsh {
                return Some(ProfileClientCommand::ExportOpenSsh {
                    path: self.path.trim().into(),
                    expected_revision: revision?,
                    selected: self.selected.iter().copied().collect(),
                });
            }
            return Some(ProfileClientCommand::ExportCatalog {
                path: self.path.trim().to_owned(),
                expected_revision: revision?,
                include_local_context: self.include_local_context,
            });
        }
        None
    }
}

/// Publish once without overwriting a destination, including one created during export.
/// The caller has already validated the bounded transfer document.
pub fn save_export(path: &str, bytes: &[u8]) -> Result<(), String> {
    let target = Path::new(path);
    if !target.is_absolute()
        || target.file_name().is_none()
        || bytes.len() > cshell_application::MAX_PROFILE_IMPORT_BYTES
    {
        return Err("Export requires an absolute file path and at most 1 MiB".into());
    }
    let parent = target
        .parent()
        .ok_or("Export destination directory is unavailable")?;
    // An owned directory also cleans up any temporary hard link left by persist_noclobber.
    let staging = tempfile::Builder::new()
        .prefix(".cshell-export-")
        .tempdir_in(parent)
        .map_err(|_| "Export destination directory is unavailable")?;
    let mut file = tempfile::NamedTempFile::new_in(staging.path())
        .map_err(|_| "Export staging file could not be created")?;
    file.write_all(bytes)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|_| "Export staging file could not be written")?;
    let published = file.persist_noclobber(target).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            "Export destination already exists; choose a new file"
        } else {
            "Export result is uncertain; check the destination before retrying"
        }
        .to_owned()
    })?;
    drop(published);
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(
            |_| "Export file was saved, but directory sync failed; check it before retrying",
        )?;
    staging.close().map_err(
        |_| "Export file was saved, but temporary cleanup failed; check it before retrying",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ProfileExportPanel, save_export};
    use crate::profile_connection::ProfileClientCommand;
    use std::error::Error;

    fn draw(
        panel: &mut ProfileExportPanel,
        context: &egui::Context,
        events: Vec<egui::Event>,
        revision: Option<u64>,
    ) -> (egui::accesskit::TreeUpdate, Option<ProfileClientCommand>) {
        draw_catalog(panel, context, events, revision, None)
    }

    fn draw_catalog(
        panel: &mut ProfileExportPanel,
        context: &egui::Context,
        events: Vec<egui::Event>,
        revision: Option<u64>,
        catalog: Option<&crate::profile_connection::DesktopProfileCatalog>,
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
            |ui| command = panel.draw_catalog(ui, revision, catalog),
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
    fn click(tree: &egui::accesskit::TreeUpdate, label: &str) -> Vec<egui::Event> {
        let bounds = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .unwrap_or_else(|| panic!("missing control {label}"))
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
    fn export_button_captures_revision_and_requires_explicit_context_opt_in()
    -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp
            .path()
            .join("profiles.json")
            .to_str()
            .ok_or("path")?
            .to_owned();
        let mut panel = ProfileExportPanel {
            path: path.clone(),
            ..Default::default()
        };
        let context = egui::Context::default();
        context.enable_accesskit();
        draw(&mut panel, &context, vec![], Some(7));
        let (tree, _) = draw(&mut panel, &context, vec![], Some(7));
        let (_, command) = draw(
            &mut panel,
            &context,
            click(&tree, "Export Profiles"),
            Some(7),
        );
        let Some(ProfileClientCommand::ExportCatalog {
            path: captured,
            expected_revision,
            include_local_context,
        }) = command
        else {
            panic!("missing export command");
        };
        assert_eq!(captured, path);
        assert_eq!(expected_revision, 7);
        assert!(!include_local_context);
        let (tree, _) = draw(&mut panel, &context, vec![], Some(7));
        draw(
            &mut panel,
            &context,
            click(
                &tree,
                "Include local arguments and environment values (may contain secrets; stored as plaintext)",
            ),
            Some(7),
        );
        let (tree, _) = draw(&mut panel, &context, vec![], Some(8));
        let (_, command) = draw(
            &mut panel,
            &context,
            click(&tree, "Export Profiles"),
            Some(8),
        );
        assert!(matches!(
            command,
            Some(ProfileClientCommand::ExportCatalog {
                expected_revision: 8,
                include_local_context: true,
                ..
            })
        ));
        let (tree, _) = draw(&mut panel, &context, vec![], None);
        assert!(
            draw(&mut panel, &context, click(&tree, "Export Profiles"), None)
                .1
                .is_none()
        );
        panel.path = "relative.json".into();
        let (tree, _) = draw(&mut panel, &context, vec![], Some(8));
        assert!(
            draw(
                &mut panel,
                &context,
                click(&tree, "Export Profiles"),
                Some(8)
            )
            .1
            .is_none()
        );
        panel.path = path.clone();
        panel.content = super::ExportContent::Themes;
        let (tree, _) = draw(&mut panel, &context, vec![], Some(9));
        let (_, command) = draw(&mut panel, &context, click(&tree, "Export Themes"), Some(9));
        assert!(
            matches!(command, Some(ProfileClientCommand::ExportThemes { path: captured, expected_revision: 9 }) if captured == path)
        );
        Ok(())
    }

    #[test]
    fn openssh_export_pointer_selection_captures_saved_targets_and_rejects_stale_removed_targets()
    -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let document = cshell_application::import_openssh_config(
            b"Host jump\nUser ops\nHost app\nUser worker\nProxyJump jump\n",
            Default::default(),
        )?;
        let app = document
            .profiles
            .iter()
            .find(|p| p.name == "app")
            .ok_or("app")?
            .id;
        let mut catalog = crate::profile_connection::DesktopProfileCatalog {
            revision: 7,
            defaults: Default::default(),
            folders: vec![],
            profiles: document.profiles,
            ssh_connections: document.ssh_connections,
            local_connections: vec![],
            themes: vec![],
        };
        let path = temp
            .path()
            .join("ssh-config")
            .to_string_lossy()
            .into_owned();
        let mut panel = ProfileExportPanel {
            path: path.clone(),
            content: super::ExportContent::OpenSsh,
            include_local_context: true,
            ..Default::default()
        };
        let context = egui::Context::default();
        context.enable_accesskit();
        draw_catalog(&mut panel, &context, vec![], Some(7), Some(&catalog));
        let (tree, _) = draw_catalog(&mut panel, &context, vec![], Some(7), Some(&catalog));
        assert!(
            draw_catalog(
                &mut panel,
                &context,
                click(&tree, "Export OpenSSH"),
                Some(7),
                Some(&catalog)
            )
            .1
            .is_none()
        );
        draw_catalog(
            &mut panel,
            &context,
            click(&tree, "app"),
            Some(7),
            Some(&catalog),
        );
        let (tree, _) = draw_catalog(&mut panel, &context, vec![], Some(7), Some(&catalog));
        let (_, command) = draw_catalog(
            &mut panel,
            &context,
            click(&tree, "Export OpenSSH"),
            Some(7),
            Some(&catalog),
        );
        assert!(
            matches!(command, Some(ProfileClientCommand::ExportOpenSsh { path: captured, expected_revision: 7, selected }) if captured == path && selected == vec![app])
        );
        let (tree, _) = draw_catalog(&mut panel, &context, vec![], None, Some(&catalog));
        assert!(
            draw_catalog(
                &mut panel,
                &context,
                click(&tree, "Export OpenSSH"),
                None,
                Some(&catalog)
            )
            .1
            .is_none()
        );
        let (tree, _) = draw_catalog(&mut panel, &context, vec![], Some(8), Some(&catalog));
        draw_catalog(
            &mut panel,
            &context,
            click(&tree, "Select all SSH targets"),
            Some(8),
            Some(&catalog),
        );
        assert_eq!(panel.selected.len(), 2);
        let (tree, _) = draw_catalog(&mut panel, &context, vec![], Some(8), Some(&catalog));
        draw_catalog(
            &mut panel,
            &context,
            click(&tree, "Clear SSH selection"),
            Some(8),
            Some(&catalog),
        );
        assert!(panel.selected.is_empty());
        panel.selected.insert(app);
        catalog
            .ssh_connections
            .retain(|target| target.profile_id != app);
        let (tree, _) = draw_catalog(&mut panel, &context, vec![], Some(9), Some(&catalog));
        assert!(panel.selected.is_empty());
        assert!(
            draw_catalog(
                &mut panel,
                &context,
                click(&tree, "Export OpenSSH"),
                Some(9),
                Some(&catalog)
            )
            .1
            .is_none()
        );
        assert!(std::fs::read_dir(temp.path())?.next().is_none());
        Ok(())
    }

    #[test]
    fn export_preserves_existing_file_and_cleans_staging() -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("profiles.json");
        let path_text = path.to_str().ok_or("non UTF-8 test path")?;
        save_export(path_text, b"first")?;
        assert!(save_export(path_text, b"second").is_err());
        assert_eq!(std::fs::read(&path)?, b"first");
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 1);
        assert!(save_export("relative.json", b"test").is_err());
        assert!(
            save_export(
                temp.path().join("large.json").to_str().ok_or("path")?,
                &vec![0; cshell_application::MAX_PROFILE_IMPORT_BYTES + 1]
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn competing_exports_publish_one_complete_file() -> Result<(), Box<dyn Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("profiles.json");
        let path_text = path.to_str().ok_or("non UTF-8 test path")?.to_owned();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (*b"ab")
            .into_iter()
            .map(|byte| {
                let path = path_text.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    save_export(&path, &vec![byte; 8192])
                })
            })
            .collect();
        let mut successes = 0;
        for worker in workers {
            successes += usize::from(worker.join().map_err(|_| "worker panicked")?.is_ok());
        }
        assert_eq!(successes, 1);
        let bytes = std::fs::read(path)?;
        assert!(bytes == vec![b'a'; 8192] || bytes == vec![b'b'; 8192]);
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 1);
        Ok(())
    }
}
