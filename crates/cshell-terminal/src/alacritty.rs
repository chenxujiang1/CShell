#[path = "shell_history.rs"]
mod shell_history;
use crate::{
    Cell, CellWidth, Color, CursorAppearance, CursorShape, FrameDelta, FrameSnapshot, Style,
    TerminalEngine, TerminalModes,
};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    self, Color as AlacrittyColor, CursorShape as AlacrittyCursorShape, NamedColor,
};
use cshell_domain::TerminalSize;
use std::collections::HashMap;
pub const DEFAULT_HISTORY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TERMINAL_COLUMNS: u16 = 1024;
pub const MAX_TERMINAL_ROWS: u16 = 512;
pub const MAX_TERMINAL_CELLS: usize = 65536;
fn bounded_size(mut size: TerminalSize) -> TerminalSize {
    size.cols = size.cols.clamp(1, MAX_TERMINAL_COLUMNS);
    size.rows = size.rows.clamp(1, MAX_TERMINAL_ROWS).min((MAX_TERMINAL_CELLS / usize::from(size.cols)) as u16);
    size
}
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
struct TerminalDimensions {
    rows: usize,
    cols: usize,
}

impl From<TerminalSize> for TerminalDimensions {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: usize::from(size.rows.max(1)),
            cols: usize::from(size.cols.max(1)),
        }
    }
}

impl Dimensions for TerminalDimensions {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone, Debug, Default)]
struct ResponseSink {
    responses: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl ResponseSink {
    fn drain(&self) -> Vec<Vec<u8>> {
        let mut responses = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *responses)
    }

    fn push(&self, response: Vec<u8>) {
        self.responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(response);
    }
}

#[derive(Debug, Default)]
struct XtermKeyboardModes {
    modify_other_keys: u8,
    format_other_keys: bool,
    pending: Vec<u8>,
}

impl XtermKeyboardModes {
    fn observe(&mut self, bytes: &[u8]) -> Vec<(usize, Vec<u8>)> {
        let mut responses = Vec::new();
        for (offset, &byte) in bytes.iter().enumerate() {
            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.pending.push(byte);
                }
                continue;
            }
            if self.pending == [0x1b] {
                if byte == b'[' {
                    self.pending.push(byte);
                } else {
                    self.pending.clear();
                    if byte == 0x1b {
                        self.pending.push(byte);
                    }
                }
                continue;
            }
            if self.pending.len() >= 32 || !(0x20..=0x7e).contains(&byte) {
                self.pending.clear();
                continue;
            }
            self.pending.push(byte);
            if (0x40..=0x7e).contains(&byte) {
                if let Some(response) = self.finish_sequence() {
                    responses.push((offset, response));
                }
                self.pending.clear();
            }
        }
        responses
    }

    fn finish_sequence(&mut self) -> Option<Vec<u8>> {
        let Ok(sequence) = std::str::from_utf8(&self.pending[2..]) else {
            return None;
        };
        match sequence {
            ">4m" => self.modify_other_keys = 0,
            ">4n" => self.modify_other_keys = 0,
            ">4f" => self.format_other_keys = false,
            "?4m" => {
                return Some(format!("\x1b[>4;{}m", self.modify_other_keys).into_bytes());
            }
            "?4g" => {
                return Some(format!("\x1b[>4;{}f", u8::from(self.format_other_keys)).into_bytes());
            }
            _ => {
                if let Some(value) = sequence
                    .strip_prefix(">4;")
                    .and_then(|value| value.strip_suffix('m'))
                    .and_then(|value| value.parse::<u8>().ok())
                    .filter(|value| *value <= 3)
                {
                    self.modify_other_keys = value;
                } else if let Some(value) = sequence
                    .strip_prefix(">4;")
                    .and_then(|value| value.strip_suffix('f'))
                    .and_then(|value| value.parse::<u8>().ok())
                {
                    self.format_other_keys = value != 0;
                }
            }
        }
        None
    }
}

impl EventListener for ResponseSink {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(text.into_bytes());
        }
    }
}

pub struct AlacrittyTerminalEngine {
    terminal: Term<ResponseSink>,
    parser: ansi::Processor,
    response_sink: ResponseSink,
    xterm_keyboard: XtermKeyboardModes,
    size: TerminalSize,
    generation: u64,
    output_guard: crate::output_guard::OutputGuard,
    shell: shell_history::Tracker,
    history_budget: usize,
    history_bytes: usize,
}

