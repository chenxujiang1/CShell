use super::*;
use crate::sftp_settings::Favorite;

#[derive(Default)]
pub(super) struct History {
    paths: Vec<String>,
    current: usize,
    pending: Option<usize>,
}
impl History {
    // Only a successfully loaded directory changes the history. Refresh and pagination do not.
    pub(super) fn loaded(&mut self, path: &str) {
        if let Some(index) = self.pending.take()
            && self.paths.get(index).is_some_and(|target| target == path)
        {
            self.current = index;
            return;
        }
        if self
            .paths
            .get(self.current)
            .is_some_and(|current| current == path)
        {
            return;
        }
        self.paths.truncate(if self.paths.is_empty() {
            0
        } else {
            self.current + 1
        });
        self.paths.push(path.into());
        if self.paths.len() > 64 {
            self.paths.remove(0);
        }
        self.current = self.paths.len() - 1;
    }
    fn target(&self, forward: bool) -> Option<usize> {
        if forward {
            (self.current + 1 < self.paths.len()).then_some(self.current + 1)
        } else {
            self.current.checked_sub(1)
        }
    }
}

pub(super) struct Navigation {
    pub(super) history: [History; 2],
    pub(super) active: usize,
    pub(super) editing: [bool; 2],
    pub(super) focus: [bool; 2],
    favorites: Vec<Favorite>,
}
impl Default for Navigation {
    fn default() -> Self {
        Self {
            history: Default::default(),
            active: 1,
            editing: [false; 2],
            focus: [false; 2],
            favorites: crate::sftp_settings::favorites(),
        }
    }
}

// Build targets from filesystem ancestors rather than splitting on a separator: Windows
// drive and UNC roots stay intact, and remote paths always use POSIX semantics.
fn breadcrumbs(local: bool, path: &str) -> Vec<(String, String)> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut targets = vec![path.to_owned()];
    for _ in 0..128 {
        let Some(current) = targets.last() else {
            break;
        };
        let parent = parent_path(local, current);
        if parent == *current {
            break;
        }
        targets.push(parent);
    }
    targets.reverse();
    targets
        .into_iter()
        .enumerate()
        .map(|(index, target)| {
            let label = if index == 0 {
                target.clone()
            } else if local {
                std::path::Path::new(&target)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.clone())
            } else {
                target
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or(&target)
                    .to_owned()
            };
            (label, target)
        })
        .collect()
}

fn favorite_for(view: &SftpView, local: bool, path: &str) -> Option<Favorite> {
    if path.is_empty() {
        return None;
    }
    Some(Favorite {
        peer: if local {
            None
        } else {
            Some(view.peer.clone()?)
        },
        path: path.into(),
    })
}

