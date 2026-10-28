use super::*;
use cshell_domain::{SessionId, ShellCommand, ShellHistory};
use std::time::Instant;

#[derive(Debug, Default)]
pub(super) struct Tracker {
    commands: Vec<ShellCommand>,
    next: u64,
    prompt: Option<(u64, u16)>,
    input: Option<(u64, u16)>,
    segments: Vec<((u64, u16), (u64, u16))>,
    incomplete: bool,
    started: Option<Instant>,
    cwd: Option<String>,
    supported: bool,
}
impl Tracker {
    pub(super) fn next_id(&self) -> u64 {
        self.next
    }
    pub(super) fn restore_next(&mut self, id: u64) {
        self.next = id;
    }
    pub(super) fn invalidate_positions(&mut self) {
        self.commands.clear();
        self.prompt = None;
        self.input = None;
        self.segments.clear();
        self.incomplete = false;
        self.started = None;
    }
}
impl AlacrittyTerminalEngine {
    pub(super) fn advance_output(&mut self, bytes: &[u8]) {
        let mut consumed = 0;
        for (offset, response) in self.xterm_keyboard.observe(bytes) {
            self.parser
                .advance(&mut self.terminal, &bytes[consumed..=offset]);
            self.response_sink.push(response);
            consumed = offset + 1;
        }
        self.parser.advance(&mut self.terminal, &bytes[consumed..]);
    }
    fn shell_position(&self) -> (u64, u16) {
        let content = self.terminal.renderable_content();
        (
            self.terminal.grid().history_offset()
                + self.terminal.grid().history_size() as u64
                + content.cursor.point.line.0.max(0) as u64
                + 1,
            content.cursor.point.column.0 as u16,
        )
    }
    pub(super) fn observe_shell(&mut self, event: &[u8]) {
        if self.terminal.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let Ok(value) = std::str::from_utf8(event) else {
            return;
        };
        if let Some(uri) = value.strip_prefix("7;file://") {
            if let Some((_, path)) = uri.split_once('/') {
                if let Some(path) = decode_path(&format!("/{path}")) {
                    self.shell.cwd = Some(path);
                }
            }
            return;
        }
        let pos = self.shell_position();
        match value {
            _ if matches!(value.split(';').nth(1), Some("P" | "A"))
                && value.split(';').any(|p| p == "k=s") =>
            {
                if let Some(start) = self.shell.input.take() {
                    if self.shell.segments.len() < 64 {
                        self.shell.segments.push((start, pos));
                    } else {
                        self.shell.incomplete = true;
                    }
                }
            }
            _ if value == "133;A" || value.starts_with("133;A;") => {
                self.shell.supported = true;
                self.shell.prompt = Some(pos);
                self.shell.input = None;
                self.shell.segments.clear();
                self.shell.incomplete = false;
                if let Some(last) = self.shell.commands.last_mut().filter(|c| c.running) {
                    last.running = false;
                    last.end_line = Some(pos.0);
                    last.end_column = pos.1;
                    last.elapsed_ms = self
                        .shell
                        .started
                        .take()
                        .map(|s| s.elapsed().as_millis() as u64);
                }
            }
            "133;B" if self.shell.prompt.is_some() => self.shell.input = Some(pos),
            "133;C" => {
                let Some(start) = self.shell.input.take() else {
                    return;
                };
                let mut command = String::new();
                let mut complete = !self.shell.incomplete;
                for (a, b) in self
                    .shell
                    .segments
                    .iter()
                    .copied()
                    .chain(std::iter::once((start, pos)))
                {
                    let (text, truncated) =
                        self.shell_text(a, b, 4096usize.saturating_sub(command.len()));
                    command.push_str(&text);
                    complete &= !truncated;
                }
                self.shell.segments.clear();
                self.shell.next = self.shell.next.saturating_add(1);
                self.shell.commands.push(ShellCommand {
                    id: self.shell.next,
                    line: self.shell.prompt.take().unwrap_or(start).0,
                    output_line: pos.0,
                    output_column: pos.1,
                    command: command.trim_end().to_owned(),
                    command_complete: complete,
                    running: true,
                    ..Default::default()
                });
                while self.shell.commands.len() > 128
                    || self
                        .shell
                        .commands
                        .iter()
                        .map(|c| c.command.len())
                        .sum::<usize>()
                        > 128 * 1024
                {
                    self.shell.commands.remove(0);
                }
                self.shell.started = Some(Instant::now());
            }
            _ if value == "133;D" || value.starts_with("133;D;") => {
                if let Some(last) = self.shell.commands.last_mut().filter(|c| c.running) {
                    last.running = false;
                    last.end_line = Some(pos.0);
                    last.end_column = pos.1;
                    last.exit_status = value
                        .strip_prefix("133;D;")
                        .and_then(|v| v.split(';').next()?.parse().ok());
                    last.elapsed_ms = self
                        .shell
                        .started
                        .take()
                        .map(|s| s.elapsed().as_millis() as u64);
                }
            }
            _ => {}
        }
    }
    fn shell_text(&self, start: (u64, u16), end: (u64, u16), limit: usize) -> (String, bool) {
        use alacritty_terminal::index::{Column, Line, Point};
        let grid = self.terminal.grid();
        let first = grid.history_offset() + 1;
        let last = first + grid.history_size() as u64 + u64::from(self.size.rows) - 1;
        let mut truncated = start.0 < first || end.0 > last || end.0 < start.0;
        let mut text = String::new();
        for id in start.0.max(first)..=end.0.min(last).min(start.0.max(first) + 512) {
            if id == end.0 && end.1 == 0 {
                break;
            }
            let row = Line((id - first) as i32 - grid.history_size() as i32);
            let a = if id == start.0 {
                usize::from(start.1)
            } else {
                0
            };
            let b = if id == end.0 {
                usize::from(end.1)
            } else {
                usize::from(self.size.cols)
            };
            let mut line = String::new();
            for col in a..b.min(usize::from(self.size.cols)) {
                let cell = &grid[Point::new(row, Column(col))];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                if !cell.c.is_control() {
                    line.push(cell.c);
                }
                for ch in cell.zerowidth().into_iter().flatten() {
                    if !ch.is_control() {
                        line.push(*ch);
                    }
                }
            }
            let wrapped = grid[Point::new(row, Column(usize::from(self.size.cols) - 1))]
                .flags
                .contains(Flags::WRAPLINE);
            let piece = if wrapped {
                line.as_str()
            } else {
                line.trim_end_matches(' ')
            };
            for ch in piece.chars() {
                if text.len() + ch.len_utf8() > limit {
                    return (text, true);
                }
                text.push(ch);
            }
            if !wrapped && id < end.0 {
                if text.len() == limit {
                    return (text, true);
                }
                text.push('\n');
            }
        }
        truncated |= end.0 > start.0.max(first) + 512;
        (text, truncated)
    }
    pub fn shell_history(&self, session: SessionId, output_id: u64) -> ShellHistory {
        let first = self.terminal.grid().history_offset() + 1;
        let commands = self.shell.commands.iter().cloned().collect();
        let mut result = ShellHistory {
            session,
            supported: self.shell.supported,
            first_line: first,
            cwd: self.shell.cwd.clone(),
            commands,
            output_id,
            ..Default::default()
        };
        if let Some(c) = result.commands.iter().find(|c| c.id == output_id) {
            let end = c
                .end_line
                .map(|l| (l, c.end_column))
                .unwrap_or_else(|| self.shell_position());
            let (text, truncated) = self.shell_text((c.output_line, c.output_column), end, 65536);
            result.output = Some(text);
            result.truncated = truncated;
        }
        result
    }
}
fn decode_path(value: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut it = value.bytes();
    while let Some(b) = it.next() {
        bytes.push(if b == b'%' {
            u8::from_str_radix(std::str::from_utf8(&[it.next()?, it.next()?]).ok()?, 16).ok()?
        } else {
            b
        });
    }
    let value = String::from_utf8(bytes).ok()?;
    (value.len() <= 4096 && value.starts_with('/') && !value.chars().any(char::is_control))
        .then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_prompts_are_excluded_from_multiline_and_heredoc_commands() {
        let mut t = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        let bytes=b"\x1b]133;A;cl=m\x07$ \x1b]133;B\x07cat <<'END'\r\n\x1b]133;P;k=s\x07quote> \x1b]133;B\x07hello\r\n\x1b]133;P;k=s\x07quote> \x1b]133;B\x07END\r\n\x1b]133;C\x07hello\r\n\x1b]133;D;0;aid=1\x07";
        for part in bytes.chunks(2) {
            t.feed(part);
        }
        let history = t.shell_history(SessionId::new(), 1);
        let command = &history.commands[0];
        assert_eq!(command.command, "cat <<'END'\nhello\nEND");
        assert!(command.command_complete);
        assert_eq!(command.exit_status, Some(0));
        assert_eq!(history.output.as_deref(), Some("hello\n"));
    }
    #[test]
    fn missing_input_history_is_marked_incomplete_instead_of_copying_a_fragment() {
        let mut t = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        for _ in 0..600 {
            t.feed(b"word\r\n");
        }
        t.feed(b"\x1b]133;C\x07");
        assert!(!t.shell_history(SessionId::new(), 0).commands[0].command_complete);
    }
    #[test]
    fn fragmented_boundaries_capture_command_output_status_and_directory() {
        let mut t = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        for chunk in b"\x1b]7;file://host/home/a%20b\x07\x1b]133;A\x07$ \x1b]133;B\x07echo test\r\n\x1b]133;C\x07test\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ".chunks(3){t.feed(chunk);}
        let h = t.shell_history(SessionId::new(), 1);
        assert_eq!(h.cwd.as_deref(), Some("/home/a b"));
        assert_eq!(h.commands[0].command, "echo test");
        assert_eq!(h.commands[0].exit_status, Some(0));
        assert_eq!(h.output.as_deref(), Some("test\n"));
        assert!(!h.commands[0].running);
    }
    #[test]
    fn unmarked_and_alternate_screen_output_never_invents_commands() {
        let mut t = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        t.feed(b"$ true\r\n\x1b[?1049h\x1b]133;A\x07\x1b]133;B\x07x\x1b]133;C\x07\x1b[?1049l");
        assert!(t.shell_history(SessionId::new(), 0).commands.is_empty());
    }
    #[test]
    fn bounded_history_and_resize_never_expose_stale_command_positions() {
        let mut t = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        for _ in 0..150 {
            t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07\x1b]133;D;0\x07");
        }
        let id = t
            .shell_history(SessionId::new(), 0)
            .commands
            .last()
            .unwrap()
            .id;
        assert_eq!(t.shell_history(SessionId::new(), 0).commands.len(), 128);
        t.resize(TerminalSize::cells(24, 100));
        assert!(t.shell_history(SessionId::new(), 0).commands.is_empty());
        t.reset_connection(TerminalSize::cells(24, 80));
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07\x1b]133;D;1\x07");
        assert!(t.shell_history(SessionId::new(), 0).commands[0].id > id);
    }
    #[test]
    fn output_copy_reports_eviction_and_limits_instead_of_claiming_completeness() {
        let mut t = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        t.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07long\r\n\x1b]133;C\x07");
        for _ in 0..600 {
            t.feed(b"line\r\n");
        }
        t.feed(b"\x1b]133;D;0\x07");
        let h = t.shell_history(SessionId::new(), 1);
        assert!(h.truncated);
        assert!(h.output.unwrap().len() <= 65536);
        assert!(decode_path("/%00secret").is_none());
        assert!(decode_path("/%GG").is_none());
    }
}
