//! Classic terminal layout. Menus and controls never draw into the GPU viewport.
use super::*;
mod ssh_chrome;
mod tab_navigation;
use crate::theme::{self, ACCENT, LINE, MUTED, PANEL, TEXT as INK};
use crate::workbench_icons::{Icon, paint as paint_icon};
use egui::{Align, Color32, FontId, Frame, Layout, RichText, Stroke, TextStyle};

const TAB_BAR: Color32 = theme::BACKGROUND;
const TAB_ACTIVE: Color32 = theme::RAISED;
const SIDEBAR_MIN_WIDTH: f32 = 120.0;
pub fn searchable_window_tabs(model:&WorkbenchViewModel)->Vec<(TabId,String,String)>{tab_navigation::searchable_tabs(model)}

pub fn install_workbench_style(context: &egui::Context) {
    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = PANEL;
    style.visuals.override_text_color = Some(INK);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.extreme_bg_color = theme::INPUT;
    style.visuals.faint_bg_color = theme::RAISED;
    style.visuals.code_bg_color = theme::INPUT;
    style.visuals.hyperlink_color = ACCENT;
    style.visuals.warn_fg_color = theme::WARNING;
    style.visuals.error_fg_color = theme::DANGER;
    style.visuals.selection.bg_fill = theme::SELECTED;
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    style.visuals.window_stroke = Stroke::new(1.0, LINE);
    style.visuals.window_corner_radius = egui::CornerRadius::same(4);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(3);
    style.visuals.widgets.inactive.weak_bg_fill = theme::RAISED;
    style.visuals.widgets.inactive.bg_fill = theme::RAISED;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, INK);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(46, 57, 70);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(46, 57, 70);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, MUTED);
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, INK);
    style.visuals.widgets.active.weak_bg_fill = theme::ACCENT_FILL;
    style.visuals.widgets.active.bg_fill = theme::ACCENT_FILL;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, ACCENT);
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.noninteractive,
    ] {
        widget.corner_radius = egui::CornerRadius::same(2);
    }
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    style.spacing.item_spacing = egui::vec2(6.0, 5.0);
    style.spacing.button_padding = egui::vec2(9.0, 5.0);
    style.spacing.interact_size.y = 26.0;
    style.spacing.text_edit_width = 220.0;
    style.spacing.combo_width = 160.0;
    style.spacing.menu_width = 360.0;
    style.wrap_mode = Some(egui::TextWrapMode::Wrap);
    style.spacing.window_margin = egui::Margin::same(12);
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(12.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(16.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(12.0));
    context.set_style_of(egui::Theme::Dark, style);
    context.set_theme(egui::ThemePreference::Dark);
}

fn action(
    ui: &mut egui::Ui,
    model: &mut WorkbenchViewModel,
    label: &str,
    command: WorkbenchMenuCommand,
) {
    if crate::form::menu_item(ui, label, true).clicked() {
        model.menu_command = Some(command);
        ui.close();
    }
}
fn prompt_termination(model: &mut WorkbenchViewModel, id: SessionId) {
    model.terminate_target = Some(id);
    model.terminate_pending = false;
    model.terminate_error = None;
    model.confirm_terminate = true;
}
fn compact(value: &str, maximum: usize) -> String {
    let mut chars = value.chars();
    let mut text: String = chars.by_ref().take(maximum).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

fn paint_line(
    painter: &egui::Painter,
    rect: egui::Rect,
    center_y: f32,
    text: &str,
    font: FontId,
    color: Color32,
) {
    let mut job =
        egui::text::LayoutJob::simple(text.to_owned(), font, color, rect.width().max(0.0));
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(rect.min.x, center_y - galley.size().y / 2.0),
        galley,
        color,
    );
}