impl SftpPanel {
    pub(in crate::sftp_panel) fn navigate(
        &mut self,
        session: SessionId,
        local: bool,
        path: String,
    ) -> SftpClientCommand {
        self.browser.navigation.active = usize::from(!local);
        self.browser.navigation.history[usize::from(!local)].pending = None;
        self.browse(session, local, path)
    }
    fn history_command(
        &mut self,
        session: SessionId,
        local: bool,
        forward: bool,
    ) -> Option<SftpClientCommand> {
        let history = &mut self.browser.navigation.history[usize::from(!local)];
        let index = history.target(forward)?;
        let path = history.paths[index].clone();
        history.pending = Some(index);
        self.browser.navigation.active = usize::from(!local);
        Some(self.browse(session, local, path))
    }
    fn edit_path(&mut self, local: bool, view: &SftpView) {
        let side = usize::from(!local);
        self.browser.navigation.active = side;
        self.browser.navigation.editing[side] = true;
        self.browser.navigation.focus[side] = true;
        if let Some(path) = loaded_path(view, local) {
            if local {
                self.local_directory = path.into();
            } else {
                self.remote_directory = path.into();
            }
        }
    }
    pub(super) fn navigation_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let context = ui.ctx();
        if egui::Popup::is_any_open(context) {
            return;
        }
        if ui.input(|input| input.pointer.any_pressed())
            && let Some(pos) = context.pointer_latest_pos()
            && context.layer_id_at(pos) == Some(ui.layer_id())
        {
            for (side, rect) in [self.browser.local_rect, self.browser.remote_rect]
                .into_iter()
                .enumerate()
            {
                if rect.is_some_and(|rect| rect.contains(pos)) {
                    self.browser.navigation.active = side;
                }
            }
        }
        if context.text_edit_focused() {return;}
        let local = self.browser.navigation.active == 0;
        let pressed = |modifiers, key| context.input_mut(|input| input.consume_key(modifiers, key));
        if pressed(egui::Modifiers::CTRL, egui::Key::L)
            || pressed(egui::Modifiers::ALT, egui::Key::D)
        {
            self.edit_path(local, view);
        } else if pressed(egui::Modifiers::ALT, egui::Key::ArrowLeft) {
            *command = self.history_command(session, local, false);
        } else if pressed(egui::Modifiers::ALT, egui::Key::ArrowRight) {
            *command = self.history_command(session, local, true);
        } else if pressed(egui::Modifiers::ALT, egui::Key::ArrowUp) {
            if let Some(path) = loaded_path(view, local) {
                *command = Some(self.navigate(session, local, parent_path(local, path)));
            }
        } else if pressed(egui::Modifiers::NONE, egui::Key::F5) {
            if let Some(path) = loaded_path(view, local) {
                *command = Some(self.navigate(session, local, path.into()));
            }
        } else if pressed(egui::Modifiers::CTRL, egui::Key::D) {
            if let Some(favorite) =
                favorite_for(view, local, loaded_path(view, local).unwrap_or(""))
            {
                self.toggle_favorite(favorite);
            }
        }
    }
    fn toggle_favorite(&mut self, favorite: Favorite) {
        match crate::sftp_settings::toggle_favorite(favorite) {
            Ok(favorites) => {
                self.browser.navigation.favorites = favorites;
                self.browser.error = None;
            }
            Err(error) => self.browser.error = Some(error),
        }
    }
    pub(super) fn navigation_bar(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        local: bool,
        command: &mut Option<SftpClientCommand>,
    ) {
        let side = usize::from(!local);
        let path = loaded_path(view, local).unwrap_or("");
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for (forward, label, hint) in [
                (false, "←", "后退 · Alt+Left"),
                (true, "→", "前进 · Alt+Right"),
            ] {
                if visuals::tool(ui,if forward {Icon::Forward}else{Icon::Back},label,
                        self.browser.navigation.history[side].target(forward).is_some(),None,true)
                    .on_hover_text(hint)
                    .clicked()
                {
                    *command = self.history_command(session, local, forward);
                }
            }
            let current = favorite_for(view, local, path);
            let starred = current
                .as_ref()
                .is_some_and(|item| self.browser.navigation.favorites.contains(item));
            egui::containers::menu::MenuButton::new(if starred { "★" } else { "☆" })
                .config(
                    egui::containers::menu::MenuConfig::new()
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                )
                .ui(ui, |ui| {
                    ui.set_max_width(330.0);
                    ui.strong(if local {
                        "本地收藏目录"
                    } else {
                        "当前服务器收藏目录"
                    });
                    if ui
                        .add_enabled(
                            current.is_some(),
                            egui::Button::new(if starred {
                                "取消收藏当前目录 · Ctrl+D"
                            } else {
                                "收藏当前目录 · Ctrl+D"
                            }),
                        )
                        .clicked()
                        && let Some(current) = current.clone()
                    {
                        self.toggle_favorite(current);
                    }
                    if !local && view.peer.is_none() {
                        ui.weak("正在读取服务器身份，稍后可收藏");
                    }
                    ui.separator();
                    let favorites: Vec<_> = self
                        .browser
                        .navigation
                        .favorites
                        .iter()
                        .filter(|item| {
                            if local {
                                item.peer.is_none()
                            } else {
                                view.peer.is_some() && item.peer == view.peer
                            }
                        })
                        .cloned()
                        .collect();
                    if favorites.is_empty() {
                        ui.weak("尚未收藏目录");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for favorite in favorites {
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_sized(
                                            [280.0, 24.0],
                                            egui::Button::new(&favorite.path).truncate(),
                                        )
                                        .on_hover_text(&favorite.path)
                                        .clicked()
                                    {
                                        *command = Some(self.navigate(
                                            session,
                                            local,
                                            favorite.path.clone(),
                                        ));
                                        ui.close();
                                    }
                                    if ui.small_button("×").on_hover_text("移除收藏").clicked()
                                    {
                                        self.toggle_favorite(favorite);
                                    }
                                });
                            }
                        });
                })
                .0
                .on_hover_text("收藏目录 · Ctrl+D 收藏 / 取消收藏");
            let path_width = (ui.available_width() - 90.0).max(20.0);
            if self.browser.navigation.editing[side] {
                let text = if local {
                    &mut self.local_directory
                } else {
                    &mut self.remote_directory
                };
                let mut output = egui::TextEdit::singleline(text)
                    .desired_width(path_width)
                    .char_limit(cshell_ipc::MAX_SFTP_PATH_BYTES)
                    .show(ui);
                if self.browser.navigation.focus[side] {
                    output.response.request_focus();
                    output
                        .state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(text.chars().count()),
                        )));
                    output.state.store(ui.ctx(), output.response.id);
                    self.browser.navigation.focus[side] = false;
                }
                let enter = (output.response.has_focus() || output.response.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Enter));
                let escape = (output.response.has_focus() || output.response.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Escape));
                let go = ui.small_button("转到").clicked() || enter;
                if go {
                    let path = text.clone();
                    if !path.is_empty() {
                        *command = Some(self.navigate(session, local, path));
                    }
                    self.browser.navigation.editing[side] = false;
                    output.response.surrender_focus();
                } else if escape {
                    if local {
                        self.local_directory = path.into();
                    } else {
                        self.remote_directory = path.into();
                    }
                    self.browser.navigation.editing[side] = false;
                    output.response.surrender_focus();
                }
            } else {
                ui.allocate_ui_with_layout(
                    egui::vec2(path_width, 24.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
                        egui::ScrollArea::horizontal()
                            .id_salt("breadcrumbs")
                            .min_scrolled_height(0.0)
                            .max_height(24.0)
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .auto_shrink([false, true])
                            .stick_to_right(true)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    for (index, (label, target)) in
                                        breadcrumbs(local, path).into_iter().enumerate()
                                    {
                                        if index > 0 {
                                            ui.weak("›");
                                        }
                                        let crumb = ui
                                            .add(
                                                egui::Button::new(label)
                                                    .small()
                                                    .wrap_mode(egui::TextWrapMode::Extend),
                                            )
                                            .on_hover_text(&target);
                                        columns::clip_accessibility(ui, &crumb);
                                        if crumb.clicked() {
                                            *command = Some(self.navigate(session, local, target));
                                        }
                                    }
                                    if path.is_empty() {
                                        ui.weak("尚未载入目录");
                                    }
                                });
                            });
                    },
                );
                if visuals::tool(ui,Icon::Edit,"编辑",true,None,true)
                    .on_hover_text("编辑路径 · Ctrl+L / Alt+D；Enter 转到，Esc 取消")
                    .clicked()
                {
                    self.edit_path(local, view);
                }
            }
            let choices = directory_choices(
                local,
                path,
                if local {
                    &self.browser.local_history
                } else {
                    &self.browser.remote_history
                },
                if local {
                    &view.local_entries
                } else {
                    &view.entries
                },
            );
            egui::ComboBox::from_id_salt("directory-history")
                .width(20.0)
                .selected_text("")
                .show_ui(ui, |ui| {
                    ui.set_max_width(330.0);
                    for choice in choices {
                        if ui
                            .add(egui::Button::new(&choice).truncate())
                            .on_hover_text(&choice)
                            .clicked()
                        {
                            *command = Some(self.navigate(session, local, choice));
                        }
                    }
                })
                .response
                .on_hover_text("选择目录：上级、最近访问和子目录");
        });
    }
}

#[cfg(test)]
mod tests;
