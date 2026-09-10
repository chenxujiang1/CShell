use crate::{
    Cell, CellWidth, Color, FrameDelta, FrameSnapshot, Style, TerminalEngine, TerminalModes,
};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{self, Color as AlacrittyColor, NamedColor};
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
        let mut terminal = Term::new(Config::default(), &dimensions, response_sink.clone());
        // The initial empty snapshot is published by the pipeline before any
        // output arrives, so subsequent deltas only need actual VT damage.
        terminal.reset_damage();
        Self {
            terminal,
            parser: ansi::Processor::new(),
            response_sink,
            size,
            generation: 0,
        }
    }
}

impl TerminalEngine for AlacrittyTerminalEngine {
    fn feed(&mut self, bytes: &[u8]) -> FrameDelta {
        let base_generation = self.generation;
        self.parser.advance(&mut self.terminal, bytes);
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
        TerminalModes {
            application_cursor: modes.contains(TermMode::APP_CURSOR),
            bracketed_paste: modes.contains(TermMode::BRACKETED_PASTE),
        }
    }

    fn snapshot(&self) -> FrameSnapshot {
        let content = self.terminal.renderable_content();
        let cursor_row = content.cursor.point.line.0.clamp(0, i32::from(u16::MAX)) as u16;
        let cursor_col = content.cursor.point.column.0.min(usize::from(u16::MAX)) as u16;
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
            terminal_modes: self.modes(),
            cells,
        }
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
    use crate::{CellWidth, Color, TerminalEngine};
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