impl std::fmt::Debug for AlacrittyTerminalEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlacrittyTerminalEngine")
            .field("size", &self.size)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

pub struct HistorySlice {
    pub total: u64,
    pub first_line: u64,
    pub anchor: u64,
    pub generation: u64,
    rows: Vec<(u64, Vec<alacritty_terminal::term::cell::Cell>)>,
    colors: Colors,
}
impl std::fmt::Debug for HistorySlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistorySlice").field("rows", &self.rows.len()).field("anchor", &self.anchor).finish_non_exhaustive()
    }
}
impl HistorySlice {
    pub fn len(&self) -> usize { self.rows.len() }
    pub fn is_empty(&self) -> bool { self.rows.is_empty() }
    pub fn into_rows(self) -> impl Iterator<Item=(u64, Vec<Cell>)> {
        self.rows.into_iter().map(move |(id, cells)| {
            let cells = cells.into_iter().map(|source| {
                let width = if source.flags.contains(Flags::WIDE_CHAR) { CellWidth::Wide }
                    else if source.flags.contains(Flags::WIDE_CHAR_SPACER) { CellWidth::WideSpacer }
                    else if source.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) { CellWidth::LeadingWideSpacer }
                    else { CellWidth::Single };
                Cell::with_zerowidth(if source.c.is_control() {' '} else {source.c}, source.zerowidth().into_iter().flatten().copied(), width, Style {
                    foreground: map_color(source.fg, &self.colors), background: map_color(source.bg, &self.colors),
                    bold: source.flags.contains(Flags::BOLD), italic: source.flags.contains(Flags::ITALIC),
                    underline: source.flags.intersects(Flags::ALL_UNDERLINES), inverse: source.flags.contains(Flags::INVERSE),
                }).with_soft_wrap(source.flags.contains(Flags::WRAPLINE))
            }).collect();
            (id,cells)
        })
    }
}

impl AlacrittyTerminalEngine {
    pub fn generation(&self)->u64 {self.generation}
    pub fn set_history_budget(&mut self, bytes: usize) {
        self.history_budget = bytes.min(DEFAULT_HISTORY_BYTES);
        self.enforce_history_budget();
    }
    pub fn retained_history_bytes(&self) -> usize { self.history_bytes }
    pub fn history_budget_bytes(&self) -> usize { self.history_budget }
    fn enforce_history_budget(&mut self) {
        let row_cost = usize::from(self.size.cols) * std::mem::size_of::<alacritty_terminal::term::cell::Cell>() + 40;
        let rows = (self.history_budget / row_cost).saturating_sub(64).min(10_000);
        self.history_bytes = self.terminal.limit_history_bytes(self.history_budget, rows);
    }

    /// Clone a bounded native slice. Dynamic cell payloads are shared immutably;
    /// expensive UTF-8/style conversion can run after releasing the parser lock.
    pub fn history_slice(&self, anchor: Option<u64>, before: usize, after: usize) -> HistorySlice {
        use alacritty_terminal::index::{Column, Line, Point};
        let grid = self.terminal.grid();
        let history = grid.history_size();
        let total = history + usize::from(self.size.rows);
        let offset = grid.history_offset();
        let anchor = anchor.unwrap_or(offset + total as u64).clamp(offset + 1, offset + total.max(1) as u64);
        let anchor = (anchor - offset) as usize;
        let limit = (131072 / usize::from(self.size.cols).max(1)).clamp(1, 512);
        let wanted_after = after.min(limit - 1).min(total - anchor);
        let wanted_before = before.min(limit - 1).min(anchor - 1);
        let (before, after) = if wanted_before + wanted_after + 1 <= limit {
            (wanted_before, wanted_after)
        } else {
            let after = wanted_after.min((limit - 1) / 2);
            let before = wanted_before.min(limit - 1 - after);
            (before, wanted_after.min(limit - 1 - before))
        };
        let start = anchor - before - 1;
        let end = anchor + after;
        let rows = (start..end).map(|index| {
            let line = Line(index as i32 - history as i32);
            let cells = (0..usize::from(self.size.cols)).map(|col| grid[Point::new(line,Column(col))].clone()).collect();
            (offset + (index+1) as u64, cells)
        }).collect();
        HistorySlice { total: offset + total as u64, first_line: offset + 1, anchor: offset + anchor as u64, generation: self.generation,
            rows, colors: self.terminal.renderable_content().colors.clone() }
    }
    pub fn history_page(&self, anchor: Option<u64>, before: usize, after: usize) -> (u64, u64, Vec<(u64, Vec<Cell>)>) {
        let slice = self.history_slice(anchor, before, after);
        (slice.total, slice.anchor, slice.into_rows().collect())
    }

