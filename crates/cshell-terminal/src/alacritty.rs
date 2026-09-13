use crate::{
    Cell, CellWidth, Color, CursorAppearance, CursorShape, FrameDelta, FrameSnapshot, Style,
    TerminalEngine, TerminalModes,
};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    self, Color as AlacrittyColor, CursorShape as AlacrittyCursorShape, NamedColor,
};
use cshell_domain::TerminalSize;
use std::collections::HashMap;
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

impl AlacrittyTerminalEngine {
    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        let response_sink = ResponseSink::default();
        let dimensions = TerminalDimensions::from(size);
        let config = Config {
            kitty_keyboard: true,
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
        }
    }
}

impl TerminalEngine for AlacrittyTerminalEngine {
    fn feed(&mut self, bytes: &[u8]) -> FrameDelta {
        let base_generation = self.generation;
        let mut consumed = 0;
        for (offset, response) in self.xterm_keyboard.observe(bytes) {
            self.parser
                .advance(&mut self.terminal, &bytes[consumed..=offset]);
            self.response_sink.push(response);
            consumed = offset + 1;
        }
        self.parser.advance(&mut self.terminal, &bytes[consumed..]);
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
        self.terminal.resize(TerminalDimensions::from(size));
        self.size = size;
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
            cells[row * usize::from(cols) + col] = cell;
        }
        FrameSnapshot {
            generation: self.generation,
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
