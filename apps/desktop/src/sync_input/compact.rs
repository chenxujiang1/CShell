//! Everyday command entry. All submissions still pass through the prepared-input risk gates.
use super::*;

// Geometry keeps menu indicators independent of symbol font fallback.
fn dropdown(ui: &mut egui::Ui, label: impl Into<String>, width: f32) -> egui::Response {
    let response = ui.add_sized([width, 28.0], egui::Button::new(label.into()).truncate());
    let p = egui::pos2(response.rect.right() - 10.0, response.rect.center().y);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            p + egui::vec2(-3.0, -1.5),
            p + egui::vec2(3.0, -1.5),
            p + egui::vec2(0.0, 2.0),
        ],
        ui.visuals().text_color(),
        egui::Stroke::NONE,
    ));
    response
}

impl DesktopSyncInput {
    fn scope_label(&self) -> String {
        if !self.compose.armed {
            return "当前会话".into();
        }
        if !self.is_armed() {
            return "目标已暂停".into();
        }
        let count = self
            .controller
            .armed_snapshot()
            .map_or(0, |s| s.targets().len());
        let name = match self.selection {
            Some(SyncInputSelection::CurrentWindow(_)) => "当前窗口",
            Some(SyncInputSelection::CurrentTabGroup(_)) => "标签组",
            _ => "自定义",
        };
        format!("{name} · {count} 台")
    }

    fn preset_targets(
        &self,
        window: WorkspaceWindowId,
        selection: SyncInputSelection,
    ) -> BTreeSet<TabId> {
        self.controller
            .port()
            .targets
            .iter()
            .filter_map(|(tab, entry)| {
                (entry.opened.window_id == window
                    && entry.opened.kind == SyncInputTargetKind::Ssh
                    && self
                        .controller
                        .port()
                        .opened_target(*tab)
                        .is_some_and(|t| t.eligibility == SyncInputEligibility::Ready)
                    && match selection {
                        SyncInputSelection::CurrentTabGroup(group) => {
                            entry.opened.group_id == group
                        }
                        _ => true,
                    })
                .then_some(*tab)
            })
            .collect()
    }

