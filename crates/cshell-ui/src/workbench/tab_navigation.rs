//! Window-scoped tab search. Navigation never creates or reconnects sessions.
use super::*;

#[derive(Clone)]
struct Entry {
    tab: TabId,
    title: String,
    target: String,
    state: String,
    group: usize,
}

pub(super) fn search_matches(query: &str, fields: &[&str]) -> bool {
    let haystack = fields.join(" ").to_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

fn entries(model: &WorkbenchViewModel) -> Vec<Entry> {
    let mut groups = Vec::new();
    if let Some(doc) = &model.workspace_document {
        if let Some(window) = model
            .workspace_window_id
            .and_then(|id| doc.windows.iter().find(|w| w.id == id))
            .or_else(|| doc.windows.first())
        {
            let mut stack = vec![window.root];
            let mut visited = std::collections::BTreeSet::new();
            while let Some(id) = stack.pop() {
                if !visited.insert(id) {
                    continue;
                }
                if let Some(pane) = doc.panes.iter().find(|p| p.id == id) {
                    match pane.content {
                        WorkspacePaneContent::Tabs { group_id } => {
                            if let Some(group) = doc.tab_groups.iter().find(|g| g.id == group_id) {
                                groups.push(group.tabs.clone());
                            }
                        }
                        WorkspacePaneContent::Split { first, second, .. } => {
                            stack.push(second);
                            stack.push(first);
                        }
                    }
                }
            }
        }
    } else {
        groups.push(model.tabs.iter().map(|t| t.id).collect());
    }
    let mut result = Vec::new();
    for (index, tabs) in groups.iter().enumerate() {
        for tab in tabs {
            let title = model
                .workspace_document
                .as_ref()
                .and_then(|d| d.bindings.iter().find(|b| b.tab_id == *tab))
                .map(|b| b.title.clone())
                .or_else(|| {
                    model
                        .tabs
                        .iter()
                        .find(|t| t.id == *tab)
                        .map(|t| t.title.clone())
                })
                .unwrap_or_else(|| "终端".into());
            let target = ssh_chrome::profile(model, *tab)
                .map(|p| p.target.clone())
                .unwrap_or_else(|| "本地终端".into());
            let state = model
                .ssh_tab_states
                .get(tab)
                .map_or("终端", |s| s.label())
                .to_owned();
            result.push(Entry {
                tab: *tab,
                title,
                target,
                state,
                group: index + 1,
            });
        }
    }
    result
}
pub(super) fn searchable_tabs(model:&WorkbenchViewModel)->Vec<(TabId,String,String)>{entries(model).into_iter().map(|e|(e.tab,e.title,e.target)).collect()}

pub(super) fn header(
    ui: &mut egui::Ui,
    model: &mut WorkbenchViewModel,
    header: egui::Rect,
    id: egui::Id,
) -> egui::Rect {
    let width = header.width().max(0.0).min(88.0);
    let tools =
        egui::Rect::from_min_max(egui::pos2(header.max.x - width, header.min.y), header.max);
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id_salt((id, "tab-tools"))
            .max_rect(tools.shrink2(egui::vec2(2.0, 1.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.set_clip_rect(header);
    row.spacing_mut().item_spacing.x = 2.0;
    if row
        .add_sized(
            [24.0_f32.min(width * 0.3), 28.0],
            egui::Button::new("+").frame(false),
        )
        .on_hover_text("新建本地终端标签")
        .clicked()
    {
        model.menu_command = Some(WorkbenchMenuCommand::NewWorkspaceTab);
    }
    let tab_list = row
        .add_enabled(
            !model.ssh_control_busy && !model.layout_paused,
            egui::Button::new("标签").min_size(egui::vec2(48.0_f32.min(width * 0.6), 28.0)),
        )
        .on_hover_text(format!("搜索本窗口全部标签（含其他分屏） · Ctrl+Shift+P\n{} 个标签有新输出", model.unread_output_tabs.len()));
    if !model.unread_output_tabs.is_empty() {
        row.painter().circle_filled(tab_list.rect.right_top() + egui::vec2(-5.0, 5.0), 3.0, theme::DANGER);
    }
    if tab_list.clicked() {
        model.tab_navigator.show(model.selected_tab);
    }
    egui::Rect::from_min_max(
        header.min,
        egui::pos2((tools.min.x - 2.0).max(header.min.x), header.max.y),
    )
}

pub(super) fn draw(context: &egui::Context, model: &mut WorkbenchViewModel) {
    if !model.tab_navigator.open {
        return;
    }
    let opening = model.tab_navigator.focus_requested;
    let all = entries(model);
    let mut selected_tab = None;
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("ssh-tab-navigator")).show(context, |ui| {
        ui.set_width((context.content_rect().width() - 48.0).clamp(80.0, 620.0));
        ui.horizontal(|ui| {
            ui.strong("切换标签");
            ui.weak(format!("当前窗口 · {} 个标签", all.len()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("关闭列表").clicked() {
                    close = true;
                }
            });
        });
        let (up, down, enter) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });
        let query = ui.add(
            egui::TextEdit::singleline(&mut model.tab_navigator.query)
                .id_salt("tab-navigator-query")
                .char_limit(256)
                .hint_text("搜索名称 / 主机 / 用户名 / 分屏")
                .desired_width(f32::INFINITY),
        );
        if model.tab_navigator.focus_requested {
            query.request_focus();
            model.tab_navigator.focus_requested = false;
        }
        let filtered: Vec<_> = all
            .iter()
            .filter(|entry| {
                search_matches(
                    &model.tab_navigator.query,
                    &[
                        &entry.title,
                        &entry.target,
                        &entry.state,
                        &format!("分屏 {}", entry.group),
                    ],
                )
            })
            .collect();
        if !filtered
            .iter()
            .any(|e| Some(e.tab) == model.tab_navigator.selected)
        {
            model.tab_navigator.selected = filtered.first().map(|e| e.tab);
        }
        if !filtered.is_empty() && (up || down) {
            let index = filtered
                .iter()
                .position(|e| Some(e.tab) == model.tab_navigator.selected)
                .unwrap_or(0);
            let next = if down {
                (index + 1) % filtered.len()
            } else {
                (index + filtered.len() - 1) % filtered.len()
            };
            model.tab_navigator.selected = Some(filtered[next].tab);
        }
        let allowed = !model.ssh_control_busy && !model.layout_paused;
        ui.separator();
        egui::ScrollArea::vertical()
            .animated(false)
            .id_salt("tab-navigator-results")
            .max_height((context.content_rect().height() - 180.0).clamp(48.0, 360.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if filtered.is_empty() {
                    ui.weak("没有匹配的标签");
                }
                for entry in &filtered {
                    let selected = Some(entry.tab) == model.tab_navigator.selected;
                    let mut full = format!(
                        "{} · {} · {} · 分屏 {}",
                        entry.title, entry.target, entry.state, entry.group
                    );
                    let unread = model.unread_output_tabs.contains(&entry.tab);
                    if unread { full.push_str(" · 有新输出"); }
                    let response = ui
                        .push_id(entry.tab, |ui| {
                            let response = ui.add_enabled(
                                allowed,
                                egui::Button::new("")
                                    .selected(selected)
                                    .min_size(egui::vec2(ui.available_width(), 40.0)),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(egui::WidgetType::Button, allowed, &full)
                            });
                            let rect = response.rect;
                            let painter = ui.painter().with_clip_rect(rect);
                            if unread { painter.circle_filled(rect.right_top() + egui::vec2(-10.0, 11.0), 3.0, theme::DANGER); }
                            let text = rect.shrink2(egui::vec2(9.0, 0.0));
                            paint_line(
                                &painter,
                                text,
                                rect.min.y + 11.0,
                                &entry.title,
                                FontId::proportional(13.0),
                                INK,
                            );
                            paint_line(
                                &painter,
                                text,
                                rect.min.y + 28.0,
                                &format!(
                                    "{} · {} · 分屏 {}",
                                    entry.target, entry.state, entry.group
                                ),
                                FontId::monospace(11.0),
                                MUTED,
                            );
                            response.on_hover_text(&full)
                        })
                        .inner;
                    if selected && (opening || up || down || query.changed()) {
                        response.scroll_to_me(Some(Align::Center));
                    }
                    if response.clicked() {
                        selected_tab = Some(entry.tab);
                    }
                }
            });
        ui.separator();
        ui.weak("↑ / ↓ 选择 · Enter 切换 · Esc 取消；只切换已有标签");
        if enter && allowed {
            selected_tab = model.tab_navigator.selected;
        }
    });
    if let Some(tab) = selected_tab {
        model.selected_tab = Some(tab);
        close = true;
    }
    if close || response.should_close() {
        model.tab_navigator.close();
    }
}

#[cfg(test)]
mod tests;
