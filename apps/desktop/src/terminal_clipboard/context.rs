use super::*;

#[derive(Clone)]
pub(crate) struct CapturedSelection {
    snapshot: Arc<cshell_terminal::FrameSnapshot>,
    history: Option<cshell_render::LogSurfaceFrame>,
    selection: TerminalSelection,
}

#[derive(Default)]
pub(super) struct Context {
    pub selected: Option<zeroize::Zeroizing<String>>,
    pub link: Option<String>,
    pub path: Option<String>,
    pub object: Option<String>,
    pub object_selection: Option<CapturedSelection>,
}
pub(super) fn web_link(text: &str) -> Option<String> {
    let mut text = text.trim();
    for (left, right) in [
        ('(', ')'),
        ('[', ']'),
        ('{', '}'),
        ('<', '>'),
        ('"', '"'),
        ('\'', '\''),
    ] {
        if text.starts_with(left) && text.ends_with(right) && text.len() > 1 {
            text = &text[left.len_utf8()..text.len() - right.len_utf8()];
        }
    }
    let text = text.trim_end_matches([',', '.', ';']);
    if text.len() > 4096 || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then(|| text.to_owned())
}
pub(super) fn remote_path(text: &str, cwd: Option<&str>) -> Option<String> {
    let text = text.trim().trim_matches(['\'', '"']);
    if text.is_empty()
        || text.len() > 4096
        || text.chars().any(char::is_control)
        || web_link(text).is_some()
    {
        return None;
    }
    if !text.contains('/') && text != "." && text != ".." && !text.contains('.') {
        return None;
    }
    if text.starts_with('~') || text.contains("://") {
        return None;
    }
    let path = if text.starts_with('/') {
        text.to_owned()
    } else {
        format!(
            "{}/{}",
            cwd.filter(|c| c.starts_with('/'))?.trim_end_matches('/'),
            text
        )
    };
    let mut components = Vec::new();
    for item in path.split('/') {
        match item {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(item),
        }
    }
    Some(format!("/{}", components.join("/")))
}
impl DesktopApp {
    pub(crate) fn terminal_link_hint(&self) -> Option<String> {
        let (text, link) = self.pointer_text()?;
        link.or_else(|| web_link(&text))
    }
    fn pointer_text(&self) -> Option<(String, Option<String>)> {
        let point = self
            .cursor_position
            .and_then(|pos| self.terminal_point_at(pos))?;
        let history;
        let snapshot = if let Some(s) = &self.terminal_clipboard.snapshot {
            s.as_ref()
        } else if let Some(frame) = self
            .log_surface
            .as_ref()
            .and_then(|s| s.prepare_frame(self.viewport_rows))
        {
            history = history_snapshot(&frame, self.viewport_rows);
            &history
        } else {
            self.terminal_surface.latest_snapshot()?.as_ref()
        };
        let row = snapshot.row(point.row)?;
        let link = row
            .get(usize::from(point.column))
            .and_then(|c| c.hyperlink_uri())
            .and_then(web_link);
        let text: String = row
            .iter()
            .filter(|c| {
                !matches!(
                    c.width,
                    cshell_terminal::CellWidth::WideSpacer
                        | cshell_terminal::CellWidth::LeadingWideSpacer
                )
            })
            .flat_map(cshell_terminal::Cell::characters)
            .collect();
        let offset: usize = row[..usize::from(point.column).min(row.len())]
            .iter()
            .filter(|c| {
                !matches!(
                    c.width,
                    cshell_terminal::CellWidth::WideSpacer
                        | cshell_terminal::CellWidth::LeadingWideSpacer
                )
            })
            .map(|c| c.characters().map(char::len_utf8).sum::<usize>())
            .sum();
        let offset = offset.min(text.len());
        let delimiter = |c: char| c.is_whitespace() || "\"'<>".contains(c);
        let start = text[..offset]
            .char_indices()
            .rev()
            .find(|(_, ch)| delimiter(*ch))
            .map_or(0, |(index, ch)| index + ch.len_utf8());
        let end = text[offset..]
            .find(delimiter)
            .map_or(text.len(), |i| offset + i);
        Some((text[start..end].to_owned(), link))
    }
    pub(super) fn capture_terminal_context(&self) -> Context {
        let selected = self
            .terminal_selection_text()
            .and_then(Result::ok)
            .filter(|s| !s.trim().is_empty() && s.len() <= 4096);
        let pointer = self.pointer_text();
        let text = selected
            .as_deref()
            .or_else(|| pointer.as_ref().map(|p| p.0.as_str()))
            .unwrap_or_default();
        let link = web_link(text).or_else(|| pointer.as_ref().and_then(|p| p.1.clone()));
        let path = remote_path(text, None);
        let object = self
            .cursor_position
            .and_then(|p| self.terminal_point_at(p))
            .and_then(|point| {
                let history = self
                    .log_surface
                    .as_ref()
                    .and_then(|s| s.prepare_frame(self.viewport_rows));
                let snapshot = self
                    .terminal_clipboard
                    .snapshot
                    .clone()
                    .or_else(|| {
                        history
                            .as_ref()
                            .map(|f| Arc::new(history_snapshot(f, self.viewport_rows)))
                    })
                    .or_else(|| self.terminal_surface.latest_snapshot().cloned())?;
                let settings = terminal_clipboard_settings::Settings {
                    smart_selection: true,
                    ..terminal_clipboard_settings::get()
                };
                let selection = smart::expand(
                    TerminalSelection {
                        anchor: point,
                        focus: point,
                        mode: TerminalSelectionMode::Word,
                    },
                    &snapshot,
                    &settings,
                );
                let value = selection.text(&snapshot).ok()?;
                let address = value.parse::<std::net::IpAddr>().is_ok()
                    || value.parse::<std::net::SocketAddr>().is_ok();
                (value.len() <= 4096
                    && (value.contains('/')
                        || value.contains('\\')
                        || address
                        || web_link(&value).is_some()))
                .then_some((
                    value,
                    CapturedSelection {
                        snapshot,
                        history: self.terminal_clipboard.history_frame.clone().or(history),
                        selection,
                    },
                ))
            });
        Context {
            selected: selected.map(zeroize::Zeroizing::new),
            link,
            path,
            object: object.as_ref().map(|(text, _)| text.clone()),
            object_selection: object.map(|(_, selection)| selection),
        }
    }
    pub(crate) fn ctrl_click_terminal_link(&mut self) -> bool {
        if !self.modifiers.control_key() || self.modifiers.alt_key() {
            return false;
        }
        let Some((text, hyperlink)) = self.pointer_text() else {
            return false;
        };
        let Some(link) = hyperlink.or_else(|| web_link(&text)) else {
            return false;
        };
        self.terminal_clipboard.pending_link = Some(link);
        true
    }
    pub(crate) fn handle_terminal_context_action(&mut self, action: Action) {
        match action {
            Action::SelectScreen => self.select_terminal_screen(),
            Action::SelectAll => self.select_terminal_all(),
            Action::ExportAll => self.export_terminal_all(),
            Action::SelectObject(captured) => {
                self.clear_clipboard_selection();
                self.terminal_clipboard.owner = self.current_workspace_tab;
                self.terminal_clipboard.snapshot = Some(captured.snapshot);
                self.terminal_clipboard.history_frame = captured.history;
                self.terminal_decorations.selection = Some(captured.selection);
                self.update_clipboard_highlight();
                self.pause_terminal_reading();
            }
            Action::CopyObject(text) => {
                if let Some(state) = &mut self.egui_state {
                    state.set_clipboard_text(text);
                    self.clipboard_notice("已复制完整对象".into());
                }
            }
            Action::Find(text) => {
                self.terminal_search_query = text;
                self.terminal_search_options.regex = false;
                self.terminal_search_options.whole_word = false;
                self.open_terminal_search();
                if self.log_surface.is_some() {
                    self.schedule_history_search(HistorySearchDirection::Backward, None, true);
                } else {
                    self.schedule_terminal_search(true);
                }
            }
            Action::OpenLink(link) => self.terminal_clipboard.pending_link = Some(link),
            Action::CopyLink(link) => {
                if let Some(state) = &mut self.egui_state {
                    state.set_clipboard_text(link);
                    self.clipboard_notice("已复制链接地址".into());
                }
            }
            Action::RemotePath(path, download) => {
                if let Some(profile) = self.sftp_profile_for_open() {
                    self.open_sftp_profile(profile);
                    if let Some(session) = self.sftp_session {
                        self.sftp_panel.queue_terminal_path(session, path, download);
                    }
                } else {
                    self.view_model.input_warning = Some("当前 SSH 会话无法打开 SFTP".into());
                }
            }
            _ => {}
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selecting_a_context_object_uses_the_frame_captured_when_menu_opened() {
        use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};
        let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(2, 20));
        terminal.feed(b"/var/log/access.log");
        let snapshot = Arc::new(terminal.snapshot());
        let selection = TerminalSelection {
            anchor: TerminalCellPoint { row: 0, column: 0 },
            focus: TerminalCellPoint { row: 0, column: 18 },
            mode: TerminalSelectionMode::Character,
        };
        let captured = CapturedSelection {
            snapshot,
            history: None,
            selection,
        };
        terminal.feed(b"\r\nNEW OUTPUT");
        let mut app = DesktopApp::default();
        app.terminal_surface
            .submit_snapshot(Arc::new(terminal.snapshot()));
        app.handle_terminal_context_action(Action::SelectObject(captured));
        assert_eq!(
            app.terminal_selection_text()
                .and_then(Result::ok)
                .as_deref()
                .map(str::trim),
            Some("/var/log/access.log")
        );
    }
    #[test]
    fn links_and_paths_are_explicitly_resolved_without_shell_expansion() {
        assert_eq!(
            web_link("(https://example.org/a?x=1&y=2)"),
            Some("https://example.org/a?x=1&y=2".into())
        );
        assert!(web_link("javascript:alert(1)").is_none());
        assert_eq!(
            web_link("https://example.org/function_(test)"),
            Some("https://example.org/function_(test)".into())
        );
        assert!(web_link("https://example.org/\ncommand").is_none());
        assert_eq!(
            remote_path("'../日志/a b.log'", Some("/srv/app")),
            Some("/srv/日志/a b.log".into())
        );
        assert!(remote_path("./log", None).is_none());
        assert!(remote_path("hello", Some("/home/me")).is_none());
        assert_eq!(remote_path("/var/log", None), Some("/var/log".into()));
    }
    #[test]
    fn context_find_is_literal_and_link_open_is_delivered_in_frame_output() {
        let mut app = DesktopApp::default();
        app.terminal_search_options.regex = true;
        app.handle_terminal_context_action(Action::Find("a.*[x]".into()));
        assert!(app.terminal_search_open && !app.terminal_search_options.regex);
        assert_eq!(app.terminal_search_query, "a.*[x]");
        app.handle_terminal_context_action(Action::OpenLink("https://example.org/test".into()));
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.draw_clipboard_ui(ui.ctx());
        });
        assert!(output.platform_output.commands.iter().any(
            |c| matches!(c,egui::OutputCommand::OpenUrl(url) if url.url=="https://example.org/test")
        ));
        output.textures_delta.clear();
        assert!(app.terminal_clipboard.pending_link.is_none());
    }
}