    /// Start a fresh remote shell without carrying parser fragments or keyboard
    /// modes across transports. Keep snapshot generations monotonic.
    pub fn reset_connection(&mut self, size: TerminalSize) {
        let generation = self.generation.saturating_add(1);
        let budget = self.history_budget;
        let command_id=self.shell.next_id();
        *self = Self::new(size);
        self.shell.restore_next(command_id);
        self.set_history_budget(budget);
        self.generation = generation;
    }

    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        let size = bounded_size(size);
        let response_sink = ResponseSink::default();
        let dimensions = TerminalDimensions::from(size);
        let config = Config {
            scrolling_history: 10_000,
            kitty_keyboard: true,
            // Clipboard consent is handled by the daemon's bounded OSC52 observer.
            // The adapter must never decode clipboard payloads or answer reads;
            // upstream's OnlyCopy default would bypass that resource boundary.
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let mut terminal = Term::new(config, &dimensions, response_sink.clone());
        // The initial empty snapshot is published by the pipeline before any
        // output arrives, so subsequent deltas only need actual VT damage.
        terminal.reset_damage();
        Self {
            terminal,
            parser: ansi::Processor::new(),
            response_sink,
            xterm_keyboard: XtermKeyboardModes::default(),
            size,
            generation: 0,
            output_guard: Default::default(),
            shell: Default::default(),
            history_budget: DEFAULT_HISTORY_BYTES,
            history_bytes: 0,
        }
    }
}

impl TerminalEngine for AlacrittyTerminalEngine {
    fn feed(&mut self, bytes: &[u8]) -> FrameDelta {
        let base_generation = self.generation;
        // Parse bounded slices, enforcing retained allocation limits between slices.
        for chunk in bytes.chunks(64 * 1024) {
            let filtered = self.output_guard.filter(chunk);
            let mut consumed = 0;
            for (offset, event) in std::mem::take(&mut self.output_guard.events) {
                self.advance_output(&filtered[consumed..offset]);
                self.observe_shell(&event);
                consumed = offset;
            }
            self.advance_output(&filtered[consumed..]);
            self.enforce_history_budget();
        }
        self.generation = self.generation.saturating_add(1);
        let dirty_rows = match self.terminal.damage() {
            TermDamage::Full => (0..self.size.rows).collect(),
            TermDamage::Partial(lines) => lines
                .filter_map(|bounds| u16::try_from(bounds.line).ok())
                .collect(),
        };
        self.terminal.reset_damage();
        FrameDelta {
            base_generation,
            generation: self.generation,
            dirty_rows,
            terminal_responses: self.response_sink.drain(),
        }
    }

    fn resize(&mut self, size: TerminalSize) {
        let size = bounded_size(size);
        if size.rows == self.size.rows && size.cols == self.size.cols { return; }
        if size.cols != self.size.cols { self.shell.invalidate_positions(); }
        // Bound history by the wider of the two row sizes before reflow.
        let row_cost = usize::from(size.cols.max(self.size.cols)) * 24 + 40;
        let rows = (self.history_budget / row_cost).saturating_sub(64).min(10_000);
        self.terminal.limit_history_bytes(self.history_budget, rows);
        self.terminal.resize(TerminalDimensions::from(size));
        self.size = size;
        self.enforce_history_budget();
        self.generation = self.generation.saturating_add(1);
    }