fn tool_button(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    primary: bool,
) -> egui::Response {
    let text = ui.painter().layout_no_wrap(
        label.trim_start_matches("＋ ").into(),
        FontId::proportional(13.0),
        if primary { ACCENT } else { INK },
    );
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(text.size().x + 38.0, 28.0),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let painter = ui.painter().with_clip_rect(rect);
    let color = if !enabled {
        theme::DISABLED
    } else if primary {
        ACCENT
    } else {
        INK
    };
    let fill = if primary {
        if response.hovered() {
            theme::ACCENT_HOVER
        } else {
            theme::ACCENT_FILL
        }
    } else if enabled && response.hovered() {
        theme::RAISED
    } else {
        Color32::TRANSPARENT
    };
    painter.rect_filled(rect, 2.0, fill);
    if primary {
        painter.rect_stroke(
            rect,
            2.0,
            Stroke::new(1.0, theme::ACCENT_HOVER),
            egui::StrokeKind::Inside,
        );
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(1.0),
            2.0,
            Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    paint_icon(
        &painter,
        egui::pos2(rect.min.x + 14.0, rect.center().y),
        icon,
        color,
    );
    // Reuse the same font for measurement and painting, including disabled controls.
    painter.text(
        egui::pos2(rect.min.x + 28.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label.trim_start_matches("＋ "),
        FontId::proportional(13.0),
        color,
    );
    response.on_hover_cursor(if enabled {
        egui::CursorIcon::PointingHand
    } else {
        egui::CursorIcon::Default
    })
}

/// Shared by the terminal workbench and the explicit batch-task view.
pub fn draw_workbench_chrome(ui: &mut egui::Ui, model: &mut WorkbenchViewModel) {
    egui::Panel::top("classic-menu")
        .exact_size(28.0)
        .frame(
            Frame::NONE
                .fill(theme::BACKGROUND)
                .inner_margin(egui::Margin::symmetric(8, 1)),
        )
        .show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let(rect,_)=ui.allocate_exact_size(egui::vec2(22.0,22.0),egui::Sense::hover());crate::brand::paint(ui.painter(),rect);
                ui.menu_button("文件", |ui| {
                    crate::form::menu_body(ui, |ui| {
                        action(
                            ui,
                            model,
                            "新建 SSH 会话…",
                            WorkbenchMenuCommand::NewSshProfile,
                        );
                        action(
                            ui,
                            model,
                            "打开本地终端",
                            WorkbenchMenuCommand::NewWorkspaceTab,
                        );
                        action(ui, model, "会话管理…", WorkbenchMenuCommand::SshConnections);
                        ui.separator();
                        action(
                            ui,
                            model,
                            "新建窗口",
                            WorkbenchMenuCommand::NewWorkspaceWindow,
                        );
                        action(ui, model, "导入 / 导出…", WorkbenchMenuCommand::Profiles);
                        ui.separator();
                        action(ui, model, "退出", WorkbenchMenuCommand::Quit);
                    })
                });
                ui.menu_button("编辑", |ui| {
                    crate::form::menu_body(ui, |ui| {
                        action(ui, model, "查找终端", WorkbenchMenuCommand::SearchTerminal);
                        action(ui, model, "命令工作区", WorkbenchMenuCommand::ShellHistory);
                        action(ui, model, "快捷命令", WorkbenchMenuCommand::QuickCommands);
                        if let Some(id) = model.selected_profile {
                            action(
                                ui,
                                model,
                                "编辑选中会话…",
                                WorkbenchMenuCommand::EditSavedProfile(id),
                            );
                        }
                        ui.separator();
                        action(
                            ui,
                            model,
                            "发送栏 / 快捷命令…",
                            WorkbenchMenuCommand::Compose,
                        );
                        action(ui, model, "同步输入…", WorkbenchMenuCommand::SyncInput);
                    })
                });
                ui.menu_button("查看", |ui| {
                    crate::form::menu_body(ui, |ui| {
                        let mut visible = !model.sidebar_hidden;
                        if ui.checkbox(&mut visible, "会话管理器").changed() {
                            model.sidebar_hidden = !visible;
                        }
                        let mut toolbar = !model.toolbar_hidden;
                        if ui.checkbox(&mut toolbar,"连接工具栏").changed(){model.toolbar_hidden=!toolbar;}
                        ui.separator();
                        action(ui, model, "终端工作台", WorkbenchMenuCommand::TerminalView);
                        if ui
                            .add_enabled(model.can_sftp, egui::Button::new("SFTP 文件…"))
                            .clicked()
                        {
                            model.menu_command = Some(WorkbenchMenuCommand::SftpFiles);
                            ui.close();
                        }
                        action(ui, model, "批量任务", WorkbenchMenuCommand::Tasks);
                        action(
                            ui,
                            model,
                            "主题 / 高亮 / 回放…",
                            WorkbenchMenuCommand::TerminalTools,
                        );
                    })
                });
                ui.menu_button("会话", |ui| {
                    crate::form::menu_body(ui, |ui| {
                        if let Some(tab) = model.selected_tab {
                            let running = model
                                .tabs
                                .iter()
                                .any(|t| t.id == tab && t.session_id.is_some());
                            if ui
                                .add_enabled(running, egui::Button::new(if is_ssh_tab(model,tab) {"复制 SSH 会话"} else {"复制为新会话"}))
                                .clicked()
                            {
                                model.menu_command =
                                    Some(WorkbenchMenuCommand::CloneWorkspaceSession(tab));
                                ui.close();
                            }
                            if is_ssh_tab(model,tab) { duplicate_channel_action(ui,model,tab); }
                            action(
                                ui,
                                model,
                                "关闭当前标签",
                                WorkbenchMenuCommand::CloseWorkspaceTab(tab),
                            );
                        }
                        if model.can_reconnect_ssh {
                            action(
                                ui,
                                model,
                                "重新连接（新 Shell）",
                                WorkbenchMenuCommand::ReconnectNewShell,
                            );
                        }
                        ui.separator();
                        action(
                            ui,
                            model,
                            "向右分屏",
                            WorkbenchMenuCommand::SplitWorkspace(SplitAxis::Horizontal),
                        );
                        action(
                            ui,
                            model,
                            "向下分屏",
                            WorkbenchMenuCommand::SplitWorkspace(SplitAxis::Vertical),
                        );
                        action(
                            ui,
                            model,
                            "将当前分屏移到新窗口",
                            WorkbenchMenuCommand::DetachWorkspaceGroup,
                        );
                        ui.separator();
                        if ui
                            .add_enabled(
                                model.selected.is_some(),
                                egui::Button::new("终止会话进程…"),
                            )
                            .clicked()
                        {
                            if let Some(id) = model.selected {
                                prompt_termination(model, id);
                            }
                            ui.close();
                        }
                    })
                });
                ui.menu_button("工具", |ui| {
                    crate::form::menu_body(ui, |ui| {
                        action(ui, model, "快捷操作…    Ctrl+Shift+K", WorkbenchMenuCommand::CommandCenter);
                        action(ui, model, "键盘快捷键…", WorkbenchMenuCommand::KeyboardSettings);
                        ui.separator();
                        action(ui, model, "全局默认设置…", WorkbenchMenuCommand::GlobalSettings);
                        action(ui, model, "凭据管理…", WorkbenchMenuCommand::Credentials);
                        action(ui, model, "终端上传任务…", WorkbenchMenuCommand::TerminalUploads);
                        action(ui, model, "ZMODEM 传输…", WorkbenchMenuCommand::ZmodemTransfers);
                        ui.separator();
                        action(ui, model, "主机与主机组…", WorkbenchMenuCommand::Hosts);
                        action(ui, model, "批量命令任务", WorkbenchMenuCommand::Tasks);
                        ui.menu_button("批量文件与混合任务", |ui| {
                            crate::form::menu_body(ui, |ui| {
                                action(ui, model, "多主机上传", WorkbenchMenuCommand::UploadTask);
                                action(ui, model, "多主机下载", WorkbenchMenuCommand::DownloadTask);
                                action(ui, model, "混合任务", WorkbenchMenuCommand::MixedTask);
                            })
                        });
                        ui.separator();
                        action(ui, model, "SSH 隧道…", WorkbenchMenuCommand::Tunnels);
                        ui.menu_button("高级设置", |ui| {
                            crate::form::menu_body(ui, |ui| {
                                action(ui, model, "CA 与证书…", WorkbenchMenuCommand::HostCa);
                                action(ui, model, "SSH 能力…", WorkbenchMenuCommand::SshProviders);
                                action(
                                    ui,
                                    model,
                                    "批量编辑 / 继承来源…",
                                    WorkbenchMenuCommand::ProfileBatch,
                                );
                                action(
                                    ui,
                                    model,
                                    "标签 / Host / 临时继承…",
                                    WorkbenchMenuCommand::TerminalPolicies,
                                );
                                action(
                                    ui,
                                    model,
                                    "剪贴板安全策略…",
                                    WorkbenchMenuCommand::ClipboardPolicy,
                                );
                                ui.separator();
                                action(ui, model, "工作区恢复…", WorkbenchMenuCommand::WorkspaceRecovery);
                                action(ui, model, "重试未打开的窗口", WorkbenchMenuCommand::RetryWorkspaceWindows);
                            })
                        });
                    })
                });
                ui.menu_button("帮助", |ui| {
                    crate::form::menu_body(ui, |ui| {
                        if ui.button("关于 CShell").clicked() {
                            model.about_open = true;
                            ui.close();
                        }
                    })
                });
                if ui.available_width() > 150.0 {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.available_width()>260.0 && ui.small_button("快捷操作    Ctrl+Shift+K").on_hover_text("搜索并执行当前窗口的功能").clicked(){model.menu_command=Some(WorkbenchMenuCommand::CommandCenter);}
                        ui.label(RichText::new("CShell").monospace().size(11.0).color(MUTED));
                    });
                }
            });
        });
    if !model.toolbar_hidden {egui::Panel::top("classic-toolbar")
        .exact_size(38.0)
        .frame(
            Frame::NONE
                .fill(PANEL)
                .stroke(Stroke::new(1.0, LINE))
                .inner_margin(egui::Margin::symmetric(10, 4)),
        )
        .show(ui, |ui| {
            ssh_chrome::toolbar(ui, model);
        });}
    if let Some(warning)=model.input_warning.clone() {
        let feedback=crate::feedback::explain(&warning);
        egui::Panel::top("workbench-notice").frame(crate::form::utility_bar()).show(ui,|ui|{
            ui.horizontal_wrapped(|ui|{
                ui.colored_label(theme::WARNING,feedback.title);
                let reserved=if model.can_reconnect_ssh {280.0}else{190.0};
                ui.add_sized([(ui.available_width()-reserved).max(80.0),24.0],egui::Label::new(if feedback.guidance.is_empty(){compact(&warning,64)}else{feedback.guidance.into()}).truncate()).on_hover_text(&warning);
                ui.menu_button("详情",|ui|{crate::form::menu_body(ui,|ui|{ui.label(&warning);if ui.button("复制详情").clicked(){ui.ctx().copy_text(warning.clone());ui.close();}});});
                if feedback.action==crate::feedback::Action::Reconnect && model.can_restart_ssh && !model.ssh_control_busy && ui.small_button("重新连接").clicked(){model.menu_command=Some(WorkbenchMenuCommand::ReconnectSsh);}
                if feedback.action==crate::feedback::Action::Unlock && ui.small_button("解锁凭据库").clicked(){model.menu_command=Some(WorkbenchMenuCommand::VaultSettings);}
                if feedback.action==crate::feedback::Action::InspectFiles && model.can_sftp && ui.small_button("查看文件").clicked(){model.menu_command=Some(WorkbenchMenuCommand::SftpFiles);}
                if ui.small_button("关闭提示").clicked(){model.input_warning=None;}
            });
        });
    }
    egui::Panel::bottom("classic-status")
        .exact_size(24.0)
        .frame(
            Frame::NONE
                .fill(theme::BACKGROUND)
                .stroke(Stroke::new(1.0, LINE))
                .inner_margin(egui::Margin::symmetric(10, 2)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let state = model.selected_tab.and_then(|tab| model.ssh_tab_states.get(&tab).copied());
                if model.sidebar_hidden && sidebar_icon_button(ui, "›", "展开会话管理器").clicked() {
                    model.sidebar_hidden = false;
                }
                let (status, color) = if let Some(state) = state {
                    (format!("{} SSH {}", if state == SshConnectionState::Connected { "●" } else { "○" }, state.label()), ssh_state_color(state))
                } else if model.daemon_connected {
                    ("● 已连接".into(), theme::SUCCESS)
                } else {
                    ("○ 未连接".into(), MUTED)
                };
                ui.label(RichText::new(status).size(12.0).color(color))
                    .on_hover_text(&model.daemon_status_detail);
                if let Some(tab) = model.tabs.iter().find(|t| Some(t.id) == model.selected_tab) {
                    ui.separator();
                    ui.label(
                        RichText::new(compact(&tab.title, 24))
                            .size(12.0)
                            .color(MUTED),
                    )
                    .on_hover_text(&tab.title);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().interact_size.y = 18.0;
                    ui.spacing_mut().button_padding.y = 1.0;
                    if state.is_some() && ui.selectable_label(model.connection_diagnostics_open, "连接诊断").clicked() {
                        model.connection_diagnostics_open = !model.connection_diagnostics_open;
                    }
                    if ui.available_width()>250.0 && let Some(status)=&model.vault_status {
                        if ui.small_button(status).on_hover_text("打开全局凭据库设置；连接会按需解锁，无需提前操作").clicked(){model.menu_command=Some(WorkbenchMenuCommand::VaultSettings);}
                        ui.separator();
                    }
                    if let Some(zoom)=model.terminal_zoom_pixels {
                        if ui.small_button(format!("{zoom} px · 临时 ↺")).on_hover_text("当前窗口临时字号，点击或 Ctrl+0 恢复全局默认").clicked(){model.terminal_zoom_pixels=None;}
                    } else {
                    let pixels = model.terminal_font_pixels.get_or_insert(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS);
                    ui.add(egui::DragValue::new(pixels)
                        .range(cshell_domain::MIN_TERMINAL_FONT_PIXELS..=cshell_domain::MAX_TERMINAL_FONT_PIXELS)
                        .speed(1.0).suffix(" px"))
                        .on_hover_text("终端字号：拖动或点击输入数值，立即生效并保存");
                    ui.small("字号");
                    }
                    ui.separator();
                    if model.layout_paused {
                        if ui
                            .small_button("布局待处理")
                            .on_hover_text(&model.workspace_status)
                            .clicked()
                        {
                            model.menu_command = Some(WorkbenchMenuCommand::WorkspaceRecovery);
                        }
                    } else {
                        ui.small(format!("{} 个标签", model.tabs.len()))
                            .on_hover_text(&model.workspace_status);
                    }
                });
            });
        });
    draw_dialogs(ui.ctx(), model);
}

