//! SSH workbench hierarchy; all actions reuse existing connection commands.
use super::*;

pub(super) fn profile(model: &WorkbenchViewModel, tab: TabId) -> Option<&SavedSshProfileViewModel> {
    let id = model
        .workspace_document
        .as_ref()?
        .bindings
        .iter()
        .find(|binding| binding.tab_id == tab)?
        .profile_id?;
    model
        .saved_ssh_profiles
        .iter()
        .find(|p| p.id == id && p.kind == ProfileKind::Ssh)
}

pub(super) fn tooltip(
    model: &WorkbenchViewModel,
    tab: TabId,
    title: &str,
    started: bool,
) -> String {
    let mut text = title.to_owned();
    if let Some(profile) = profile(model, tab) {
        text.push_str(&format!("\n配置目标：{}", profile.target));
        if let Some(env)=model.profile_environments.get(&profile.id){text.push_str(&format!("\n环境：{}",env.label()));}
    }
    if let Some(state) = model.ssh_tab_states.get(&tab) {
        text.push_str(&format!("\nSSH {}", state.label()));
    } else if !started {
        text.push_str("\n尚未连接");
    }
    if let Some(realtime) = model.input_target_tabs.get(&tab) {
        text.push_str(if *realtime { "\n接收实时键盘同步" } else { "\n命令栏接收目标" });
    }
    if model.unread_output_tabs.contains(&tab) { text.push_str("\n有未查看的新输出"); }
    text.push_str("\n右键：复制、分屏、移动");
    text
}

pub(super) fn toolbar(ui: &mut egui::Ui, model: &mut WorkbenchViewModel) {
    let wide = ui.available_width() >= 1080.0;
    ui.spacing_mut().item_spacing.x = 4.0;
    ui.horizontal_centered(|ui| {
        if tool_button(ui, Icon::Add, "＋ 新建", true, false)
            .on_hover_text("新建并保存 SSH 会话").clicked() {
            model.menu_command = Some(WorkbenchMenuCommand::NewSshProfile);
        }
        if tool_button(ui, Icon::Sessions, "会话管理", true, false).clicked() {
            model.menu_command = Some(WorkbenchMenuCommand::SshConnections);
        }
        ui.separator();
        let target = model.selected_profile.and_then(|id| model.saved_ssh_profiles.iter().find(|p| p.id == id));
        let hint = target.map_or_else(|| "重新连接当前已断开的 SSH 标签".to_owned(),
            |p| format!("在新标签连接左侧选中的配置：{}\n{}", p.name, p.target));
        if tool_button(ui, Icon::Connect, "连接",
            !model.ssh_control_busy && (model.selected_profile.is_some() || model.can_reconnect_ssh),
            model.selected_profile.is_some() || model.can_reconnect_ssh).on_hover_text(hint).clicked() {
            model.menu_command = model.selected_profile.map(WorkbenchMenuCommand::OpenSavedProfile)
                .or(Some(WorkbenchMenuCommand::ReconnectNewShell));
        }
        if tool_button(ui, Icon::Disconnect, "断开", model.can_disconnect_ssh && !model.ssh_control_busy, false)
            .on_hover_text("断开当前标签的 SSH 连接及其传输，保留终端历史记录").clicked() {
            model.menu_command = Some(WorkbenchMenuCommand::DisconnectSsh);
        }
        if tool_button(ui, Icon::Reconnect, "重新连接", model.can_restart_ssh && !model.ssh_control_busy, false)
            .on_hover_text("重新连接当前标签；已连接时先关闭旧连接，不改变左侧选中的配置").clicked() {
            model.menu_command = Some(WorkbenchMenuCommand::ReconnectSsh);
        }
        ui.separator();
        if wide {
            let available = !model.ssh_control_busy && !model.layout_paused && model.selected_tab
                .and_then(|tab| model.tabs.iter().find(|t| t.id == tab)).and_then(|t| t.session_id)
                .is_some_and(|id| model.reusable_ssh_sessions.contains(&id));
            if tool_button(ui, Icon::Terminal, "复制渠道", available, false)
                .on_hover_text("复用当前已认证的 SSH 连接打开独立 Shell；无需再次输入动态码，共享连接断开会影响所有渠道").clicked()
                && let Some(tab) = model.selected_tab {
                model.menu_command = Some(WorkbenchMenuCommand::CloneSshChannel(tab));
            }
        }
        if tool_button(ui, Icon::Folder, "SFTP", model.can_sftp && !model.ssh_control_busy, false)
            .on_hover_text("为当前 SSH 连接打开独立文件管理窗口，复用现有认证").clicked() {
            model.menu_command = Some(WorkbenchMenuCommand::SftpFiles);
        }
        if wide && tool_button(ui, Icon::Search, "查找", true, false).on_hover_text("查找终端 · Ctrl+Shift+F").clicked() {
            model.menu_command = Some(WorkbenchMenuCommand::SearchTerminal);
        }
        ui.menu_button("分屏", |ui| crate::form::menu_body(ui, |ui| {
            action(ui, model, "向右分屏", WorkbenchMenuCommand::SplitWorkspace(SplitAxis::Horizontal));
            action(ui, model, "向下分屏", WorkbenchMenuCommand::SplitWorkspace(SplitAxis::Vertical));
        }));
        ui.menu_button("更多", |ui| crate::form::menu_body(ui, |ui| {
            if !wide {
                if let Some(tab) = model.selected_tab { duplicate_channel_action(ui, model, tab); }
                action(ui, model, "查找终端", WorkbenchMenuCommand::SearchTerminal);
                ui.separator();
            }
            action(ui, model, "本地终端", WorkbenchMenuCommand::NewWorkspaceTab);
            action(ui, model, "凭据管理…", WorkbenchMenuCommand::Credentials);
            action(ui, model, "全局默认设置…", WorkbenchMenuCommand::GlobalSettings);
            if model.task_view { action(ui, model, "返回终端", WorkbenchMenuCommand::TerminalView); }
            if let Some(tab) = model.selected_tab {
                ui.separator();
                if ui.add_enabled(!model.ssh_control_busy, egui::Button::new("关闭当前标签")).clicked() {
                    model.menu_command = Some(WorkbenchMenuCommand::CloseWorkspaceTab(tab));
                    ui.close();
                }
            }
        }));
    });
}