    fn modes(&self) -> TerminalModes {
        let modes = self.terminal.mode();
        let mut kitty_keyboard_flags = 0;
        if modes.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
            kitty_keyboard_flags |= TerminalModes::KITTY_DISAMBIGUATE;
        }
        if modes.contains(TermMode::REPORT_EVENT_TYPES) {
            kitty_keyboard_flags |= TerminalModes::KITTY_REPORT_EVENTS;
        }
        if modes.contains(TermMode::REPORT_ALTERNATE_KEYS) {
            kitty_keyboard_flags |= TerminalModes::KITTY_REPORT_ALTERNATE_KEYS;
        }
        if modes.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
            kitty_keyboard_flags |= TerminalModes::KITTY_REPORT_ALL_KEYS;
        }
        if modes.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
            kitty_keyboard_flags |= TerminalModes::KITTY_REPORT_ASSOCIATED_TEXT;
        }
        TerminalModes {
            mouse_tracking: if modes.contains(TermMode::MOUSE_MOTION) { 3 } else if modes.contains(TermMode::MOUSE_DRAG) { 2 } else if modes.contains(TermMode::MOUSE_REPORT_CLICK) { 1 } else { 0 },
            mouse_encoding: if modes.contains(TermMode::SGR_MOUSE) { 2 } else if modes.contains(TermMode::UTF8_MOUSE) { 1 } else { 0 },
            application_cursor: modes.contains(TermMode::APP_CURSOR),
            bracketed_paste: modes.contains(TermMode::BRACKETED_PASTE),
            kitty_keyboard_flags,
            modify_other_keys: self.xterm_keyboard.modify_other_keys,
            format_other_keys: self.xterm_keyboard.format_other_keys,
        }
    }

    fn snapshot(&self) -> FrameSnapshot {
        let content = self.terminal.renderable_content();
        let cursor_row = content.cursor.point.line.0.clamp(0, i32::from(u16::MAX)) as u16;
        let cursor_col = content.cursor.point.column.0.min(usize::from(u16::MAX)) as u16;
        let cursor_shape = map_cursor_shape(content.cursor.shape);
        let cursor_appearance = CursorAppearance {
            shape: cursor_shape,
            blinking: cursor_shape != CursorShape::Hidden && self.terminal.cursor_style().blinking,
            color: content.colors[NamedColor::Cursor].map(|rgb| Color::Rgb(rgb.r, rgb.g, rgb.b)),
        };
        let rows = self.size.rows.max(1);
        let cols = self.size.cols.max(1);
        let mut cells = vec![Cell::default(); usize::from(rows) * usize::from(cols)];
        let mut hyperlink_uris = HashMap::<Arc<str>, ()>::new();
        for indexed in content.display_iter {
            let Ok(row) = usize::try_from(indexed.point.line.0) else {
                continue;
            };
            let col = indexed.point.column.0;
            if row >= usize::from(rows) || col >= usize::from(cols) {
                continue;
            }
            let source = indexed.cell;
            let width = if source.flags.contains(Flags::WIDE_CHAR) {
                CellWidth::Wide
            } else if source.flags.contains(Flags::WIDE_CHAR_SPACER) {
                CellWidth::WideSpacer
            } else if source.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                CellWidth::LeadingWideSpacer
            } else {
                CellWidth::Single
            };
            // Alacritty retains a horizontal-tab marker in the grid cell where
            // the control character was received. It is terminal bookkeeping,
            // not a printable glyph; publishing it would make renderer output
            // depend on font-specific handling of U+0009.
            let character = if source.c == '\t' { ' ' } else { source.c };
            let mut cell = Cell::with_zerowidth(
                character,
                source.zerowidth().into_iter().flatten().copied(),
                width,
                Style {
                    foreground: map_color(source.fg, content.colors),
                    background: map_color(source.bg, content.colors),
                    bold: source.flags.contains(Flags::BOLD),
                    italic: source.flags.contains(Flags::ITALIC),
                    underline: source.flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: source.flags.contains(Flags::INVERSE),
                },
            );
            if let Some(hyperlink) = source.hyperlink() {
                let uri = hyperlink.uri();
                if !uri.is_empty() && uri.len() <= crate::MAX_HYPERLINK_URI_BYTES {
                    let shared_uri =
                        if let Some((shared_uri, ())) = hyperlink_uris.get_key_value(uri) {
                            shared_uri.clone()
                        } else {
                            let shared_uri = Arc::<str>::from(uri);
                            hyperlink_uris.insert(shared_uri.clone(), ());
                            shared_uri
                        };
                    cell = cell.with_hyperlink_uri(shared_uri);
                }
            }
            cells[row * usize::from(cols) + col] = cell.with_soft_wrap(source.flags.contains(Flags::WRAPLINE));
        }
        FrameSnapshot {
            generation: self.generation,
            screen_start_line: self.terminal.grid().history_offset() + self.terminal.grid().history_size() as u64 + 1,
            rows,
            cols,
            cursor_row: cursor_row.min(rows.saturating_sub(1)),
            cursor_col: cursor_col.min(cols.saturating_sub(1)),
            cursor_appearance,
            terminal_modes: self.modes(),
            cells,
        }
    }
}