fn matches_profile(profile: &SavedSshProfileViewModel, search: &str) -> bool {
    tab_navigation::search_matches(search, &[&profile.name, &profile.target])
}
fn matches_profile_or_folder(profile: &SavedSshProfileViewModel, folders: &[ProfileFolder], search: &str) -> bool {
    if matches_profile(profile, search) {return true;}
    let mut current=profile.folder_id;
    for _ in 0..16 {
        let Some(folder)=current.and_then(|id|folders.iter().find(|f|f.id==id)) else {return false;};
        if tab_navigation::search_matches(search,&[&folder.name]) {return true;}
        current=folder.parent_id;
    }
    false
}
fn folder_matches(
    folder: FolderId,
    folders: &[ProfileFolder],
    profiles: &[SavedSshProfileViewModel],
    search: &str,
    depth: usize,
) -> bool {
    depth < 16
        && (search.is_empty()
            || folders
                .iter()
                .any(|f| f.id == folder && tab_navigation::search_matches(search, &[&f.name]))
            || profiles
                .iter()
                .any(|p| p.folder_id == Some(folder) && matches_profile(p, search))
            || folders.iter().any(|f| {
                f.parent_id == Some(folder)
                    && folder_matches(f.id, folders, profiles, search, depth + 1)
            }))
}
fn profile_row(
    ui: &mut egui::Ui,
    model: &mut WorkbenchViewModel,
    profile: &SavedSshProfileViewModel,
) {
    ui.push_id(profile.id, |ui| {
        let name = if profile.favorite {
            format!("★ {}", profile.name)
        } else {
            profile.name.clone()
        };
        let response = ui
            .add_sized(
                [ui.available_width(), 38.0],
                egui::Button::new("")
                    .selected(model.selected_profile == Some(profile.id))
                    .frame(false)
                    .truncate(),
            )
            .on_hover_text(format!(
                "{}\n{}\n{} · 双击连接，右键打开 SFTP 或编辑",
                profile.name,
                profile.target,
                if profile.kind == ProfileKind::Ssh {
                    "SSH"
                } else {
                    "本地终端"
                }
            ));
        let rect = response.rect;
        let selected = model.selected_profile == Some(profile.id);
        let painter = ui.painter().with_clip_rect(rect);
        if selected {
            painter.rect_filled(rect, 2.0, theme::SELECTED);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    rect.min + egui::vec2(0.0, 6.0),
                    egui::vec2(2.0, rect.height() - 12.0),
                ),
                1.5,
                ACCENT,
            );
        }
        let icon_center = egui::pos2(rect.min.x + 16.0, rect.center().y);
        paint_icon(
            &painter,
            icon_center,
            Icon::Terminal,
            if selected { ACCENT } else { MUTED },
        );
        let text_rect = egui::Rect::from_min_max(
            rect.min + egui::vec2(32.0, 0.0),
            rect.max - egui::vec2(4.0, 0.0),
        );
        let text_painter = painter.with_clip_rect(text_rect);
        paint_line(
            &text_painter,
            text_rect,
            rect.min.y + 11.0,
            &name,
            FontId::proportional(13.0),
            INK,
        );
        paint_line(
            &text_painter,
            text_rect,
            rect.min.y + 27.0,
            &profile.target,
            FontId::monospace(10.5),
            MUTED,
        );
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::Button,
                ui.is_enabled(),
                model.selected_profile == Some(profile.id),
                &name,
            )
        });
        if response.clicked() {
            model.selected_profile = Some(profile.id);
        }
        if response.double_clicked() {
            model.menu_command = Some(WorkbenchMenuCommand::OpenSavedProfile(profile.id));
        }
        response.context_menu(|ui| {
            crate::form::menu_body(ui, |ui| {
                action(
                    ui,
                    model,
                    "连接会话",
                    WorkbenchMenuCommand::OpenSavedProfile(profile.id),
                );
                if profile.kind == ProfileKind::Ssh {
                    action(ui,model,"打开 SFTP",WorkbenchMenuCommand::OpenSavedSftp(profile.id));
                }
                action(
                    ui,
                    model,
                    "编辑会话…",
                    WorkbenchMenuCommand::EditSavedProfile(profile.id),
                );
                ui.separator();
                action(
                    ui,
                    model,
                    "新建 SSH 会话…",
                    WorkbenchMenuCommand::NewSshProfile,
                );
            })
        });
    });
}
fn folder_rows(
    ui: &mut egui::Ui,
    model: &mut WorkbenchViewModel,
    folders: &[ProfileFolder],
    profiles: &[SavedSshProfileViewModel],
    parent: Option<FolderId>,
    search: &str,
    depth: usize,
) {
    if depth >= 16 {
        return;
    }
    let mut children: Vec<_> = folders.iter().filter(|f| f.parent_id == parent).collect();
    children.sort_by_key(|f| f.name.to_lowercase());
    for folder in children {
        if !folder_matches(folder.id, folders, profiles, search, depth) {
            continue;
        }
        let next_search = if tab_navigation::search_matches(search, &[&folder.name]) {
            ""
        } else {
            search
        };
        sidebar_header(ui, RichText::new(&folder.name).strong().color(INK))
            .id_salt(("session-folder", folder.id))
            .default_open(true)
            .open(if search.is_empty() { None } else { Some(true) })
            .show(ui, |ui| {
                folder_rows(
                    ui,
                    model,
                    folders,
                    profiles,
                    Some(folder.id),
                    next_search,
                    depth + 1,
                )
            })
            .header_response
            .on_hover_text(&folder.name);
    }
    for profile in profiles
        .iter()
        .filter(|p| p.folder_id == parent && matches_profile(p, search))
    {
        profile_row(ui, model, profile);
    }
}
#[cfg(test)]
mod sidebar_tests {
    fn sidebar_frame(
        context: &egui::Context,
        model: &mut super::WorkbenchViewModel,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut output = context.run_ui(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 760.0))),
            events,
            ..Default::default()
        }, |ui| super::draw_sidebar(ui, model));
        output.textures_delta.clear();
        output
    }

    fn text_shapes<'a>(shape: &'a egui::Shape, result: &mut Vec<&'a egui::epaint::TextShape>) {
        match shape {
            egui::Shape::Text(text) => result.push(text),
            egui::Shape::Vec(shapes) => for shape in shapes { text_shapes(shape, result); },
            _ => {}
        }
    }

    #[test]
    fn narrow_sidebar_elides_text_and_keeps_controls_inside_released_width() {
        use super::*;
        let context = crate::tests::classic_context();
        context.enable_accesskit();
        let folder_name = "生产环境部署会话分组".repeat(8);
        let profile_name = "Production SSH server with a long name";
        let target = "operator@very-long-production-host.example.test:2222";
        let mut model = WorkbenchViewModel::default();
        let mut parent = None;
        for _ in 0..6 {
            let id = FolderId::new();
            model.profile_folders.push(ProfileFolder {
                id, name: folder_name.clone(), parent_id: parent, terminal: Default::default(),
            });
            parent = Some(id);
        }
        for folder_id in [None, parent] {
            model.saved_ssh_profiles.push(SavedSshProfileViewModel {
                id: ProfileId::new(), name: profile_name.into(), target: target.into(),
                favorite: false, folder_id, kind: ProfileKind::Ssh,
            });
        }
        for _ in 0..3 { sidebar_frame(&context, &mut model, vec![]); }
        let width = || egui::containers::panel::PanelState::load(&context, egui::Id::new("classic-session-tree")).unwrap().size().x;
        let edge = egui::pos2(width(), 350.0);
        sidebar_frame(&context, &mut model, vec![egui::Event::PointerMoved(edge), egui::Event::PointerButton {
            pos: edge, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE,
        }]);
        let end = egui::pos2(120.0, 350.0);
        for _ in 0..2 { sidebar_frame(&context, &mut model, vec![egui::Event::PointerMoved(end)]); }
        sidebar_frame(&context, &mut model, vec![egui::Event::PointerButton {
            pos: end, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE,
        }, egui::Event::PointerMoved(egui::pos2(1000.0, 500.0))]);
        for _ in 0..120 {
            sidebar_frame(&context, &mut model, vec![]);
            assert!((width() - 120.0).abs() <= 1.0, "narrow sidebar expanded: {}", width());
        }
        let output = sidebar_frame(&context, &mut model, vec![]);
        let mut texts = Vec::new();
        for shape in &output.shapes { text_shapes(&shape.shape, &mut texts); }
        for source in [&folder_name, profile_name, target] {
            let matches: Vec<_> = texts.iter().filter(|text| text.galley.text() == source).collect();
            assert!(matches.len() >= 2, "nested and root text should remain visible: {source}");
            for text in matches {
                assert!(text.galley.elided, "text should be elided: {source}");
                let rendered: String = text.galley.rows.iter().flat_map(|row| row.glyphs.iter()).map(|glyph| glyph.chr).collect();
                assert!(rendered.ends_with('…'), "missing ellipsis: {rendered}");
                assert!(rendered.starts_with(source.chars().next().unwrap()), "prefix lost: {rendered}");
                assert!(text.pos.x + text.galley.size().x <= 111.0, "text extends past sidebar: {rendered}");
            }
        }
        let tree = output.platform_output.accesskit_update.unwrap();
        for label in ["收起会话管理器", "自动适应侧栏宽度", "新建 SSH 会话"] {
            let center = crate::tests::button_center(&tree, label);
            assert!(center.x > 0.0 && center.x < 120.0, "control outside narrow sidebar: {label} {center:?}");
        }
        let new = crate::tests::button_center(&tree, "新建 SSH 会话");
        for pressed in [true, false] {
            sidebar_frame(&context, &mut model, vec![egui::Event::PointerMoved(new), egui::Event::PointerButton {
                pos: new, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE,
            }]);
        }
        assert_eq!(model.menu_command, Some(WorkbenchMenuCommand::NewSshProfile));
    }

    #[test]
    fn session_manager_has_no_default_local_terminal_entry() {
        let context = crate::tests::classic_context();
        context.enable_accesskit();
        let mut model = super::WorkbenchViewModel::default();
        for _ in 0..2 {
            let mut output = context.run_ui(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 760.0))),
                ..Default::default()
            }, |ui| super::draw_sidebar(ui, &mut model));
            output.textures_delta.clear();
            let tree = output.platform_output.accesskit_update.unwrap();
            assert!(tree.nodes.iter().any(|(_, node)| node.label() == Some("会话管理器") || node.value() == Some("会话管理器")));
            assert!(!tree.nodes.iter().any(|(_, node)| node.label() == Some("本地终端")));
            assert!(model.menu_command.is_none());
        }
    }
}