pub(super) fn identity(
    ui: &mut egui::Ui,
    model: &WorkbenchViewModel,
    rect: egui::Rect,
    tab: TabId,
    id: egui::Id,
) -> egui::Rect {
    if !is_ssh_tab(model, tab) && profile(model, tab).is_none() {
        return rect;
    }
    let bar = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.max.x, (rect.min.y + 28.0).min(rect.max.y)),
    );
    let focused = model.selected_tab == Some(tab);
    let state = model.ssh_tab_states.get(&tab).copied();
    let color = state.map_or(MUTED, ssh_state_color);
    ui.painter()
        .rect_filled(bar, 0.0, if focused { theme::SELECTED } else { PANEL });
    ui.painter().line_segment(
        [bar.left_bottom(), bar.right_bottom()],
        Stroke::new(1.0, LINE),
    );
    if focused {
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                bar.min,
                egui::pos2((bar.min.x + 3.0).min(bar.max.x), bar.max.y),
            ),
            0.0,
            ACCENT,
        );
    }
    let name = profile(model, tab)
        .map(|p| p.name.as_str())
        .or_else(|| {
            model
                .tabs
                .iter()
                .find(|t| t.id == tab)
                .map(|t| t.title.as_str())
        })
        .unwrap_or("SSH");
    let target = profile(model, tab)
        .map(|p| format!("配置：{}", p.target))
        .unwrap_or_else(|| "SSH 会话".into());
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id_salt((id, "ssh-identity"))
            .max_rect(bar.shrink2(egui::vec2(8.0, 2.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.set_clip_rect(bar);
    row.spacing_mut().item_spacing.x = 6.0;
    row.horizontal(|ui| {
        if let Some(env)=profile(model,tab).and_then(|p|model.profile_environments.get(&p.id)) {
            ui.add_sized([42.0f32.min((bar.width()*0.22).max(1.0)),22.0],egui::Label::new(RichText::new(env.label()).size(12.0).strong().color(env.color())).truncate()).on_hover_text(format!("环境标记：{}",env.label()));
        }
        let state_width = (ui.available_width() * 0.32).min(76.0);
        ui.add_sized(
            [state_width, 22.0],
            egui::Label::new(
                RichText::new(state.map_or("待连接", SshConnectionState::label))
                    .size(12.0)
                    .color(color),
            )
            .halign(Align::Min)
            .truncate(),
        );
        if focused && bar.width() >= 430.0 {
            ui.small(if state == Some(SshConnectionState::Connected) {
                "当前输入"
            } else {
                "当前面板"
            });
        }
        let name_width = (ui
            .painter()
            .layout_no_wrap(name.to_owned(), FontId::proportional(12.0), INK)
            .size()
            .x
            + 12.0)
            .min(ui.available_width() * 0.34)
            .min(160.0);
        ui.add_sized(
            [name_width, 22.0],
            egui::Label::new(RichText::new(name).size(12.0).strong())
                .halign(Align::Min)
                .truncate(),
        )
        .on_hover_text(name);
        ui.add_sized(
            [ui.available_width().max(0.0), 22.0],
            egui::Label::new(RichText::new(&target).monospace().size(12.0).color(MUTED))
                .halign(Align::Min)
                .truncate(),
        )
        .on_hover_text(format!(
            "连接配置：{name}\n{target}\n此处显示保存的连接目标。"
        ));
    });
    egui::Rect::from_min_max(egui::pos2(rect.min.x, bar.max.y), rect.max)
}