    fn scope_popup(&mut self, ui: &mut egui::Ui, window: WorkspaceWindowId, active: Option<TabId>) {
        let response = dropdown(ui, self.scope_label(), 140.0);
        let popup_id = response.id.with("scope");
        if response.clicked() {
            self.target_filter.clear();
            if let Some(snapshot) = self.controller.armed_snapshot() {
                self.selected = snapshot.targets().iter().map(|t| t.tab_id).collect();
            } else {
                self.selected.retain(|tab| {
                    self.controller
                        .port()
                        .targets
                        .get(tab)
                        .is_some_and(|t| t.opened.window_id == window)
                });
            }
        }
        let mut apply = None;
        let mut current = false;
        let mut reuse = false;
        egui::Popup::menu(&response)
            .id(popup_id)
            .align(egui::RectAlign::TOP_START)
            .width(340.0_f32.min(ui.ctx().content_rect().width() - 24.0))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.strong("发送范围");
                if let Some(tab) = active
                    && !self.targeted(tab)
                    && self
                        .controller
                        .port()
                        .opened_target(tab)
                        .is_some_and(|t| t.eligibility == SyncInputEligibility::Ready)
                    && ui.button("加入当前会话").clicked()
                {
                    let mut tabs = self
                        .controller
                        .armed_snapshot()
                        .map(|s| {
                            s.targets()
                                .iter()
                                .map(|t| t.tab_id)
                                .collect::<BTreeSet<_>>()
                        })
                        .unwrap_or_default();
                    tabs.insert(tab);
                    apply = Some((SyncInputSelection::Manual, tabs));
                    ui.close();
                }
                if ui
                    .selectable_label(!self.compose.armed, "当前会话")
                    .clicked()
                {
                    current = true;
                    ui.close();
                }
                if self.is_armed() && !self.compose.armed && ui.button("使用已选择的目标").clicked()
                {
                    reuse = true;
                    ui.close();
                }
                for (label, selection) in [
                    (
                        "当前标签组 SSH",
                        active
                            .and_then(|tab| self.controller.port().targets.get(&tab))
                            .map(|t| SyncInputSelection::CurrentTabGroup(t.opened.group_id)),
                    ),
                    (
                        "当前窗口 SSH",
                        Some(SyncInputSelection::CurrentWindow(window)),
                    ),
                ] {
                    if let Some(selection) = selection {
                        let targets = self.preset_targets(window, selection);
                        if ui
                            .add_enabled(
                                !targets.is_empty(),
                                egui::Button::new(format!("{label} · {} 台", targets.len())),
                            )
                            .clicked()
                        {
                            apply = Some((selection, targets));
                            ui.close();
                        }
                    }
                }
                ui.separator();
                ui.label("自定义目标");
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.target_filter)
                        .hint_text("搜索主机…")
                        .desired_width(f32::INFINITY),
                );
                if response.clicked() {
                    search.request_focus();
                }
                ui.horizontal(|ui| {
                    if ui.small_button("全选 SSH").clicked() {
                        self.selected =
                            self.preset_targets(window, SyncInputSelection::CurrentWindow(window));
                    }
                    if ui.small_button("清空选择").clicked() {
                        self.selected.clear();
                    }
                });
                let filter = self.target_filter.trim().to_lowercase();
                egui::ScrollArea::vertical()
                    .id_salt("inline-command-targets")
                    .max_height((ui.ctx().content_rect().height() * 0.35).clamp(60.0, 220.0))
                    .show(ui, |ui| {
                        for (tab, entry) in &self.controller.port().targets {
                            if entry.opened.window_id != window
                                || !entry.opened.title.to_lowercase().contains(&filter)
                            {
                                continue;
                            }
                            let ready = self
                                .controller
                                .port()
                                .opened_target(*tab)
                                .is_some_and(|t| t.eligibility == SyncInputEligibility::Ready);
                            let mut selected = self.selected.contains(tab);
                            let label = format!(
                                "{} · {}{}",
                                entry.opened.title,
                                if entry.opened.kind == SyncInputTargetKind::Ssh {
                                    "SSH"
                                } else {
                                    "本地"
                                },
                                if ready { "" } else { " · 暂不可用" }
                            );
                            if ui
                                .add_enabled(
                                    ready || selected,
                                    egui::Checkbox::new(&mut selected, label),
                                )
                                .changed()
                            {
                                if selected {
                                    self.selected.insert(*tab);
                                } else {
                                    self.selected.remove(tab);
                                }
                            }
                        }
                    });
                ui.weak("目标固定；新连接不会自动加入。本地与 SSH 分别选择。");
                if ui
                    .add_enabled(
                        !self.selected.is_empty(),
                        egui::Button::new(format!("应用 · {} 台", self.selected.len())),
                    )
                    .clicked()
                {
                    apply = Some((SyncInputSelection::Manual, self.selected.clone()));
                    ui.close();
                }
            });
        if current {
            self.compose.armed = false;
            self.compose.focus_requested = true;
        }
        if reuse {
            self.compose.armed = true;
            self.compose.focus_requested = true;
        }
        if let Some((selection, tabs)) = apply {
            self.selection = Some(selection);
            self.selected = tabs;
            self.picker_command_only = true;
            self.choose_targets(window, active);
            self.compose.focus_requested = true;
        }
    }

    fn history_popup(&mut self, ui: &mut egui::Ui) {
        let response = dropdown(ui, "历史", 68.0)
            .on_hover_text("↑/↓ 调取最近命令；仅保留本次运行，最多 100 条 / 256 KiB");
        let mut load = None;
        if response.clicked() {
            self.compose.history_filter.clear();
        }
        egui::Popup::menu(&response)
            .align(egui::RectAlign::TOP_END)
            .width(380.0_f32.min(ui.ctx().content_rect().width() - 24.0))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                let search = ui.add(
                    egui::TextEdit::singleline(&mut *self.compose.history_filter)
                        .hint_text("搜索历史命令…")
                        .desired_width(f32::INFINITY),
                );
                if response.clicked() {
                    search.request_focus();
                }
                let filter = self.compose.history_filter.to_lowercase();
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for body in self
                            .history
                            .entries()
                            .rev()
                            .filter(|body| body.to_lowercase().contains(&filter))
                        {
                            let display: String = body
                                .chars()
                                .take(100)
                                .map(|c| if c.is_control() { ' ' } else { c })
                                .collect();
                            if ui
                                .add(egui::Button::new(display).truncate())
                                .on_hover_text("填入编辑框，核对后发送")
                                .clicked()
                            {
                                load = Some(zeroize::Zeroizing::new(body.to_owned()));
                                ui.close();
                            }
                        }
                    });
                ui.separator();
                ui.weak("历史可能包含敏感信息，仅存内存。");
                if ui.small_button("清空命令历史").clicked() {
                    self.history.clear();
                }
            });
        if let Some(body) = load {
            self.compose.multiline = body.contains(['\n', '\r']);
            self.compose.text = body;
            self.compose.history_recalled = true;
            self.history.reset_cursor();
            self.compose.focus_requested = true;
        }
    }

    fn command_editor(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        ime_event: bool,
        blocked: bool,
    ) -> bool {
        let id = egui::Id::new(("compact-command-editor", self.compose.owner));
        let popup = egui::Popup::is_any_open(ui.ctx());
        let eligible = !blocked && !popup && !ime_event && !self.compose.ime_active;
        if eligible && !self.compose.multiline && ui.memory(|m| m.has_focus(id)) {
            let older = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp));
            let newer =
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown));
            if (older || newer)
                && let Some(body) = self.history.navigate(older, &self.compose.text)
            {
                self.compose.text = body;
                self.compose.history_recalled = true;
            }
        }
        let enter = eligible
            && ui.input(|i| {
                i.key_pressed(egui::Key::Enter)
                    && !i.modifiers.alt
                    && !i.modifiers.shift
                    && (!self.compose.multiline || i.modifiers.ctrl || i.modifiers.command)
            });
        if self.compose.history_recalled {
            let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(self.compose.text.chars().count()),
                )));
            state.store(ui.ctx(), id);
            self.compose.history_recalled = false;
        }
        let editor = if self.compose.multiline {
            let edit = egui::TextEdit::multiline(&mut *self.compose.text).desired_rows(4);
            if enter { edit.return_key(None) } else { edit }
        } else {
            egui::TextEdit::singleline(&mut *self.compose.text)
        };
        let response = ui
            .add_enabled_ui(!popup && !blocked, |ui| {
                ui.add_sized(
                    [width, if self.compose.multiline { 90.0 } else { 28.0 }],
                    editor
                        .id(id)
                        .font(egui::TextStyle::Monospace)
                        .char_limit(MAX_QUICK_COMMAND_BODY_BYTES)
                        .hint_text(if self.compose.multiline {
                            "多行命令 · Ctrl+Enter 发送"
                        } else {
                            "输入命令 · Enter 发送 · ↑↓ 历史"
                        }),
                )
            })
            .inner;
        if response.changed() {
            self.history.reset_cursor();
        }
        if self.compose.focus_requested && !blocked && !popup {
            response.request_focus();
            self.compose.focus_requested = false;
        }
        if (ime_event || self.compose.ime_active) && response.lost_focus() {
            response.request_focus();
        }
        self.compose.focus_active = response.has_focus() || popup || self.compose.ime_active;
        enter && (response.has_focus() || response.lost_focus())
    }

    pub(super) fn draw_compact_command_bar(
        &mut self,
        root: &mut egui::Ui,
        window: WorkspaceWindowId,
        active: Option<TabId>,
        ime_event: bool,
    ) {
        let blocked = self.modal_blocked || self.panel_open || self.arm_review.is_some();
        let popup_was_open = egui::Popup::is_any_open(root.ctx());
        let mut send = false;
        let mut preview = false;
        let mut close = false;
        let multiline = self.compose.multiline;
        egui::Panel::bottom("terminal-compose-dock")
            .exact_size(if multiline { 146.0 } else { 42.0 })
            .frame(cshell_ui::form::utility_bar())
            .show(root, |ui| {
                if blocked {
                    ui.disable();
                }
                ui.spacing_mut().item_spacing.x = 5.0;
                ui.horizontal(|ui| {
                    self.scope_popup(ui, window, active);
                    if !multiline {
                        let width = (ui.available_width() - 335.0).max(60.0);
                        send |=
                            self.command_editor(ui, width, ime_event, blocked || popup_was_open);
                    } else {
                        ui.label("多行编辑");
                    }
                    let brief = self.status.split('；').next().unwrap_or("");
                    ui.add_sized(
                        [70.0, 24.0],
                        egui::Label::new(egui::RichText::new(brief).small()).truncate(),
                    )
                    .on_hover_text(format!("{}\n入队不表示远端执行成功。", self.status));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui
                            .small_button("×")
                            .on_hover_text("收起命令栏并清除草稿 · Esc")
                            .clicked();
                        let more = dropdown(ui, "更多", 68.0);
                        egui::Popup::menu(&more)
                            .align(egui::RectAlign::TOP_END)
                            .show(|ui| {
                                if ui
                                    .checkbox(&mut self.compose.multiline, "多行编辑")
                                    .changed()
                                {
                                    self.compose.focus_requested = true;
                                    ui.close();
                                }
                                ui.checkbox(
                                    &mut self.compose.append_enter,
                                    "追加 Enter（执行命令）",
                                );
                                if ui.button("预览…").clicked() {
                                    preview = true;
                                    ui.close();
                                }
                                if ui.button("快捷命令").clicked() {
                                    self.compose.page = 1;
                                    ui.close();
                                }
                                if ui.button("查找终端内容…").clicked() {
                                    self.search_requested = true;
                                    ui.close();
                                }
                                if ui.button("实时键盘同步…").clicked() {
                                    self.picker_command_only = false;
                                    self.panel_open = true;
                                    ui.close();
                                }
                                if ui.button("管理快捷命令…").clicked() {
                                    self.compose.page = 2;
                                    ui.close();
                                }
                                if self.is_armed() && ui.button("清除发送目标").clicked() {
                                    let draft = std::mem::take(&mut self.compose);
                                    self.disarm(SyncInputDisarmReason::User);
                                    self.compose = draft;
                                    self.compose.armed = false;
                                    self.compose.focus_requested = true;
                                    ui.close();
                                }
                            });
                        send |= ui
                            .add_enabled(
                                active.is_some()
                                    && self.compose_can_send()
                                    && (!self.compose.armed || self.is_armed()),
                                egui::Button::new(
                                    egui::RichText::new("发送").color(cshell_ui::theme::ACCENT),
                                )
                                .fill(cshell_ui::theme::ACCENT_FILL),
                            )
                            .on_hover_text(if multiline {
                                "Ctrl+Enter 发送；风险正文仍需确认"
                            } else {
                                "Enter 发送；风险正文仍需确认"
                            })
                            .clicked();
                        self.history_popup(ui);
                    });
                });
                if multiline {
                    send |= self.command_editor(
                        ui,
                        ui.available_width(),
                        ime_event,
                        blocked || popup_was_open,
                    );
                }
            });
        if popup_was_open && !egui::Popup::is_any_open(root.ctx()) {
            self.compose.focus_requested = true;
        }
        if close {
            self.hide_command_bar();
        } else if !blocked
            && (send || preview)
            && let Some(active) = active
        {
            self.request_compose_send(active, preview);
        }
    }
}
