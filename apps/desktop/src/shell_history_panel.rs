//! Commands are reported by the shell, never inferred from echoed keystrokes.
use crate::terminal_tools_connection::{Connection, View};
use cshell_domain::SessionId;
use cshell_ipc::{TerminalToolsOperation as Op, TerminalToolsRequest as Request};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Panel {
    session: Option<SessionId>,
    epoch: u64,
    selected: Option<u64>,
    last_poll: Option<Instant>,
    pending_copy: Option<(SessionId, u64, u64)>,
    pub jump: Option<u64>,
    pub load_command: Option<String>,
    query: String,
    failed_only: bool,
    bookmarks_only: bool,
    bookmarks: std::collections::BTreeSet<u64>,
    saved_bookmarks: std::collections::BTreeMap<SessionId, std::collections::BTreeSet<u64>>,
}

#[cfg(test)]
pub(super) fn preview(session: SessionId) -> (Panel, View) {
    let command = cshell_domain::ShellCommand {
        id: 1,
        line: 1,
        output_line: 2,
        output_column: 0,
        end_line: Some(5),
        end_column: 0,
        command: "systemctl status nginx".into(),
        command_complete: true,
        exit_status: Some(3),
        elapsed_ms: Some(240),
        running: false,
    };
    let data=cshell_domain::ShellHistory{session,supported:true,first_line:1,cwd:Some("/etc/nginx".into()),commands:vec![command],output_id:1,output:Some("Offline preview / 离线示例\nnginx.service - HTTP server\nActive: failed (Result: exit-code)\n".into()),truncated:false};
    (
        Panel {
            session: Some(session),
            selected: Some(1),
            ..Default::default()
        },
        View {
            shell: Some(data),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_layout_gate::{bounds, check_window, context, frame};
    #[test]
    fn search_failure_and_bookmark_filters_and_tab_switch_keep_scope() {
        let ctx = context();
        let session = SessionId::new();
        let (mut panel, mut view) = preview(session);
        let mut second = view.shell.as_ref().unwrap().commands[0].clone();
        second.id = 2;
        second.command = "echo healthy".into();
        second.exit_status = Some(0);
        view.shell.as_mut().unwrap().commands.push(second);
        panel.query = "HEALTHY".into();
        let _ = frame(&ctx, egui::vec2(900.0, 600.0), |c| {
            cshell_ui::form::dialog(c, "filter test")
                .show(c, |ui| panel.draw(ui, Some(session), &view, None));
        });
        assert_eq!(panel.selected, Some(2));
        panel.query.clear();
        panel.failed_only = true;
        let _ = frame(&ctx, egui::vec2(900.0, 600.0), |c| {
            cshell_ui::form::dialog(c, "filter test")
                .show(c, |ui| panel.draw(ui, Some(session), &view, None));
        });
        assert_eq!(panel.selected, Some(1));
        panel.failed_only = false;
        panel.bookmarks.insert(2);
        panel.bookmarks_only = true;
        let _ = frame(&ctx, egui::vec2(900.0, 600.0), |c| {
            cshell_ui::form::dialog(c, "filter test")
                .show(c, |ui| panel.draw(ui, Some(session), &view, None));
        });
        assert_eq!(panel.selected, Some(2));
        let other = SessionId::new();
        let (_, other_view) = preview(other);
        let _ = frame(&ctx, egui::vec2(900.0, 600.0), |c| {
            cshell_ui::form::dialog(c, "filter test")
                .show(c, |ui| panel.draw(ui, Some(other), &other_view, None));
        });
        assert!(panel.bookmarks.is_empty());
        let _ = frame(&ctx, egui::vec2(900.0, 600.0), |c| {
            cshell_ui::form::dialog(c, "filter test")
                .show(c, |ui| panel.draw(ui, Some(session), &view, None));
        });
        assert!(panel.bookmarks.contains(&2));
    }
    #[test]
    fn incomplete_command_text_cannot_be_copied_as_a_complete_command() {
        let ctx = context();
        let session = SessionId::new();
        let (mut panel, mut view) = preview(session);
        view.shell.as_mut().unwrap().commands[0].command_complete = false;
        let tree = frame(&ctx, egui::vec2(720.0, 480.0), |c| {
            cshell_ui::form::dialog(c, "命令文本检查")
                .show(c, |ui| panel.draw(ui, Some(session), &view, None));
        });
        assert!(
            tree.nodes
                .iter()
                .find(|(_, n)| n.role() == egui::accesskit::Role::Button
                    && n.label() == Some("复制命令"))
                .unwrap()
                .1
                .is_disabled()
        );
    }
    #[test]
    fn direct_output_copy_accepts_only_the_requested_session_command_and_receipt() {
        let ctx = context();
        let session = SessionId::new();
        let (mut panel, mut view) = preview(session);
        view.shell_epoch = 7;
        panel.pending_copy = Some((session, 1, 7));
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 480.0),
                )),
                ..Default::default()
            },
            |ui| panel.draw(ui, Some(session), &view, None),
        );
        assert!(output.platform_output.commands.iter().any(
            |c| matches!(c,egui::OutputCommand::CopyText(text) if text.contains("nginx.service"))
        ));
        output.textures_delta.clear();
        panel.pending_copy = Some((SessionId::new(), 1, 7));
        let mut output = ctx.run_ui(Default::default(), |ui| {
            panel.draw(ui, Some(session), &view, None)
        });
        assert!(
            !output
                .platform_output
                .commands
                .iter()
                .any(|c| matches!(c, egui::OutputCommand::CopyText(_)))
        );
        output.textures_delta.clear();
    }
    #[test]
    fn command_history_actions_fit_and_switching_session_discards_selection() {
        for size in [egui::vec2(720.0, 480.0), egui::vec2(1280.0, 800.0)] {
            let ctx = context();
            let session = SessionId::new();
            let (mut panel, view) = preview(session);
            let mut render = |c: &egui::Context| {
                cshell_ui::form::dialog(c, "命令历史检查")
                    .show(c, |ui| panel.draw(ui, Some(session), &view, None));
            };
            for _ in 0..3 {
                frame(&ctx, size, &mut render);
            }
            let tree = frame(&ctx, size, &mut render);
            check_window(&ctx, size, "命令历史检查", &tree);
            for label in ["定位到终端", "复制命令", "复制输出"] {
                let node = tree
                    .nodes
                    .iter()
                    .find(|(_, n)| {
                        n.role() == egui::accesskit::Role::Button && n.label() == Some(label)
                    })
                    .unwrap();
                let rect = bounds(&node.1);
                assert!(
                    rect.height() > 10.0 && rect.bottom() < size.y,
                    "{label}: {rect:?}"
                );
            }
            let other = SessionId::new();
            frame(&ctx, size, |c| {
                cshell_ui::form::dialog(c, "命令历史检查")
                    .show(c, |ui| panel.draw(ui, Some(other), &view, None));
            });
            assert!(panel.selected.is_none());
            assert!(panel.jump.is_none());
        }
    }
}
impl Panel {
    fn request(&mut self, connection: Option<&Connection>, session: SessionId, id: u64) -> bool {
        let epoch = self.epoch.wrapping_add(1);
        if connection.is_some_and(|c| {
            c.request(
                Request {
                    operation: Op::ShellHistory as i32,
                    session_id: session.as_uuid().as_bytes().to_vec(),
                    cursor: id,
                    ..Default::default()
                },
                epoch,
                false,
            )
        }) {
            self.epoch = epoch;
            self.last_poll = Some(Instant::now());
            return true;
        }
        false
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        session: Option<SessionId>,
        view: &View,
        connection: Option<&Connection>,
    ) {
        if self.session != session {
            if let Some(previous) = self.session {
                self.saved_bookmarks
                    .insert(previous, std::mem::take(&mut self.bookmarks));
            }
            if self.saved_bookmarks.len() > 32
                && let Some(key) = self.saved_bookmarks.keys().next().copied()
            {
                self.saved_bookmarks.remove(&key);
            }
            self.bookmarks = session
                .and_then(|id| self.saved_bookmarks.remove(&id))
                .unwrap_or_default();
            self.session = session;
            self.selected = None;
            self.last_poll = None;
            self.jump = None;
            self.pending_copy = None;
            self.load_command = None;
        }
        let Some(session) = session else {
            ui.label("请先打开一个终端会话。");
            return;
        };
        let data = view.shell.as_ref().filter(|d| d.session == session);
        if let Some((scope, id, epoch)) = self.pending_copy {
            if scope != session || self.selected != Some(id) || (!view.busy && view.error.is_some())
            {
                self.pending_copy = None;
            } else if view.shell_epoch == epoch
                && let Some(text) = data
                    .filter(|d| d.output_id == id)
                    .and_then(|d| d.output.as_ref())
            {
                ui.ctx().copy_text(text.clone());
                self.pending_copy = None;
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.strong("当前会话的命令");
            ui.weak("最近 128 条 · 仅本次连接 · 调整列数后重新记录");
            if ui
                .add_enabled(!view.busy, egui::Button::new("刷新"))
                .clicked()
            {
                self.last_poll = None;
            }
        });
        if let Some(data) = data {
            self.bookmarks
                .retain(|id| data.commands.iter().any(|c| c.id == *id));
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("搜索命令")
                        .char_limit(512)
                        .desired_width(180.0),
                );
                ui.checkbox(&mut self.failed_only, "仅失败");
                ui.checkbox(&mut self.bookmarks_only, "仅书签");
            });
            if self.selected.is_none() {
                self.selected = data.commands.last().map(|c| c.id);
            }
            if let Some(cwd) = &data.cwd {
                ui.add(egui::Label::new(format!("目录：{cwd}")).truncate())
                    .on_hover_text(cwd);
            }
            if !data.supported {
                ui.label("此 shell 尚未报告命令边界。复制下面的初始化脚本，在目标 bash / zsh 中粘贴执行即可启用；仅作用于当前 shell，不修改启动文件。");
                ui.horizontal(|ui| {
                    if ui.button("复制 bash 初始化脚本").clicked() {
                        ui.ctx()
                            .copy_text(include_str!("../assets/shell-integration/bash.sh").into());
                    }
                    if ui.button("复制 zsh 初始化脚本").clicked() {
                        ui.ctx()
                            .copy_text(include_str!("../assets/shell-integration/zsh.sh").into());
                    }
                });
            }
            let query = self.query.to_lowercase();
            let commands = data
                .commands
                .iter()
                .filter(|c| {
                    (!self.failed_only || c.exit_status.is_some_and(|s| s != 0))
                        && (!self.bookmarks_only || self.bookmarks.contains(&c.id))
                        && c.command.to_lowercase().contains(&query)
                })
                .collect::<Vec<_>>();
            let ids: Vec<_> = commands.iter().map(|c| c.id).collect();
            if self.selected.is_some_and(|id| !ids.contains(&id)) {
                self.selected = ids.last().copied();
            }
            ui.horizontal_wrapped(|ui| {
                for (forward, label) in [(false, "上一条"), (true, "下一条")] {
                    let index = self
                        .selected
                        .and_then(|id| ids.iter().position(|v| *v == id));
                    let target = if forward {
                        index.and_then(|i| ids.get(i + 1)).copied()
                    } else {
                        index
                            .and_then(|i| i.checked_sub(1))
                            .and_then(|i| ids.get(i))
                            .copied()
                            .or_else(|| index.is_none().then(|| ids.last().copied()).flatten())
                    };
                    if ui
                        .add_enabled(target.is_some(), egui::Button::new(label))
                        .clicked()
                    {
                        self.selected = target;
                    }
                }
                if let Some(c) = data.commands.iter().find(|c| Some(c.id) == self.selected) {
                    if ui
                        .add_enabled(c.line >= data.first_line, egui::Button::new("定位到终端"))
                        .on_disabled_hover_text(
                            "命令起始行已超出终端历史保留范围；仍可复制命令或查看保留的输出。",
                        )
                        .clicked()
                    {
                        self.jump = Some(c.line);
                    }
                    if ui
                        .button(if self.bookmarks.contains(&c.id) {
                            "取消书签"
                        } else {
                            "添加书签"
                        })
                        .clicked()
                    {
                        if !self.bookmarks.remove(&c.id) {
                            self.bookmarks.insert(c.id);
                        }
                    }
                    if ui
                        .add_enabled(c.command_complete, egui::Button::new("载入命令栏"))
                        .on_hover_text("载入可编辑草稿；保留已有草稿，不直接发送")
                        .clicked()
                    {
                        self.load_command = Some(c.command.clone());
                    }
                    if ui
                        .add_enabled(c.command_complete, egui::Button::new("复制命令"))
                        .on_disabled_hover_text(
                            "命令文本已截断或起始行已淘汰，无法完整复制；请在终端中核对。",
                        )
                        .clicked()
                    {
                        ui.ctx().copy_text(c.command.clone());
                    }
                    if ui
                        .add_enabled(!view.busy, egui::Button::new("查看输出"))
                        .clicked()
                    {
                        self.request(connection, session, c.id);
                    }
                    if ui
                        .add_enabled(!view.busy, egui::Button::new("复制输出"))
                        .on_hover_text("按需读取所选命令的保留输出并直接复制；截断会明确提示。")
                        .clicked()
                    {
                        if data.output_id == c.id
                            && !c.running
                            && let Some(text) = &data.output
                        {
                            ui.ctx().copy_text(text.clone());
                        } else if self.request(connection, session, c.id) {
                            self.pending_copy = Some((session, c.id, self.epoch));
                        }
                    }
                }
            });
            egui::ScrollArea::vertical()
                .id_salt("shell-command-list")
                .max_height((ui.ctx().content_rect().height() * 0.25).clamp(70.0, 220.0))
                .show(ui, |ui| {
                    for c in commands.iter().rev() {
                        let state = if c.running {
                            "执行中".into()
                        } else {
                            c.exit_status
                                .map_or("状态未知".into(), |v| format!("退出 {v}"))
                        };
                        let elapsed = c
                            .elapsed_ms
                            .map_or(String::new(), |v| format!(" · {:.2}s", v as f64 / 1000.0));
                        if ui
                            .add_sized(
                                [ui.available_width(), 28.0],
                                egui::Button::new(format!("{}    {state}{elapsed}", c.command))
                                    .selected(Some(c.id) == self.selected)
                                    .truncate(),
                            )
                            .on_hover_text(&c.command)
                            .clicked()
                        {
                            self.selected = Some(c.id);
                        }
                    }
                    if data.supported && data.commands.is_empty() {
                        ui.weak("等待下一条命令；不补造启用前或已淘汰的历史。");
                    }
                });
            if data.output_id != 0 && Some(data.output_id) == self.selected {
                if let Some(output) = &data.output {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.strong("命令输出");
                        if ui.button("复制输出").clicked() {
                            ui.ctx().copy_text(output.clone());
                        }
                    });
                    if data.truncated {
                        ui.colored_label(
                            cshell_ui::theme::WARNING,
                            "只展示保留范围内的前 64 KiB / 513 行；内容不完整。",
                        );
                    }
                    egui::ScrollArea::both()
                        .id_salt("shell-command-output")
                        .max_height((ui.ctx().content_rect().height() * 0.25).clamp(70.0, 240.0))
                        .show(ui, |ui| {
                            ui.monospace(output);
                        });
                }
            }
        }
        if !view.busy
            && self.last_poll.is_none_or(|t| {
                t.elapsed() > Duration::from_secs(2)
                    && view.shell.as_ref().is_none_or(|d| d.output_id == 0)
            })
        {
            self.request(connection, session, 0);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}
