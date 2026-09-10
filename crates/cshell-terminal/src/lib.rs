//! Bounded terminal snapshot model and Phase 0 parser probe.
//!
//! `ProbeTerminalEngine` proves the queue/snapshot/render contracts. The full
//! product adapter is `AlacrittyTerminalEngine`; the probe remains for focused tests.

mod alacritty;
mod input;

use cshell_domain::TerminalSize;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use unicode_width::UnicodeWidthChar;

pub use alacritty::AlacrittyTerminalEngine;
pub use input::{InputEncodeError, InputEncoder, TerminalModes};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum CellWidth {
    #[default]
    Single,
    Wide,
    WideSpacer,
    LeadingWideSpacer,
}

pub const MAX_ZERO_WIDTH_CHARS_PER_CELL: usize = 64;

#[must_use]
pub fn is_zero_width_character(character: char) -> bool {
    character.width().unwrap_or(0) == 0
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub character: char,
    zerowidth: Option<Arc<[char]>>,
    pub width: CellWidth,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            zerowidth: None,
            width: CellWidth::Single,
            style: Style::default(),
        }
    }
}

impl Cell {
    #[must_use]
    pub const fn new(character: char, width: CellWidth, style: Style) -> Self {
        Self {
            character,
            zerowidth: None,
            width,
            style,
        }
    }

    #[must_use]
    pub fn with_zerowidth(
        character: char,
        zerowidth: impl IntoIterator<Item = char>,
        width: CellWidth,
        style: Style,
    ) -> Self {
        let characters: Vec<_> = zerowidth
            .into_iter()
            .filter(|character| is_zero_width_character(*character))
            .take(MAX_ZERO_WIDTH_CHARS_PER_CELL)
            .collect();
        Self {
            character,
            zerowidth: (!characters.is_empty()).then(|| Arc::from(characters)),
            width,
            style,
        }
    }

    #[must_use]
    pub fn zerowidth(&self) -> &[char] {
        self.zerowidth.as_deref().unwrap_or_default()
    }

