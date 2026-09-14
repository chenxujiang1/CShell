use cshell_terminal::{Cell, CellWidth, FrameSnapshot};
use regex::{Regex, RegexBuilder};
use std::sync::Arc;
use thiserror::Error;

pub const MAX_TERMINAL_SEARCH_QUERY_BYTES: usize = 4 * 1024;
pub const MAX_TERMINAL_SEARCH_MATCHES: usize = 4 * 1024;
pub const MAX_TERMINAL_SEARCH_REGEX_BYTES: usize = 1024 * 1024;
pub const MAX_TERMINAL_SELECTION_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct TerminalCellPoint {
    pub row: u16,
    pub column: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalSelectionMode {
    Character,
    Line,
    Block,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSelection {
    pub anchor: TerminalCellPoint,
    pub focus: TerminalCellPoint,
    pub mode: TerminalSelectionMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSearchMatch {
    pub start: TerminalCellPoint,
    pub end: TerminalCellPoint,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalSearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalSearchResult {
    pub matches: Arc<[TerminalSearchMatch]>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TerminalDecorations {
    pub revision: u64,
    pub selection: Option<TerminalSelection>,
    pub search: TerminalSearchResult,
    pub active_search_match: Option<usize>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum TerminalInteractionError {
    #[error("terminal search query contains {actual} bytes; maximum is {maximum}")]
    SearchQueryTooLong { actual: usize, maximum: usize },
    #[error("terminal search regular expression is invalid: {0}")]
    InvalidSearchRegex(String),
    #[error("terminal search regular expression must not match empty text")]
    EmptySearchRegexMatch,
    #[error("terminal selection contains more than {0} UTF-8 bytes")]
    SelectionTooLarge(usize),
}

impl TerminalSelection {
    #[must_use]
    pub fn contains(self, snapshot: &FrameSnapshot, row: u16, column: u16) -> bool {
        if row >= snapshot.rows || column >= snapshot.cols {
            return false;
        }
        match self.mode {
            TerminalSelectionMode::Character => {
                let (start, end) = ordered_points(
                    snap_wide_point(snapshot, self.anchor),
                    snap_wide_point(snapshot, self.focus),
                );
                let point = snap_wide_point(snapshot, TerminalCellPoint { row, column });
                point >= start && point <= end
            }
            TerminalSelectionMode::Line => {
                let first = self.anchor.row.min(self.focus.row);
                let last = self.anchor.row.max(self.focus.row);
                (first..=last).contains(&row)
            }
            TerminalSelectionMode::Block => {
                let first_row = self.anchor.row.min(self.focus.row);
                let last_row = self.anchor.row.max(self.focus.row);
                let first_column = self.anchor.column.min(self.focus.column);
                let last_column = self.anchor.column.max(self.focus.column);
                (first_row..=last_row).contains(&row)
                    && (first_column..=last_column).contains(&column)
            }
        }
    }

    pub fn text(self, snapshot: &FrameSnapshot) -> Result<String, TerminalInteractionError> {
        if snapshot.rows == 0 || snapshot.cols == 0 {
            return Ok(String::new());
        }
        let mut output = String::new();
        let first_row = self.anchor.row.min(self.focus.row).min(snapshot.rows - 1);
        let last_row = self.anchor.row.max(self.focus.row).min(snapshot.rows - 1);
        for row in first_row..=last_row {
            if row > first_row {
                push_bounded(&mut output, "\n")?;
            }
            let (first_column, last_column) = self.columns_for_row(snapshot, row);
            let Some(cells) = snapshot.row(row) else {
                continue;
            };
            for column in first_column..=last_column {
                let column = usize::from(column);
                let Some(cell) = cells.get(column) else {
                    break;
                };
                if is_spacer(cell) {
                    continue;
                }
                let mut encoded = [0_u8; 4];
                push_bounded(&mut output, cell.character.encode_utf8(&mut encoded))?;
                for character in cell.zerowidth() {
                    let mut encoded = [0_u8; 4];
                    push_bounded(&mut output, character.encode_utf8(&mut encoded))?;
                }
            }
        }
        Ok(output)
    }

    fn columns_for_row(self, snapshot: &FrameSnapshot, row: u16) -> (u16, u16) {
        let last = snapshot.cols.saturating_sub(1);
        match self.mode {
            TerminalSelectionMode::Line => (0, last),
            TerminalSelectionMode::Block => (
                self.anchor.column.min(self.focus.column).min(last),
                self.anchor.column.max(self.focus.column).min(last),
            ),
            TerminalSelectionMode::Character => {
                let (start, end) = ordered_points(self.anchor, self.focus);
                let first = if row == start.row { start.column } else { 0 }.min(last);
                let last = if row == end.row { end.column } else { last }.min(last);
                (snap_wide_start(snapshot, row, first), last)
            }
        }
    }
}

impl TerminalSearchMatch {
    #[must_use]
    pub fn contains(self, row: u16, column: u16) -> bool {
        row == self.start.row && (self.start.column..=self.end.column).contains(&column)
    }
}

pub fn search_terminal_snapshot(
    snapshot: &FrameSnapshot,
    query: &str,
    options: TerminalSearchOptions,
) -> Result<TerminalSearchResult, TerminalInteractionError> {
    if query.len() > MAX_TERMINAL_SEARCH_QUERY_BYTES {
        return Err(TerminalInteractionError::SearchQueryTooLong {
            actual: query.len(),
            maximum: MAX_TERMINAL_SEARCH_QUERY_BYTES,
        });
    }
    if query.is_empty() {
        return Ok(TerminalSearchResult::default());
    }
    let matcher = build_search_regex(query, options)?;
    let mut matches = Vec::new();
    for row in 0..snapshot.rows {
        let Some(cells) = snapshot.row(row) else {
            continue;
        };
        let (text, byte_columns) = searchable_row(cells);
        for matched in matcher.find_iter(&text) {
            if matched.is_empty() {
                return Err(TerminalInteractionError::EmptySearchRegexMatch);
            }
            let byte_start = matched.start();
            let byte_end = matched.end();
            if options.whole_word && !is_whole_word(&text, byte_start, byte_end) {
                continue;
            }
            let Some(&start_column) = byte_columns.get(byte_start) else {
                continue;
            };
            let Some(&end_column) = byte_columns.get(byte_end.saturating_sub(1)) else {
                continue;
            };
            if matches.len() == MAX_TERMINAL_SEARCH_MATCHES {
                return Ok(TerminalSearchResult {
                    matches: matches.into(),
                    truncated: true,
                });
            }
            matches.push(TerminalSearchMatch {
                start: TerminalCellPoint {
                    row,
                    column: start_column,
                },
                end: TerminalCellPoint {
                    row,
                    column: end_column,
                },
            });
        }
    }
    Ok(TerminalSearchResult {
        matches: matches.into(),
        truncated: false,
    })
}

fn build_search_regex(
    query: &str,
    options: TerminalSearchOptions,
) -> Result<Regex, TerminalInteractionError> {
    let pattern = if options.regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let matcher = RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .size_limit(MAX_TERMINAL_SEARCH_REGEX_BYTES)
        .build()
        .map_err(|error| TerminalInteractionError::InvalidSearchRegex(error.to_string()))?;
    if options.regex && matcher.is_match("") {
        return Err(TerminalInteractionError::EmptySearchRegexMatch);
    }
    Ok(matcher)
}

fn ordered_points(
    first: TerminalCellPoint,
    second: TerminalCellPoint,
) -> (TerminalCellPoint, TerminalCellPoint) {
    if first <= second {
        (first, second)
    } else {
        (second, first)
    }
}

fn snap_wide_start(snapshot: &FrameSnapshot, row: u16, column: u16) -> u16 {
    if column == 0 {
        return column;
    }
    snapshot
        .row(row)
        .and_then(|cells| cells.get(usize::from(column)))
        .filter(|cell| cell.width == CellWidth::WideSpacer)
        .map_or(column, |_| column - 1)
}

fn snap_wide_point(snapshot: &FrameSnapshot, point: TerminalCellPoint) -> TerminalCellPoint {
    TerminalCellPoint {
        row: point.row.min(snapshot.rows.saturating_sub(1)),
        column: snap_wide_start(
            snapshot,
            point.row.min(snapshot.rows.saturating_sub(1)),
            point.column.min(snapshot.cols.saturating_sub(1)),
        ),
    }
}

fn searchable_row(cells: &[Cell]) -> (String, Vec<u16>) {
    let mut text = String::new();
    let mut byte_columns = Vec::new();
    for (column, cell) in cells.iter().enumerate() {
        if is_spacer(cell) {
            continue;
        }
        let source: String = cell.characters().collect();
        let end_column = if cell.width == CellWidth::Wide {
            column.saturating_add(1)
        } else {
            column
        }
        .min(usize::from(u16::MAX)) as u16;
        byte_columns.extend(std::iter::repeat_n(end_column, source.len()));
        if let Some(first) = byte_columns.len().checked_sub(source.len())
            && !source.is_empty()
        {
            byte_columns[first] = column.min(usize::from(u16::MAX)) as u16;
        }
        text.push_str(&source);
    }
    (text, byte_columns)
}

fn is_whole_word(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(|character| !is_word_character(character))
        && after.is_none_or(|character| !is_word_character(character))
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_spacer(cell: &Cell) -> bool {
    matches!(
        cell.width,
        CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
    )
}

fn push_bounded(output: &mut String, text: &str) -> Result<(), TerminalInteractionError> {
    if output.len().saturating_add(text.len()) > MAX_TERMINAL_SELECTION_BYTES {
        return Err(TerminalInteractionError::SelectionTooLarge(
            MAX_TERMINAL_SELECTION_BYTES,
        ));
    }
    output.push_str(text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_TERMINAL_SEARCH_MATCHES, MAX_TERMINAL_SEARCH_QUERY_BYTES, TerminalCellPoint,
        TerminalInteractionError, TerminalSearchOptions, TerminalSelection, TerminalSelectionMode,
        search_terminal_snapshot,
    };
    use cshell_terminal::{Cell, CellWidth, FrameSnapshot, TerminalModes};

    fn snapshot(rows: &[Vec<Cell>]) -> FrameSnapshot {
        let row_count = u16::try_from(rows.len())
            .unwrap_or_else(|error| panic!("test row count must fit u16: {error}"));
        let cols = u16::try_from(rows.first().map_or(0, Vec::len))
            .unwrap_or_else(|error| panic!("test column count must fit u16: {error}"));
        FrameSnapshot {
            generation: 1,
            rows: row_count,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: Default::default(),
            terminal_modes: TerminalModes::default(),
            cells: rows.iter().flatten().cloned().collect(),
        }
    }

    fn cells(text: &str, cols: usize) -> Vec<Cell> {
        text.chars()
            .map(|character| Cell::new(character, CellWidth::Single, Default::default()))
            .chain(std::iter::repeat_with(Cell::default))
            .take(cols)
            .collect()
    }

    #[test]
    fn character_line_and_block_selection_copy_project_owned_cells() {
        let frame = snapshot(&[cells("alpha", 5), cells("bravo", 5), cells("charl", 5)]);
        let selection = TerminalSelection {
            anchor: TerminalCellPoint { row: 0, column: 2 },
            focus: TerminalCellPoint { row: 1, column: 2 },
            mode: TerminalSelectionMode::Character,
        };
        assert_eq!(
            selection
                .text(&frame)
                .unwrap_or_else(|error| panic!("selection must fit: {error}")),
            "pha\nbra"
        );
        assert!(selection.contains(&frame, 1, 1));
        assert!(!selection.contains(&frame, 2, 0));

        assert_eq!(
            TerminalSelection {
                mode: TerminalSelectionMode::Line,
                ..selection
            }
            .text(&frame)
            .unwrap_or_else(|error| panic!("line selection must fit: {error}")),
            "alpha\nbravo"
        );
        assert_eq!(
            TerminalSelection {
                anchor: TerminalCellPoint { row: 0, column: 1 },
                focus: TerminalCellPoint { row: 2, column: 3 },
                mode: TerminalSelectionMode::Block,
            }
            .text(&frame)
            .unwrap_or_else(|error| panic!("block selection must fit: {error}")),
            "lph\nrav\nhar"
        );
    }

    #[test]
    fn selection_and_search_preserve_wide_and_combining_cell_boundaries() {
        let wide = Cell::with_zerowidth('界', ['\u{fe0f}'], CellWidth::Wide, Default::default());
        let frame = snapshot(&[vec![
            Cell::new('A', CellWidth::Single, Default::default()),
            wide,
            Cell::new(' ', CellWidth::WideSpacer, Default::default()),
            Cell::new('B', CellWidth::Single, Default::default()),
        ]]);
        let selection = TerminalSelection {
            anchor: TerminalCellPoint { row: 0, column: 2 },
            focus: TerminalCellPoint { row: 0, column: 3 },
            mode: TerminalSelectionMode::Character,
        };
        assert_eq!(
            selection
                .text(&frame)
                .unwrap_or_else(|error| panic!("wide selection must fit: {error}")),
            "界️B"
        );

        let result = search_terminal_snapshot(
            &frame,
            "界️B",
            TerminalSearchOptions {
                case_sensitive: true,
                whole_word: false,
                regex: false,
            },
        )
        .unwrap_or_else(|error| panic!("wide search must be valid: {error}"));
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].start.column, 1);
        assert_eq!(result.matches[0].end.column, 3);
    }

    #[test]
    fn search_supports_unicode_case_and_whole_word_with_hard_limits() {
        let frame = snapshot(&[cells("Rust rustacean RUST", 19)]);
        let result = search_terminal_snapshot(
            &frame,
            "rust",
            TerminalSearchOptions {
                case_sensitive: false,
                whole_word: true,
                regex: false,
            },
        )
        .unwrap_or_else(|error| panic!("whole-word search must be valid: {error}"));
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].start.column, 0);
        assert_eq!(result.matches[1].start.column, 15);

        assert!(matches!(
            search_terminal_snapshot(
                &frame,
                &"x".repeat(MAX_TERMINAL_SEARCH_QUERY_BYTES + 1),
                TerminalSearchOptions::default()
            ),
            Err(TerminalInteractionError::SearchQueryTooLong { .. })
        ));

        let many = snapshot(&[cells(
            &"a".repeat(MAX_TERMINAL_SEARCH_MATCHES + 8),
            MAX_TERMINAL_SEARCH_MATCHES + 8,
        )]);
        let result = search_terminal_snapshot(&many, "a", TerminalSearchOptions::default())
            .unwrap_or_else(|error| panic!("bounded search must be valid: {error}"));
        assert_eq!(result.matches.len(), MAX_TERMINAL_SEARCH_MATCHES);
        assert!(result.truncated);
    }

    #[test]
    fn regex_search_is_bounded_validated_and_maps_terminal_cells() {
        let wide = Cell::with_zerowidth('界', ['\u{fe0f}'], CellWidth::Wide, Default::default());
        let frame = snapshot(&[vec![
            Cell::new('A', CellWidth::Single, Default::default()),
            wide,
            Cell::new(' ', CellWidth::WideSpacer, Default::default()),
            Cell::new('B', CellWidth::Single, Default::default()),
        ]]);
        let options = TerminalSearchOptions {
            case_sensitive: true,
            whole_word: false,
            regex: true,
        };
        let result = search_terminal_snapshot(&frame, "界.*B", options)
            .unwrap_or_else(|error| panic!("regex search must be valid: {error}"));
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].start.column, 1);
        assert_eq!(result.matches[0].end.column, 3);

        assert!(matches!(
            search_terminal_snapshot(&frame, "[", options),
            Err(TerminalInteractionError::InvalidSearchRegex(_))
        ));
        assert_eq!(
            search_terminal_snapshot(&frame, "a*", options),
            Err(TerminalInteractionError::EmptySearchRegexMatch)
        );
        assert_eq!(
            search_terminal_snapshot(&frame, "\\b", options),
            Err(TerminalInteractionError::EmptySearchRegexMatch)
        );

        let literal = snapshot(&[cells("a.b acb", 7)]);
        let literal_result =
            search_terminal_snapshot(&literal, "a.b", TerminalSearchOptions::default())
                .unwrap_or_else(|error| panic!("literal search must escape regex syntax: {error}"));
        assert_eq!(literal_result.matches.len(), 1);
        assert_eq!(literal_result.matches[0].start.column, 0);
    }
}