fn sidebar_icon_button(ui: &mut egui::Ui, glyph: &str, label: &str) -> egui::Response {
    let response = ui.small_button(glyph).on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response
}

fn sidebar_header(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) -> egui::CollapsingHeader {
    // CollapsingHeader forces Extend even when the surrounding UI truncates.
    // Pre-layout within the remaining space, retaining the full source text for
    // accessibility. Bound indentation so deep trees remain usable when narrow.
    ui.spacing_mut().indent = 18.0_f32.min((ui.available_width() - 80.0).max(0.0));
    let width =
        (ui.available_width() - ui.spacing().indent - ui.spacing().button_padding.x).max(0.0);
    let galley = text.into().into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        width,
        TextStyle::Button,
    );
    egui::CollapsingHeader::new(galley)
}

fn sidebar_auto_width(ui: &egui::Ui, model: &WorkbenchViewModel) -> f32 {
    let depth = |mut parent: Option<FolderId>| {
        let mut depth = 0;
        while let Some(id) = parent {
            if depth == 16 { break; }
            depth += 1;
            parent = model.profile_folders.iter().find(|folder| folder.id == id).and_then(|folder| folder.parent_id);
        }
        depth as f32 * 18.0
    };
    let measure = |text: &str, font: FontId| ui.painter().layout_no_wrap(text.to_owned(), font, INK).size().x;
    let mut width = 244.0_f32;
    for profile in &model.saved_ssh_profiles {
        let indent = depth(profile.folder_id);
        width = width.max(measure(&profile.name, FontId::proportional(13.0)) + indent + 80.0);
        width = width.max(measure(&profile.target, FontId::monospace(11.0)) + indent + 70.0);
    }
    for folder in &model.profile_folders {
        width = width.max(measure(&folder.name, FontId::proportional(13.0)) + depth(folder.parent_id) + 70.0);
    }
    for session in &model.sessions {
        width = width.max(measure(&session.title, FontId::proportional(13.0)) + 80.0);
    }
    width.min((ui.available_width() * 0.4).clamp(220.0, 520.0))
}