    pub fn characters(&self) -> impl Iterator<Item = char> + '_ {
        std::iter::once(self.character).chain(self.zerowidth().iter().copied())
    }

    fn push_zerowidth(&mut self, character: char) {
        if self.zerowidth().len() >= MAX_ZERO_WIDTH_CHARS_PER_CELL {
            return;
        }
        let mut characters = self.zerowidth().to_vec();
        characters.push(character);
        self.zerowidth = Some(Arc::from(characters));
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FrameSnapshot {
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub terminal_modes: TerminalModes,
    pub cells: Vec<Cell>,
}

impl FrameSnapshot {
    #[must_use]
    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        let start = usize::from(row).checked_mul(usize::from(self.cols))?;
        let end = start.checked_add(usize::from(self.cols))?;
        self.cells.get(start..end)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameDelta {
    pub base_generation: u64,
    pub generation: u64,
    pub dirty_rows: Vec<u16>,
    pub terminal_responses: Vec<Vec<u8>>,
}

pub trait TerminalEngine {
    fn feed(&mut self, bytes: &[u8]) -> FrameDelta;
    fn resize(&mut self, size: TerminalSize);
    fn modes(&self) -> TerminalModes;
    fn snapshot(&self) -> FrameSnapshot;
}

#[derive(Clone, Debug)]
enum ParserState {
    Ground,
    Escape,
    Csi(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct ProbeTerminalEngine {
    size: TerminalSize,
    cells: Vec<Cell>,
    cursor_row: u16,
    cursor_col: u16,
    style: Style,
    generation: u64,
    parser_state: ParserState,
    utf8_pending: Vec<u8>,
    dirty_rows: Vec<bool>,
    pending_responses: Vec<Vec<u8>>,
    modes: TerminalModes,
}

impl ProbeTerminalEngine {
    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        let cell_count = usize::from(size.rows).saturating_mul(usize::from(size.cols));
        Self {
            size,
            cells: vec![Cell::default(); cell_count],
            cursor_row: 0,
            cursor_col: 0,
            style: Style::default(),
            generation: 0,
            parser_state: ParserState::Ground,
            utf8_pending: Vec::with_capacity(4),
            dirty_rows: vec![true; usize::from(size.rows)],
            pending_responses: Vec::new(),
            modes: TerminalModes::default(),
        }
    }

    fn process_byte(&mut self, byte: u8) {
        let state = std::mem::replace(&mut self.parser_state, ParserState::Ground);
        match state {
            ParserState::Ground => self.process_ground(byte),
            ParserState::Escape => {
                if byte == b'[' {
                    self.parser_state = ParserState::Csi(Vec::with_capacity(16));
                }
            }
            ParserState::Csi(mut parameters) => {
                if (0x40..=0x7e).contains(&byte) {
                    self.apply_csi(&parameters, byte);
                } else if parameters.len() < 64 {
                    parameters.push(byte);
                    self.parser_state = ParserState::Csi(parameters);
                }
            }
        }
    }

    fn process_ground(&mut self, byte: u8) {
        match byte {
            0x1b => self.parser_state = ParserState::Escape,
            b'\r' => self.cursor_col = 0,
            b'\n' => self.line_feed(),
            0x08 => self.cursor_col = self.cursor_col.saturating_sub(1),
            b'\t' => {
                let next_tab = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_tab.min(self.size.cols.saturating_sub(1));
            }
            0x20..=0x7e => self.put_character(char::from(byte)),
            0x80..=0xff => self.process_utf8_byte(byte),
            _ => {}
        }
    }

    fn process_utf8_byte(&mut self, byte: u8) {
        self.utf8_pending.push(byte);
        let expected = expected_utf8_len(self.utf8_pending[0]);
        if expected == 0 || self.utf8_pending.len() > expected {
            self.put_character('\u{fffd}');
            self.utf8_pending.clear();
            return;
        }
        if self.utf8_pending.len() == expected {
            let character = std::str::from_utf8(&self.utf8_pending)
                .ok()
                .and_then(|text| text.chars().next())
                .unwrap_or('\u{fffd}');
            self.utf8_pending.clear();
            self.put_character(character);
        }
    }

    fn apply_csi(&mut self, bytes: &[u8], final_byte: u8) {
        let parameters = std::str::from_utf8(bytes).unwrap_or_default();
        if let Some(private_modes) = parameters.strip_prefix('?') {
            let enabled = match final_byte {
                b'h' => true,
                b'l' => false,
                _ => return,
            };
            for mode in private_modes.split(';') {
                match mode {
                    "1" => self.modes.application_cursor = enabled,
                    "2004" => self.modes.bracketed_paste = enabled,
                    _ => {}
                }
            }
            return;
        }
        let values: Vec<u16> = parameters
            .split(';')
            .filter_map(|value| value.parse().ok())
            .collect();
        match final_byte {
            b'm' => self.apply_sgr(&values),
            b'H' | b'f' => {
                self.cursor_row = values
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.size.rows.saturating_sub(1));
                self.cursor_col = values
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.size.cols.saturating_sub(1));
            }
            b'J' if values.first().copied().unwrap_or(0) == 2 => {
                self.cells.fill(Cell::default());
                self.dirty_rows.fill(true);
            }
            b'n' if values.first().copied() == Some(6) => {
                self.pending_responses.push(
                    format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1).into_bytes(),
                );
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, values: &[u16]) {
        let values = if values.is_empty() { &[0][..] } else { values };
        for value in values {
            match *value {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                22 => self.style.bold = false,
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                30..=37 => self.style.foreground = Color::Indexed((*value - 30) as u8),
                40..=47 => self.style.background = Color::Indexed((*value - 40) as u8),
                90..=97 => self.style.foreground = Color::Indexed((*value - 90 + 8) as u8),
                100..=107 => self.style.background = Color::Indexed((*value - 100 + 8) as u8),
                39 => self.style.foreground = Color::Default,
                49 => self.style.background = Color::Default,
                _ => {}
            }
        }
    }

    fn put_character(&mut self, character: char) {
        if self.size.rows == 0 || self.size.cols == 0 {
            return;
        }
        let character_width = character.width().unwrap_or(0).min(2) as u16;
        if character_width == 0 {
            let mut column = self.cursor_col.saturating_sub(1);
            let row_start = usize::from(self.cursor_row) * usize::from(self.size.cols);
            let mut index = row_start + usize::from(column);
            if self
                .cells
                .get(index)
                .is_some_and(|cell| cell.width == CellWidth::WideSpacer)
            {
                column = column.saturating_sub(1);
                index = row_start + usize::from(column);
            }
            if let Some(cell) = self.cells.get_mut(index) {
                cell.push_zerowidth(character);
            }
            if let Some(dirty) = self.dirty_rows.get_mut(usize::from(self.cursor_row)) {
                *dirty = true;
            }
            return;
        }
        if self.cursor_col >= self.size.cols
            || self.cursor_col.saturating_add(character_width) > self.size.cols
        {
            self.cursor_col = 0;
            self.line_feed();
        }
        let index = usize::from(self.cursor_row) * usize::from(self.size.cols)
            + usize::from(self.cursor_col);
        if let Some(cell) = self.cells.get_mut(index) {
            *cell = Cell::new(
                character,
                if character_width == 2 {
                    CellWidth::Wide
                } else {
                    CellWidth::Single
                },
                self.style,
            );
        }
        if character_width == 2
            && self.cursor_col.saturating_add(1) < self.size.cols
            && let Some(spacer) = self.cells.get_mut(index.saturating_add(1))
        {
            *spacer = Cell::new(' ', CellWidth::WideSpacer, self.style);
        }
        if let Some(dirty) = self.dirty_rows.get_mut(usize::from(self.cursor_row)) {
            *dirty = true;
        }
        self.cursor_col = self.cursor_col.saturating_add(character_width);
    }

    fn line_feed(&mut self) {
        if self.size.rows == 0 || self.size.cols == 0 {
            return;
        }
        if self.cursor_row + 1 < self.size.rows {
            self.cursor_row += 1;
            return;
        }
        let cols = usize::from(self.size.cols);
        self.cells.rotate_left(cols);
        let last_row_start = self.cells.len().saturating_sub(cols);
        self.cells[last_row_start..].fill(Cell::default());
        self.dirty_rows.fill(true);
    }
}

impl TerminalEngine for ProbeTerminalEngine {
    fn feed(&mut self, bytes: &[u8]) -> FrameDelta {
        let base_generation = self.generation;
        for byte in bytes {
            self.process_byte(*byte);
        }
        self.generation = self.generation.saturating_add(1);
        let dirty_rows = self
            .dirty_rows
            .iter_mut()
            .enumerate()
            .filter_map(|(row, dirty)| {
                let was_dirty = std::mem::replace(dirty, false);
                was_dirty.then_some(row as u16)
            })
            .collect();
        FrameDelta {
            base_generation,
            generation: self.generation,
            dirty_rows,
            terminal_responses: std::mem::take(&mut self.pending_responses),
        }
    }

    fn resize(&mut self, size: TerminalSize) {
        let mut replacement =
            vec![Cell::default(); usize::from(size.rows) * usize::from(size.cols)];
        let copy_rows = self.size.rows.min(size.rows);
        let copy_cols = self.size.cols.min(size.cols);
        for row in 0..copy_rows {
            let old_start = usize::from(row) * usize::from(self.size.cols);
            let new_start = usize::from(row) * usize::from(size.cols);
            replacement[new_start..new_start + usize::from(copy_cols)]
                .clone_from_slice(&self.cells[old_start..old_start + usize::from(copy_cols)]);
        }
        self.size = size;
        self.cells = replacement;
        self.cursor_row = self.cursor_row.min(size.rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(size.cols.saturating_sub(1));
        self.dirty_rows = vec![true; usize::from(size.rows)];
        self.generation = self.generation.saturating_add(1);
    }

    fn modes(&self) -> TerminalModes {
        self.modes
    }

    fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            generation: self.generation,
            rows: self.size.rows,
            cols: self.size.cols,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            terminal_modes: self.modes,
            cells: self.cells.clone(),
        }
    }
}

const fn expected_utf8_len(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{CellWidth, Color, ProbeTerminalEngine, TerminalEngine};
    use cshell_domain::TerminalSize;

    #[test]
    fn parses_color_and_utf8_across_chunks() {
        let mut terminal = ProbeTerminalEngine::new(TerminalSize::cells(2, 10));
        terminal.feed(b"\x1b[31mA\xe4");
        terminal.feed(b"\xb8\xade\xcc\x81");
        let snapshot = terminal.snapshot();
        let row = snapshot.row(0).unwrap_or_default();
        assert_eq!(row[0].character, 'A');
        assert_eq!(row[0].style.foreground, Color::Indexed(1));
        assert_eq!(row[1].character, '\u{4e2d}');
        assert_eq!(row[1].width, CellWidth::Wide);
        assert_eq!(row[2].width, CellWidth::WideSpacer);
        assert_eq!(row[3].character, 'e');
        assert_eq!(row[3].zerowidth(), &['\u{301}']);
    }

    #[test]
    fn flood_keeps_memory_bounded_by_view_size() {
        let mut terminal = ProbeTerminalEngine::new(TerminalSize::cells(24, 80));
        for _ in 0..20_000 {
            terminal.feed(b"0123456789\rprogress\n");
        }
        assert_eq!(terminal.snapshot().cells.len(), 24 * 80);
    }

    #[test]
    fn resize_preserves_visible_prefix() {
        let mut terminal = ProbeTerminalEngine::new(TerminalSize::cells(2, 4));
        terminal.feed(b"abc");
        terminal.resize(TerminalSize::cells(3, 8));
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.row(0).unwrap_or_default()[0].character, 'a');
    }

    #[test]
    fn device_status_report_generates_a_cursor_response() {
        let mut terminal = ProbeTerminalEngine::new(TerminalSize::cells(24, 80));
        terminal.feed(b"abc");
        let delta = terminal.feed(b"\x1b[6n");
        assert_eq!(delta.terminal_responses, vec![b"\x1b[1;4R".to_vec()]);
    }

    #[test]
    fn probe_tracks_input_relevant_private_modes() {
        let mut terminal = ProbeTerminalEngine::new(TerminalSize::cells(24, 80));
        terminal.feed(b"\x1b[?1;2004h");
        assert!(terminal.modes().application_cursor);
        assert!(terminal.modes().bracketed_paste);
        terminal.feed(b"\x1b[?1;2004l");
        assert_eq!(terminal.modes(), super::TerminalModes::default());
    }
}