fn map_cursor_shape(shape: AlacrittyCursorShape) -> CursorShape {
    match shape {
        AlacrittyCursorShape::Block => CursorShape::Block,
        AlacrittyCursorShape::Underline => CursorShape::Underline,
        AlacrittyCursorShape::Beam => CursorShape::Beam,
        AlacrittyCursorShape::HollowBlock => CursorShape::HollowBlock,
        AlacrittyCursorShape::Hidden => CursorShape::Hidden,
    }
}

fn map_color(color: AlacrittyColor, dynamic_colors: &Colors) -> Color {
    let index = match color {
        AlacrittyColor::Spec(_) => None,
        AlacrittyColor::Indexed(index) => Some(usize::from(index)),
        AlacrittyColor::Named(named) => Some(named as usize),
    };
    if let Some(rgb) = index.and_then(|index| dynamic_colors[index]) {
        return Color::Rgb(rgb.r, rgb.g, rgb.b);
    }
    match color {
        AlacrittyColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AlacrittyColor::Indexed(index) => Color::Indexed(index),
        AlacrittyColor::Named(named) => map_named_color(named),
    }
}

fn map_named_color(color: NamedColor) -> Color {
    let index = color as usize;
    if index <= NamedColor::BrightWhite as usize {
        Color::Indexed(index as u8)
    } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&index) {
        Color::Indexed((index - NamedColor::DimBlack as usize) as u8)
    } else {
        Color::Default
    }
}

#[cfg(test)]
mod tests {
    use super::AlacrittyTerminalEngine;
    use crate::{CellWidth, Color, CursorAppearance, CursorShape, TerminalEngine};
    use cshell_domain::TerminalSize;