fn draw_sidebar(ui: &mut egui::Ui, model: &mut WorkbenchViewModel) {
    if model.sidebar_hidden {
        return;
    }
    let panel_id = egui::Id::new("classic-session-tree");
    let resize_id = panel_id.with("__resize");
    let max_width = (ui.available_width() * 0.6).clamp(SIDEBAR_MIN_WIDTH, 640.0);
    if let Some(response) = ui.read_response(resize_id) {
        let release_position = ui.input(|input| input.raw.events.iter().rev().find_map(|event| {
            match event {
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, .. } => Some(*pos),
                _ => None,
            }
        }));
        let position = if response.drag_stopped() {
            // Use the actual release event, not a later pointer position in the
            // same frame or a stale drag-stopped response on the next frame.
            release_position
        } else if response.dragged() && ui.input(|input| input.pointer.primary_down()) {
            response.interact_pointer_pos()
        } else {
            None
        };
        if let Some(position) = position {
            model.sidebar_width = Some((position.x - ui.available_rect_before_wrap().left()).clamp(SIDEBAR_MIN_WIDTH, max_width));
        }
    }
    let auto_width = sidebar_auto_width(ui, model);
    let width = model.sidebar_width.map_or(auto_width, |width| width.clamp(SIDEBAR_MIN_WIDTH, max_width));
    let folders = model.profile_folders.clone();
    let profiles = model.saved_ssh_profiles.clone();
    egui::Panel::left("classic-session-tree")
        .resizable(true)
        // Content (especially collapsing-header labels) must never enlarge the
        // released manual width. Resizing updates the explicit width above.
        .exact_size(width)
        .frame(
            Frame::NONE
                .fill(PANEL)
                .stroke(Stroke::new(1.0, LINE))
                .inner_margin(egui::Margin::symmetric(10, 12)),
        )
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().button_padding.x = 5.0;
                    if sidebar_icon_button(ui, "‹", "收起会话管理器").clicked() {
                        model.sidebar_hidden = true;
                    }
                    if sidebar_icon_button(ui, "↔", "自动适应侧栏宽度").clicked() {
                        model.sidebar_width = None;
                    }
                    if sidebar_icon_button(ui, "＋", "新建 SSH 会话").clicked() {
                        model.menu_command = Some(WorkbenchMenuCommand::NewSshProfile);
                    }
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 26.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.add(egui::Label::new(RichText::new("会话管理器").strong().size(14.0).color(INK)).truncate())
                                .on_hover_text(format!("会话管理器 · {} 个配置", profiles.len()));
                            if width >= 240.0 {
                                ui.label(RichText::new(profiles.len().to_string()).monospace().size(11.0).color(MUTED));
                            }
                        },
                    );
                });
            });
            ui.add_space(8.0);
            ui.spacing_mut().text_edit_width = ui.available_width();
            let search_id=egui::Id::new("saved-session-search");
            let escape=ui.memory(|m|m.has_focus(search_id)) && ui.input_mut(|i|i.consume_key(egui::Modifiers::NONE,egui::Key::Escape));
            let keep_focus=escape && !model.profile_search.is_empty();
            if escape {model.profile_search.clear();}
            let search_response=ui.add(
                egui::TextEdit::singleline(&mut model.profile_search).id(search_id)
                    .hint_text("搜索会话 / 主机 / 用户名")
                    .margin(egui::vec2(7.0, 5.0)).desired_width(f32::INFINITY),
            ).on_hover_text("可组合关键词，例如 prod root；Enter 连接匹配项，Esc 清空或退出搜索。");
            if keep_focus {search_response.request_focus();} else if escape {search_response.surrender_focus();}
            if search_response.lost_focus() && ui.input(|i|i.key_pressed(egui::Key::Enter))
                && !model.ssh_control_busy && !model.layout_paused {
                let candidate=profiles.iter().filter(|p|matches_profile_or_folder(p,&folders,&model.profile_search))
                    .find(|p|Some(p.id)==model.selected_profile)
                    .or_else(||profiles.iter().find(|p|matches_profile_or_folder(p,&folders,&model.profile_search)));
                if let Some(profile)=candidate {
                    model.selected_profile=Some(profile.id);
                    model.menu_command=Some(WorkbenchMenuCommand::OpenSavedProfile(profile.id));
                }else{search_response.request_focus();}
            }
            ui.add_space(5.0);
            ui.separator();
            let search = model.profile_search.trim().to_lowercase();
            egui::ScrollArea::vertical()
                .id_salt("classic-session-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if profiles
                        .iter()
                        .any(|p| p.favorite && matches_profile(p, &search))
                    {
                        sidebar_header(ui, "收藏")
                            .id_salt("favorite-sessions")
                            .default_open(true)
                            .show(ui, |ui| {
                                for profile in profiles
                                    .iter()
                                    .filter(|p| p.favorite && matches_profile(p, &search))
                                {
                                    profile_row(ui, model, profile);
                                }
                            });
                        ui.separator();
                    }
                    sidebar_header(ui, RichText::new("全部会话").strong().color(MUTED))
                        .id_salt("all-saved-sessions")
                        .default_open(true)
                        .open(if search.is_empty() { None } else { Some(true) })
                        .show(ui, |ui| {
                            folder_rows(ui, model, &folders, &profiles, None, &search, 0);
                            if profiles.is_empty() {
                                ui.add_space(16.0);
                                ui.label(RichText::new("建立你的连接工作区").color(INK));
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new("保存服务器和分组，下次双击即可连接。")
                                        .size(12.0)
                                        .color(MUTED),
                                );
                                ui.add_space(10.0);
                                if ui.button("新建 SSH 会话").clicked() {
                                    model.menu_command = Some(WorkbenchMenuCommand::NewSshProfile);
                                }
                                ui.add_space(16.0);
                            } else if !profiles.iter().any(|p| matches_profile(p, &search))
                                && !search.is_empty()
                                && !folders
                                    .iter()
                                    .any(|f| tab_navigation::search_matches(&search, &[&f.name]))
                            {
                                ui.weak("没有匹配的会话");
                            }
                        });
                    if !model.sessions.is_empty() {
                        ui.separator();
                        sidebar_header(ui, "已打开的会话")
                            .default_open(false)
                            .show(ui, |ui| {
                                let sessions = model.sessions.clone();
                                for session in sessions {
                                    let marker = if session.connected { "●" } else { "○" };
                                    ui.horizontal(|ui| {
                                    let label = ui.add_sized(
                                        [(ui.available_width() - 30.0).max(1.0), 26.0],
                                        egui::Button::new(format!("{marker} {}", session.title))
                                            .selected(model.selected == Some(session.id))
                                            .frame(false)
                                            .truncate(),
                                    ).on_hover_text(&session.title);
                                    if label.clicked() {
                                        model.selected = Some(session.id);
                                    }
                                    if ui.small_button("×")
                                        .on_hover_text("移除此会话；运行中的进程将在确认后终止，不删除保存的连接配置")
                                        .clicked()
                                    {
                                        prompt_termination(model, session.id);
                                    }
                                    label.context_menu(|ui| {
                                        crate::form::menu_body(ui, |ui| {
                                            if let Some(tab) = model.tabs.iter().find(|tab| tab.session_id == Some(session.id)).map(|tab| tab.id) {
                                                action(ui, model, "关闭标签", WorkbenchMenuCommand::CloseWorkspaceTab(tab));
                                            }
                                            if ui.button("移除会话…").clicked() {
                                                prompt_termination(model, session.id);
                                                ui.close();
                                            }
                                        });
                                    });
                                    });
                                }
                            });
                    }
                });
        });
    if ui.read_response(resize_id).is_some_and(|response| response.hovered() || response.dragged()) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
}