    #[test]
    fn produces_project_owned_cells_and_true_color() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 8));
        terminal.feed(b"\x1b[31mA\x1b[38;2;1;2;3mB");
        let snapshot = terminal.snapshot();
        let row = snapshot.row(0).unwrap_or_default();
        assert_eq!(row[0].character, 'A');
        assert_eq!(row[0].style.foreground, Color::Indexed(1));
        assert_eq!(row[1].character, 'B');
        assert_eq!(row[1].style.foreground, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn exposes_terminal_generated_pty_responses() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        terminal.feed(b"abc");
        let delta = terminal.feed(b"\x1b[6n");
        assert_eq!(delta.terminal_responses, vec![b"\x1b[1;4R".to_vec()]);
    }

    #[test]
    fn exposes_input_relevant_terminal_modes() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        terminal.feed(b"\x1b[?1h\x1b[?2004h");
        let modes = terminal.modes();
        assert!(modes.application_cursor);
        assert!(modes.bracketed_paste);
        assert_eq!(terminal.snapshot().terminal_modes, modes);

        terminal.feed(b"\x1b[?1l\x1b[?2004l");
        assert_eq!(terminal.modes(), crate::TerminalModes::default());
    }

    #[test]
    fn negotiates_and_reports_kitty_keyboard_modes() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        terminal.feed(b"\x1b[>31u");
        assert_eq!(terminal.modes().kitty_keyboard_flags, 31);

        let query = terminal.feed(b"\x1b[?u");
        assert_eq!(query.terminal_responses, vec![b"\x1b[?31u".to_vec()]);

        terminal.feed(b"\x1b[<u");
        assert_eq!(terminal.modes().kitty_keyboard_flags, 0);
    }

    #[test]
    fn tracks_fragmented_xterm_other_key_modes_and_answers_queries() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        terminal.feed(b"\x1b[>4;");
        terminal.feed(b"2m\x1b[>4;1f");
        let modes = terminal.modes();
        assert_eq!(modes.modify_other_keys, 2);
        assert!(modes.format_other_keys);

        let query = terminal.feed(b"\x1b[6n\x1b[?4m\x1b[?4g\x1b[6n");
        assert_eq!(
            query.terminal_responses,
            vec![
                b"\x1b[1;1R".to_vec(),
                b"\x1b[>4;2m".to_vec(),
                b"\x1b[>4;1f".to_vec(),
                b"\x1b[1;1R".to_vec(),
            ]
        );

        terminal.feed(b"\x1b[>4n\x1b[>4f");
        assert_eq!(terminal.modes(), crate::TerminalModes::default());
    }

    #[test]
    fn reports_only_rows_damaged_by_output_and_cursor_motion() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(4, 8));
        let first = terminal.feed(b"A");
        assert_eq!(first.dirty_rows, vec![0]);

        let moved = terminal.feed(b"\x1b[3;1H?");
        assert_eq!(moved.dirty_rows, vec![0, 2]);
    }

    #[test]
    fn resize_forces_a_full_damage_frame() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(4, 8));
        terminal.resize(TerminalSize::cells(6, 10));
        let delta = terminal.feed(&[]);
        assert_eq!(delta.dirty_rows, (0..6).collect::<Vec<_>>());
    }

    #[test]
    fn preserves_wide_spacers_and_zero_width_combining_characters() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 8));
        terminal.feed("\u{4f60}e\u{301}".as_bytes());
        let snapshot = terminal.snapshot();
        let row = snapshot.row(0).unwrap_or_default();
        assert_eq!(row[0].character, '\u{4f60}');
        assert_eq!(row[0].width, CellWidth::Wide);
        assert_eq!(row[1].width, CellWidth::WideSpacer);
        assert_eq!(row[2].character, 'e');
        assert_eq!(row[2].zerowidth(), &['\u{301}']);
        assert_eq!(row[2].characters().collect::<String>(), "e\u{301}");
    }

    #[test]
    fn preserves_osc_8_hyperlinks_without_leaking_upstream_types() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 8));
        terminal.feed(b"\x1b]8;id=docs;https://example.test/a\x1b\\A\x1b]8;;\x1b\\B");
        let snapshot = terminal.snapshot();
        let row = snapshot.row(0).unwrap_or_default();
        assert_eq!(row[0].hyperlink_uri(), Some("https://example.test/a"));
        assert_eq!(row[1].hyperlink_uri(), None);
    }

    #[test]
    fn resolves_and_resets_osc_dynamic_colors_in_project_cells() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 8));
        terminal.feed(b"\x1b]4;1;rgb:01/02/03\x07\x1b[31mA");
        assert_eq!(
            terminal.snapshot().row(0).unwrap_or_default()[0]
                .style
                .foreground,
            Color::Rgb(1, 2, 3)
        );

        terminal.feed(b"\x1b]104;1\x07");
        assert_eq!(
            terminal.snapshot().row(0).unwrap_or_default()[0]
                .style
                .foreground,
            Color::Indexed(1)
        );

        terminal.feed(b"\x1b[0m\x1b]10;rgb:04/05/06\x07\x1b]11;rgb:07/08/09\x07B");
        let snapshot = terminal.snapshot();
        let cell = &snapshot.row(0).unwrap_or_default()[1];
        assert_eq!(cell.style.foreground, Color::Rgb(4, 5, 6));
        assert_eq!(cell.style.background, Color::Rgb(7, 8, 9));
    }

    #[test]
    fn preserves_fragmented_cursor_shape_visibility_blinking_and_osc_12_color() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 8));
        let input = b"\x1b]12;rgb:0a/14/1e\x07\x1b[5 q";
        for chunk in input.chunks(2) {
            terminal.feed(chunk);
        }
        assert_eq!(
            terminal.snapshot().cursor_appearance,
            CursorAppearance {
                shape: CursorShape::Beam,
                blinking: true,
                color: Some(Color::Rgb(10, 20, 30)),
            }
        );

        terminal.feed(b"\x1b[?25l");
        assert_eq!(
            terminal.snapshot().cursor_appearance,
            CursorAppearance {
                shape: CursorShape::Hidden,
                blinking: false,
                color: Some(Color::Rgb(10, 20, 30)),
            }
        );

        terminal.feed(b"\x1b[?25h\x1b[4 q\x1b]112\x07");
        assert_eq!(
            terminal.snapshot().cursor_appearance,
            CursorAppearance {
                shape: CursorShape::Underline,
                blinking: false,
                color: None,
            }
        );
    }

    #[test]
    fn discards_unbounded_osc_8_hyperlinks_before_snapshot_publication() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 8));
        let uri = format!(
            "https://example.test/{}",
            "x".repeat(crate::MAX_HYPERLINK_URI_BYTES)
        );
        let input = format!("\u{1b}]8;;{uri}\u{1b}\u{5c}A\u{1b}]8;;\u{1b}\u{5c}");
        terminal.feed(input.as_bytes());
        assert_eq!(
            terminal.snapshot().row(0).unwrap_or_default()[0].hyperlink_uri(),
            None
        );
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    #[test]
    fn history_preserves_old_commands_colors_unicode_and_does_not_change_live_screen() {
        let mut terminal=AlacrittyTerminalEngine::new(TerminalSize::cells(4,40));
        terminal.feed("user$ echo hello\r\n\x1b[31m你好 e\u{301}\x1b[0m\r\n".as_bytes());
        for i in 0..100 {terminal.feed(format!("line {i}\r\n").as_bytes());}
        let live=terminal.snapshot();
        let (total,anchor,rows)=terminal.history_page(Some(1),0,5);
        assert!(total>100);assert_eq!(anchor,1);
        let text:Vec<String>=rows.iter().map(|(_,cells)|cells.iter().filter(|c|!matches!(c.width,CellWidth::WideSpacer|CellWidth::LeadingWideSpacer)).flat_map(|c|c.characters()).collect()).collect();
        assert!(text[0].contains("user$ echo hello"));assert!(text[1].contains("你好 e\u{301}"));
        assert_eq!(rows[1].1[0].style.foreground,Color::Indexed(1));
        assert_eq!(terminal.snapshot(),live);
        for _ in 0..10100 {terminal.feed(b"next\r\n");}
        let (total,_,rows)=terminal.history_page(None,usize::MAX,usize::MAX);
        assert!(total>=10104);assert!(rows[0].0>1);assert!(rows.len()<=4096);
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    #[test]
    fn native_combining_storage_is_bounded_and_osc_overflow_does_not_leak_text() {
        use alacritty_terminal::index::{Line,Column};
        let mut term = AlacrittyTerminalEngine::new(TerminalSize::cells(4,80));
        term.feed(b"a");
        for _ in 0..32 { term.feed("\u{301}".repeat(8192).as_bytes()); }
        assert_eq!(term.terminal.grid()[Line(0)][Column(0)].zerowidth().unwrap().len(),64);
        term.feed(b"\x1b]0;");
        for _ in 0..128 {term.feed(&vec![b'x';65536]);}
        term.feed(b"\x1b\\OK");
        assert_eq!(term.snapshot().row(0).unwrap()[1].character,'O');
    }
    #[test]
    fn oversized_hyperlinks_do_not_enter_native_cells_and_utf8_still_recovers() {
        use alacritty_terminal::index::{Line,Column};
        let mut term = AlacrittyTerminalEngine::new(TerminalSize::cells(4,80));
        term.feed(format!("\x1b]8;;https://example.org/{}\x07x\x1b]8;;\x07", "a".repeat(8192)).as_bytes());
        assert!(term.terminal.grid()[Line(0)][Column(0)].hyperlink().is_none());
        term.feed("你好".as_bytes());
        assert_eq!(term.snapshot().row(0).unwrap()[1].character,'你');
    }
    #[test]
    fn byte_budget_reclaims_oldest_rows_and_keeps_ids_and_alternate_screen() {
        let mut term = AlacrittyTerminalEngine::new(TerminalSize::cells(4,120));
        term.set_history_budget(256*1024);
        term.feed("row\r\n".repeat(15000).as_bytes());
        assert!(term.retained_history_bytes()<=256*1024);
        let first = term.history_slice(Some(1),0,4);
        assert!(first.first_line>1);assert_eq!(first.anchor,first.first_line);
        let before = first.first_line;
        term.feed(b"\x1b[?1049h\x1b[HTOP");
        term.set_history_budget(128*1024);
        assert_eq!(term.snapshot().row(0).unwrap()[0].character,'T');
        term.feed(b"\x1b[?1049l");
        assert!(term.history_slice(Some(1),0,4).first_line>=before);
        assert!(term.retained_history_bytes()<=128*1024);
        term.resize(TerminalSize::cells(24,500));
        assert!(term.retained_history_bytes()<=128*1024);
    }
}