fn is_ssh_tab(model:&WorkbenchViewModel,tab:TabId)->bool {
    model.ssh_tab_states.contains_key(&tab) || model.tabs.iter().find(|t|t.id==tab).and_then(|t|t.session_id).is_some_and(|id|model.reusable_ssh_sessions.contains(&id))
}
fn duplicate_channel_action(ui:&mut egui::Ui,model:&mut WorkbenchViewModel,tab:TabId) {
    let available=!model.ssh_control_busy && !model.layout_paused && model.tabs.iter().find(|t|t.id==tab).and_then(|t|t.session_id).is_some_and(|id|model.reusable_ssh_sessions.contains(&id));
    if ui.add_enabled(available,egui::Button::new("复制 SSH 渠道")).on_hover_text("复用已认证的连接，打开独立 Shell；无需动态码，连接断开会影响共享渠道").clicked() {
        model.menu_command=Some(WorkbenchMenuCommand::CloneSshChannel(tab)); ui.close();
    }
}

fn tab_context(ui: &mut egui::Ui, model: &mut WorkbenchViewModel, tab: TabId, started: bool) {
    let ssh=is_ssh_tab(model,tab);
    if started {
        action(
            ui,
            model,
            if is_ssh_tab(model,tab) {"复制 SSH 会话"} else {"复制为新会话"},
            WorkbenchMenuCommand::CloneWorkspaceSession(tab),
        );
        if is_ssh_tab(model,tab) {duplicate_channel_action(ui,model,tab);}
    } else {
        action(
            ui,
            model,
            "连接此会话",
            WorkbenchMenuCommand::OpenWorkspaceTab(tab),
        );
    }
    action(
        ui,
        model,
        "关闭标签",
        WorkbenchMenuCommand::CloseWorkspaceTab(tab),
    );
    ui.separator();
    action(
        ui,
        model,
        "向左移动",
        WorkbenchMenuCommand::MoveWorkspaceTab(tab, -1),
    );
    action(
        ui,
        model,
        "向右移动",
        WorkbenchMenuCommand::MoveWorkspaceTab(tab, 1),
    );
    action(
        ui,
        model,
        "向右分屏",
        WorkbenchMenuCommand::SplitWorkspace(SplitAxis::Horizontal),
    );
    action(
        ui,
        model,
        "向下分屏",
        WorkbenchMenuCommand::SplitWorkspace(SplitAxis::Vertical),
    );
    let Some(document) = model.workspace_document.clone() else {
        return;
    };
    let current = model
        .workspace_window_id
        .or_else(|| document.windows.first().map(|w| w.id));
    for (index, window) in document.windows.iter().enumerate() {
        if Some(window.id) != current && model.available_windows.contains(&window.id) {
            action(
                ui,
                model,
                &format!("移到窗口 {}", index + 1),
                WorkbenchMenuCommand::MoveTabToWindow(tab, window.id),
            );
        }
    }
    if let Some(window) = current {
        // Only offer groups in this window; other windows have their own move operation.
        let mut pending: Vec<_> = document
            .windows
            .iter()
            .find(|w| w.id == window)
            .map(|w| vec![w.root])
            .unwrap_or_default();
        let mut ordinal = 0;
        let mut visited = std::collections::BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            if let Some(pane) = document.panes.iter().find(|p| p.id == id) {
                match pane.content {
                    WorkspacePaneContent::Split { first, second, .. } => {
                        pending.push(second);
                        pending.push(first);
                    }
                    WorkspacePaneContent::Tabs { group_id } => {
                        ordinal += 1;
                        if document
                            .tab_groups
                            .iter()
                            .any(|g| g.id == group_id && !g.tabs.contains(&tab))
                        {
                            action(
                                ui,
                                model,
                                &format!("移到分屏 {ordinal}"),
                                WorkbenchMenuCommand::MoveTabToGroup(tab, group_id),
                            );
                        }
                    }
                }
            }
        }
    }
}

fn ssh_state_color(state: SshConnectionState) -> Color32 {
    match state {
        SshConnectionState::Connected => theme::SUCCESS,
        SshConnectionState::Unconfirmed => theme::DANGER,
        SshConnectionState::Disconnected => theme::WARNING,
        _ => ACCENT,
    }
}

fn tab_strip(
    ui: &mut egui::Ui,
    model: &mut WorkbenchViewModel,
    tabs: &[TabId],
    active: Option<TabId>,
    header: egui::Rect,
    id: egui::Id,
) {
    ui.painter().rect_filled(header, 0.0, TAB_BAR);
    let header=tab_navigation::header(ui,model,header,id);
    let reveal_key=id.with("last-visible-tab");
    let previous=ui.ctx().data(|data|data.get_temp::<(Option<TabId>,f32)>(reveal_key));
    let reveal=previous.is_none_or(|(last,width)|last!=active || (width-header.width()).abs()>1.0);
    ui.ctx().data_mut(|data|data.insert_temp(reveal_key,(active,header.width())));
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(header.shrink2(egui::vec2(4.0, 1.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.set_clip_rect(header);
    egui::ScrollArea::horizontal()
        .id_salt(id)
        .animated(false)
        .max_width(header.width().max(0.0))
        .auto_shrink([false, true])
        .show(&mut row, |ui| {
            ui.visuals_mut().override_text_color = Some(MUTED);
            ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
            ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::NONE;
            ui.visuals_mut().widgets.hovered.weak_bg_fill = TAB_ACTIVE;
            ui.visuals_mut().widgets.hovered.bg_stroke = Stroke::NONE;
            ui.visuals_mut().widgets.hovered.fg_stroke = Stroke::new(1.0, INK);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for tab in tabs {
                    let saved = model.tabs.iter().find(|t| t.id == *tab).cloned();
                    let title = saved
                        .as_ref()
                        .map(|t| t.title.clone())
                        .or_else(|| {
                            model
                                .workspace_document
                                .as_ref()
                                .and_then(|d| d.bindings.iter().find(|b| b.tab_id == *tab))
                                .map(|b| b.title.clone())
                        })
                        .unwrap_or_else(|| "终端".into());
                    let started = saved.as_ref().is_some_and(|t| t.session_id.is_some());
                    let selected = active == Some(*tab);
                    let ssh_state = model.ssh_tab_states.get(tab).copied();
                    let state_color = ssh_state.map_or(ACCENT, ssh_state_color);
                    ui.push_id(tab, |ui| {
                        let tab_frame = Frame::NONE
                            .fill(if selected {
                                TAB_ACTIVE
                            } else {
                                Color32::TRANSPARENT
                            })
                            .inner_margin(egui::Margin::symmetric(6, 1))
                            .corner_radius(egui::CornerRadius {
                                nw: 4,
                                ne: 4,
                                sw: 0,
                                se: 0,
                            })
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 3.0;
                                    let (icon_rect, icon_response) = ui.allocate_exact_size(
                                        egui::vec2(16.0, 26.0),
                                        egui::Sense::hover(),
                                    );
                                    paint_icon(
                                        ui.painter(),
                                        icon_rect.center(),
                                        if ssh_state == Some(SshConnectionState::Disconnected) { Icon::Disconnect } else { Icon::Terminal },
                                        if ssh_state.is_some() { state_color } else if selected { ACCENT } else { MUTED },
                                    );
                                     if model.unread_output_tabs.contains(tab) {
                                         ui.painter().circle_filled(icon_rect.right_top() + egui::vec2(-1.0, 5.0), 3.0, theme::DANGER);
                                         icon_response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "有新输出"));
                                         icon_response.on_hover_text("有未查看的新输出，查看此标签后清除");
                                     }
                                    let response = ui
                                        .add_sized(
                                            [124.0, 28.0],
                                            egui::Button::new(
                                                RichText::new(compact(&title, 19))
                                                    .color(if selected { INK } else { MUTED }),
                                            )
                                            .frame(false)
                                            .truncate(),
                                        )
                                        .on_hover_text(ssh_chrome::tooltip(model, *tab, &title, started));
                                    if let Some(state) = ssh_state.filter(|state| *state != SshConnectionState::Connected) {
                                        ui.label(RichText::new(state.label()).size(11.0).strong().color(state_color))
                                            .on_hover_text(format!("SSH {}", state.label()));
                                    }
                                     if let Some(realtime) = model.input_target_tabs.get(tab) {
                                         ui.add(egui::Label::new(RichText::new(if *realtime { "同步" } else { "命令" }).size(11.0).strong()
                                             .color(if *realtime { theme::WARNING } else { ACCENT })).wrap_mode(egui::TextWrapMode::Extend))
                                             .on_hover_text(if *realtime { "接收实时键盘同步" } else { "已选为命令栏接收目标；与当前查看的标签分别标识" });
                                     }
                                    if response.clicked() {
                                        model.selected_tab = Some(*tab);
                                    }
                                    if response.clicked_by(egui::PointerButton::Middle) {
                                        model.menu_command =
                                            Some(WorkbenchMenuCommand::CloseWorkspaceTab(*tab));
                                    }
                                    response.context_menu(|ui| {
                                        ui.set_style(ui.ctx().style_of(egui::Theme::Dark));
                                        crate::form::menu_body(ui, |ui| {
                                            tab_context(ui, model, *tab, started)
                                        });
                                    });
                                    if ui.small_button("×").on_hover_text("关闭标签并终止、移除对应会话").clicked()
                                    {
                                        model.menu_command =
                                            Some(WorkbenchMenuCommand::CloseWorkspaceTab(*tab));
                                    }
                                });
                            });
                        if selected {
                            if reveal {tab_frame.response.scroll_to_me(Some(Align::Center));}
                            let rect = tab_frame.response.rect;
                            ui.painter().line_segment(
                                [
                                    rect.left_top() + egui::vec2(3.0, 0.0),
                                    rect.right_top() - egui::vec2(3.0, 0.0),
                                ],
                                Stroke::new(2.0, state_color),
                            );
                        }
                    });
                }
            });
        });
}

fn pane_body(
    ui: &mut egui::Ui,
    model: &mut WorkbenchViewModel,
    rect: egui::Rect,
    tabs: &[TabId],
    active: Option<TabId>,
    id: egui::Id,
) -> egui::Rect {
    let header = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.max.x, (rect.min.y + 34.0).min(rect.max.y)),
    );
    tab_strip(ui, model, tabs, active, header, id);
    let content = egui::Rect::from_min_max(egui::pos2(rect.min.x, header.max.y), rect.max);
    let content = active.map_or(content, |tab| ssh_chrome::identity(ui, model, content, tab, id));
    let mut top = content.min.y;
    let ssh_notice = active.and_then(|tab| model.ssh_tab_states.get(&tab).copied().map(|state| (tab, state)))
        .filter(|(_, state)| *state != SshConnectionState::Connected);
    if let Some((tab, state)) = ssh_notice {
        let notice = egui::Rect::from_min_max(egui::pos2(rect.min.x, top),
            egui::pos2(rect.max.x, (top + 64.0).min(rect.max.y)));
        let color = ssh_state_color(state);
        let fill = match state {
            SshConnectionState::Disconnected => Color32::from_rgb(48, 40, 28),
            SshConnectionState::Unconfirmed => Color32::from_rgb(48, 30, 34),
            _ => PANEL,
        };
        ui.painter().rect_filled(notice, 0.0, fill);
        ui.painter().rect_filled(egui::Rect::from_min_max(notice.min, egui::pos2((notice.min.x + 4.0).min(notice.max.x), notice.max.y)), 0.0, color);
        let mut row = ui.new_child(egui::UiBuilder::new().id_salt((id, "ssh-status-notice"))
            .max_rect(notice.shrink2(egui::vec2(12.0, 6.0))).layout(Layout::top_down(Align::Min)));
        row.set_clip_rect(notice);
        let (heading, detail, retry) = match state {
            SshConnectionState::Disconnected => ("SSH 已断开", "连接已关闭，以下为终端历史记录，输入不会发送到服务器。", true),
            SshConnectionState::Disconnecting => ("正在断开 SSH 连接…", "正在等待后台确认，请稍候。", false),
            SshConnectionState::Reconnecting => ("正在重新连接 SSH…", "正在使用此标签的连接配置重新建立连接，请稍候。", false),
            SshConnectionState::Connecting => ("正在连接 SSH…", "正在建立连接，请稍候。", false),
            _ => ("SSH 状态待确认", "尚未确认连接状态，请检查错误信息后重试。", true),
        };
        row.horizontal(|ui| {
            if !retry { ui.spinner(); }
            ui.label(RichText::new(heading).strong().color(color));
            if retry {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!model.ssh_control_busy && !model.layout_paused,
                        egui::Button::new(RichText::new("重新连接").color(ACCENT)).fill(theme::ACCENT_FILL)).clicked() {
                        model.menu_command = Some(WorkbenchMenuCommand::ReconnectSshTab(tab));
                    }
                });
            }
        });
        row.add(egui::Label::new(RichText::new(detail).size(12.0).color(MUTED)).truncate()).on_hover_text(detail);
        top = notice.max.y;
    } else {
        if let Some(tab) = active
            .and_then(|id| model.tabs.iter().find(|t| t.id == id))
            .cloned()
            && tab.session_id.is_none()
        {
            let notice = egui::Rect::from_min_max(
                egui::pos2(rect.min.x, top),
                egui::pos2(rect.max.x, (top + 36.0).min(rect.max.y)),
            );
            let mut row = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt((id, "restore-notice"))
                    .max_rect(notice.shrink(4.0))
                    .layout(Layout::left_to_right(Align::Center)),
            );
            row.set_clip_rect(notice);
            row.painter().rect_filled(notice, 0.0, theme::PANEL);
            row.small(if model.ssh_control_busy && model.selected_tab == Some(tab.id) { "正在处理连接…" } else { "会话未连接" });
            if row.add_enabled(!model.ssh_control_busy, egui::Button::new("连接此会话")).clicked() {
                model.menu_command = Some(WorkbenchMenuCommand::OpenWorkspaceTab(tab.id));
            }
            top = notice.max.y;
        }
    }
    egui::Rect::from_min_max(egui::pos2(rect.min.x, top), rect.max)
}
pub(super) fn draw_pane(
    ui: &mut egui::Ui,
    model: &mut WorkbenchViewModel,
    document: &WorkspaceDocument,
    id: PaneId,
    rect: egui::Rect,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let Some(pane) = document.panes.iter().find(|p| p.id == id) else {
        return;
    };
    match pane.content {
        WorkspacePaneContent::Tabs { group_id } => {
            let Some(group) = document.tab_groups.iter().find(|g| g.id == group_id) else {
                return;
            };
            let body = pane_body(
                ui,
                model,
                rect,
                &group.tabs,
                group.active_tab,
                egui::Id::new(group_id),
            );
            // Only paint the inset edge: an opaque egui body would cover the GPU terminal.
            ui.painter().rect_stroke(body, 0.0, Stroke::new(1.0, TAB_BAR), egui::StrokeKind::Inside);
            let focused = group.active_tab.is_some() && group.active_tab == model.selected_tab;
            if focused && document.tab_groups.len() > 1 {
                ui.painter().rect_stroke(rect.shrink(0.5), 0.0, Stroke::new(1.0, ACCENT), egui::StrokeKind::Inside);
            }
            model.pane_views.push(WorkspacePaneView {
                group_id,
                tab_id: group.active_tab,
                rect: body.shrink(1.0_f32.min(body.width().max(0.0) / 2.0).min(body.height().max(0.0) / 2.0)),
            });
        }
        WorkspacePaneContent::Split {
            axis,
            ratio_permille,
            first,
            second,
        } => {
            let horizontal = axis == SplitAxis::Horizontal;
            let size = if horizontal {
                rect.width()
            } else {
                rect.height()
            };
            let origin = if horizontal { rect.min.x } else { rect.min.y };
            let gap = 5.0_f32.min(size.max(0.0));
            let split =
                origin + gap / 2.0 + (size - gap).max(0.0) * f32::from(ratio_permille) / 1000.0;
            let (mut left, mut right, mut divider) = (rect, rect, rect);
            if horizontal {
                left.max.x = split - gap / 2.0;
                right.min.x = split + gap / 2.0;
                divider.min.x = split - gap / 2.0;
                divider.max.x = split + gap / 2.0;
            } else {
                left.max.y = split - gap / 2.0;
                right.min.y = split + gap / 2.0;
                divider.min.y = split - gap / 2.0;
                divider.max.y = split + gap / 2.0;
            }
            ui.painter().rect_filled(divider, 0.0, LINE);
            let response = ui.interact(
                divider,
                egui::Id::new(("workspace-divider", id)),
                egui::Sense::drag(),
            );
            response.clone().on_hover_cursor(if horizontal {
                egui::CursorIcon::ResizeHorizontal
            } else {
                egui::CursorIcon::ResizeVertical
            });
            if response.dragged()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let position = if horizontal { pointer.x } else { pointer.y };
                let ratio = (((position - origin - gap / 2.0) / (size - gap).max(1.0)) * 1000.0)
                    .clamp(100.0, 900.0) as u16;
                model.menu_command = Some(WorkbenchMenuCommand::ResizeSplit(id, ratio));
            }
            draw_pane(ui, model, document, first, left, depth + 1);
            draw_pane(ui, model, document, second, right, depth + 1);
        }
    }
}

#[must_use]
pub fn draw_workbench(ui: &mut egui::Ui, model: &mut WorkbenchViewModel) -> egui::Rect {
    draw_workbench_chrome(ui, model);
    draw_workbench_content(ui, model)
}

#[must_use]
pub fn draw_workbench_content(ui: &mut egui::Ui, model: &mut WorkbenchViewModel) -> egui::Rect {
    draw_sidebar(ui, model);
    model.pane_views.clear();
    let document = model.workspace_document.clone();
    egui::CentralPanel::default()
        .frame(Frame::NONE.fill(Color32::TRANSPARENT))
        .show(ui, |ui| {
            let rect = ui.max_rect();
            if let Some(document) = &document
                && let Some(window) = model
                    .workspace_window_id
                    .and_then(|id| document.windows.iter().find(|w| w.id == id))
                    .or_else(|| document.windows.first())
            {
                draw_pane(ui, model, document, window.root, rect, 0);
                model
                    .pane_views
                    .iter()
                    .find(|p| p.tab_id == model.selected_tab)
                    .map_or(rect, |p| p.rect)
            } else {
                let tabs: Vec<_> = model.tabs.iter().map(|t| t.id).collect();
                pane_body(
                    ui,
                    model,
                    rect,
                    &tabs,
                    model.selected_tab,
                    egui::Id::new("fallback-tabs"),
                )
            }
        })
        .inner
}

fn draw_dialogs(context: &egui::Context, model: &mut WorkbenchViewModel) {
    tab_navigation::draw(context, model);
    if model.confirm_terminate {
        let response = egui::Modal::new(egui::Id::new("cshell-terminate-dialog"))
            .show(context, |ui| {
                ui.set_width((context.content_rect().width() - 48.0).clamp(260.0, 440.0));
                ui.heading("终止会话进程？");
                if let Some(id) = model.terminate_target {
                    let title = model
                        .sessions
                        .iter()
                        .find(|s| s.id == id)
                        .map_or("会话", |s| s.title.as_str());
                    ui.label(title);
                }
                ui.label("将移除此会话并终止仍在运行的进程及其子进程，正在执行的命令无法恢复。保存的连接配置保留。");
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!model.terminate_pending, egui::Button::new("取消"))
                        .clicked()
                    {
                        model.confirm_terminate = false;
                        model.terminate_target = None;
                        model.terminate_error = None;
                    }
                    let label = if model.terminate_pending { "正在终止…" } else { "终止进程" };
                    if ui
                        .add_enabled(
                            !model.terminate_pending && model.terminate_target.is_some(),
                            egui::Button::new(label),
                        )
                        .clicked()
                    {
                        model.terminate_pending = true;
                        model.terminate_error = None;
                        if let Some(id) = model.terminate_target {
                            model.menu_command = Some(WorkbenchMenuCommand::TerminateSession(id));
                        }
                    }
                });
                if model.terminate_pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在等待后台确认，成功后移除会话。");
                    });
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                }
                if let Some(error) = &model.terminate_error {
                    ui.colored_label(theme::DANGER, error);
                }
            });
        if response.should_close() && !model.terminate_pending {
            model.confirm_terminate = false;
            model.terminate_target = None;
            model.terminate_error = None;
        }
    }
    if model.about_open {
        let response = egui::Modal::new(egui::Id::new("cshell-about-dialog")).show(context, |ui| {
            ui.heading("关于 CShell");
            ui.label("SSH 终端与会话管理");
            ui.label(format!("版本 {} · 开发试用版", env!("CARGO_PKG_VERSION")));
            ui.add_space(8.0);
            if ui.button("关闭").clicked() {
                ui.close();
            }
        });
        if response.should_close() {
            model.about_open = false;
        }
    }
}

#[cfg(test)]
#[path = "workbench_mouse_tests.rs"]
mod mouse_tests;
