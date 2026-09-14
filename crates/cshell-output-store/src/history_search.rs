use crate::{JournalError, JournalLineIndex};
use regex::{Regex, RegexBuilder};
use thiserror::Error;

pub const MAX_HISTORY_SEARCH_QUERY_BYTES: usize = 4 * 1024;
pub const MAX_HISTORY_SEARCH_SCAN_LINES: usize = 4096;
pub const MAX_HISTORY_SEARCH_MATCHES: usize = 4096;
const MAX_HISTORY_SEARCH_REGEX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HistorySearchDirection {
    #[default]
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistorySearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// A stable logical-line cursor. `byte_offset` is inclusive for forward
/// searches and exclusive for backward searches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistorySearchCursor {
    pub line_id: u64,
    pub byte_offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistorySearchRequest {
    pub query: String,
    pub options: HistorySearchOptions,
    pub direction: HistorySearchDirection,
    pub cursor: Option<HistorySearchCursor>,
    pub max_scan_lines: usize,
    pub max_matches: usize,
}

impl HistorySearchRequest {
    #[must_use]
    pub fn forward(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            options: HistorySearchOptions::default(),
            direction: HistorySearchDirection::Forward,
            cursor: None,
            max_scan_lines: MAX_HISTORY_SEARCH_SCAN_LINES,
            max_matches: MAX_HISTORY_SEARCH_MATCHES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistorySearchMatch {
    pub line_id: u64,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistorySearchResult {
    pub revision: u64,
    pub matches: Vec<HistorySearchMatch>,
    pub scanned_lines: usize,
    pub next_cursor: Option<HistorySearchCursor>,
    /// True when at least one indexed line exceeded the bounded page budget.
    pub incomplete: bool,
}

#[derive(Debug, Error)]
pub enum HistorySearchError {
    #[error("history search query is empty")]
    EmptyQuery,
    #[error("history search query contains {actual} bytes; maximum is {maximum}")]
    QueryTooLong { actual: usize, maximum: usize },
    #[error("history search scan limit must be between 1 and {MAX_HISTORY_SEARCH_SCAN_LINES}")]
    InvalidScanLimit,
    #[error("history search match limit must be between 1 and {MAX_HISTORY_SEARCH_MATCHES}")]
    InvalidMatchLimit,
    #[error("history search regular expression is invalid: {0}")]
    InvalidRegex(String),
    #[error("history search regular expression must not match empty text")]
    EmptyRegexMatch,
    #[error("history search cursor is not on a valid UTF-8 boundary of line {0}")]
    InvalidCursor(u64),
    #[error(transparent)]
    Journal(#[from] JournalError),
}

impl JournalLineIndex {
    /// Searches one bounded page of ANSI-decoded history. Callers continue with
    /// `next_cursor`; no request scans or materializes the complete journal.
    pub fn search_history(
        &self,
        request: &HistorySearchRequest,
    ) -> Result<HistorySearchResult, HistorySearchError> {
        validate_request(request)?;
        let matcher = build_matcher(&request.query, request.options)?;
        let total = self.total_line_count();
        let revision = self.revision();
        if total == 0 {
            return Ok(HistorySearchResult {
                revision,
                matches: Vec::new(),
                scanned_lines: 0,
                next_cursor: None,
                incomplete: false,
            });
        }

        let cursor = request.cursor.unwrap_or(match request.direction {
            HistorySearchDirection::Forward => HistorySearchCursor {
                line_id: 1,
                byte_offset: 0,
            },
            HistorySearchDirection::Backward => HistorySearchCursor {
                line_id: total,
                byte_offset: usize::MAX,
            },
        });
        if cursor.line_id == 0 || cursor.line_id > total {
            return Err(JournalError::UnknownLine(cursor.line_id).into());
        }
        let side_rows = request.max_scan_lines.saturating_sub(1);
        let page = match request.direction {
            HistorySearchDirection::Forward => {
                self.read_styled_page(Some(cursor.line_id), 0, side_rows)?
            }
            HistorySearchDirection::Backward => {
                self.read_styled_page(Some(cursor.line_id), side_rows, 0)?
            }
        };
        let mut matches = Vec::new();
        let mut scanned_lines = 0;
        let mut incomplete = false;
        let rows: Box<dyn Iterator<Item = _>> = match request.direction {
            HistorySearchDirection::Forward => Box::new(page.rows.iter()),
            HistorySearchDirection::Backward => Box::new(page.rows.iter().rev()),
        };
        let mut last_line_id = cursor.line_id;

        for row in rows.take(request.max_scan_lines) {
            // The shared page reader has a total byte budget. Defer a later
            // truncated row to the next request instead of silently searching
            // only its prefix and advancing past it. A truncated first row is
            // an individually oversized logical line, which cannot be made
            // complete by reducing the page width; report it as incomplete.
            if row.truncated && scanned_lines > 0 {
                return Ok(HistorySearchResult {
                    revision: page.revision,
                    matches,
                    scanned_lines,
                    next_cursor: Some(line_start_cursor(row.line_id, request.direction)),
                    incomplete,
                });
            }
            scanned_lines += 1;
            last_line_id = row.line_id;
            incomplete |= row.truncated;
            let boundary = if row.line_id == cursor.line_id {
                cursor.byte_offset.min(row.text.len())
            } else {
                match request.direction {
                    HistorySearchDirection::Forward => 0,
                    HistorySearchDirection::Backward => row.text.len(),
                }
            };
            if !row.text.is_char_boundary(boundary) {
                return Err(HistorySearchError::InvalidCursor(row.line_id));
            }
            let found: Box<dyn Iterator<Item = _>> = match request.direction {
                HistorySearchDirection::Forward => Box::new(
                    matcher
                        .find_iter(&row.text)
                        .filter(|matched| matched.start() >= boundary),
                ),
                HistorySearchDirection::Backward => Box::new(
                    matcher
                        .find_iter(&row.text)
                        .filter(|matched| matched.end() <= boundary)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev(),
                ),
            };
            for matched in found {
                if matched.is_empty() {
                    return Err(HistorySearchError::EmptyRegexMatch);
                }
                if request.options.whole_word
                    && !is_whole_word(&row.text, matched.start(), matched.end())
                {
                    continue;
                }
                let result = HistorySearchMatch {
                    line_id: row.line_id,
                    byte_start: matched.start(),
                    byte_end: matched.end(),
                };
                matches.push(result);
                if matches.len() == request.max_matches {
                    return Ok(HistorySearchResult {
                        revision: page.revision,
                        matches,
                        scanned_lines,
                        next_cursor: Some(match request.direction {
                            HistorySearchDirection::Forward => HistorySearchCursor {
                                line_id: row.line_id,
                                byte_offset: result.byte_end,
                            },
                            HistorySearchDirection::Backward => HistorySearchCursor {
                                line_id: row.line_id,
                                byte_offset: result.byte_start,
                            },
                        }),
                        incomplete,
                    });
                }
            }
        }

        let next_line = match request.direction {
            HistorySearchDirection::Forward => {
                last_line_id.checked_add(1).filter(|id| *id <= total)
            }
            HistorySearchDirection::Backward => last_line_id.checked_sub(1).filter(|id| *id > 0),
        };
        Ok(HistorySearchResult {
            revision: page.revision,
            matches,
            scanned_lines,
            next_cursor: next_line.map(|line_id| line_start_cursor(line_id, request.direction)),
            incomplete,
        })
    }
}

fn line_start_cursor(line_id: u64, direction: HistorySearchDirection) -> HistorySearchCursor {
    HistorySearchCursor {
        line_id,
        byte_offset: match direction {
            HistorySearchDirection::Forward => 0,
            HistorySearchDirection::Backward => usize::MAX,
        },
    }
}

fn validate_request(request: &HistorySearchRequest) -> Result<(), HistorySearchError> {
    if request.query.is_empty() {
        return Err(HistorySearchError::EmptyQuery);
    }
    if request.query.len() > MAX_HISTORY_SEARCH_QUERY_BYTES {
        return Err(HistorySearchError::QueryTooLong {
            actual: request.query.len(),
            maximum: MAX_HISTORY_SEARCH_QUERY_BYTES,
        });
    }
    if request.max_scan_lines == 0 || request.max_scan_lines > MAX_HISTORY_SEARCH_SCAN_LINES {
        return Err(HistorySearchError::InvalidScanLimit);
    }
    if request.max_matches == 0 || request.max_matches > MAX_HISTORY_SEARCH_MATCHES {
        return Err(HistorySearchError::InvalidMatchLimit);
    }
    Ok(())
}

fn build_matcher(query: &str, options: HistorySearchOptions) -> Result<Regex, HistorySearchError> {
    let pattern = if options.regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let matcher = RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .size_limit(MAX_HISTORY_SEARCH_REGEX_BYTES)
        .build()
        .map_err(|error| HistorySearchError::InvalidRegex(error.to_string()))?;
    if options.regex && matcher.is_match("") {
        return Err(HistorySearchError::EmptyRegexMatch);
    }
    Ok(matcher)
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{Durability, JournalWriter};

    fn history() -> (tempfile::TempDir, JournalLineIndex) {
        let directory = tempfile::tempdir().unwrap();
        let mut writer =
            JournalWriter::create(directory.path().join("search.csjr"), Durability::Ephemeral)
                .unwrap();
        writer
            .append(
                b"zero\n\x1b[31mError one\x1b[0m\nwide \xe7\x95\x8c Error two\nERROR_THREE\ntail",
            )
            .unwrap();
        (directory, writer.line_index())
    }

    #[test]
    fn forward_search_is_ansi_decoded_and_continues_within_a_line() {
        let (_directory, index) = history();
        let mut request = HistorySearchRequest::forward("error");
        request.max_matches = 1;
        let first = index.search_history(&request).unwrap();
        assert_eq!(first.matches[0].line_id, 2);
        assert_eq!(first.matches[0].byte_start, 0);
        assert_eq!(first.next_cursor.unwrap().line_id, 2);

        request.cursor = first.next_cursor;
        let second = index.search_history(&request).unwrap();
        assert_eq!(second.matches[0].line_id, 3);
        assert_eq!(
            &"wide 界 Error two"[second.matches[0].byte_start..second.matches[0].byte_end],
            "Error"
        );
    }

    #[test]
    fn backward_regex_search_preserves_order_and_cursor() {
        let (_directory, index) = history();
        let mut request = HistorySearchRequest::forward("Error|ERROR");
        request.direction = HistorySearchDirection::Backward;
        request.options.regex = true;
        request.options.case_sensitive = true;
        request.max_matches = 2;
        let first = index.search_history(&request).unwrap();
        assert_eq!(
            first
                .matches
                .iter()
                .map(|item| item.line_id)
                .collect::<Vec<_>>(),
            vec![4, 3]
        );
        request.cursor = first.next_cursor;
        let second = index.search_history(&request).unwrap();
        assert_eq!(
            second
                .matches
                .iter()
                .map(|item| item.line_id)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn scan_and_match_limits_are_hard_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::create(
            directory.path().join("bounded-search.csjr"),
            Durability::Ephemeral,
        )
        .unwrap();
        writer.append(&b"hit hit\n".repeat(5000)).unwrap();
        let index = writer.line_index();
        let mut request = HistorySearchRequest::forward("hit");
        request.max_scan_lines = 17;
        request.max_matches = 7;
        let result = index.search_history(&request).unwrap();
        assert_eq!(result.scanned_lines, 4);
        assert_eq!(result.matches.len(), 7);
        assert_eq!(
            result.next_cursor,
            Some(HistorySearchCursor {
                line_id: 4,
                byte_offset: 3
            })
        );
    }

    #[test]
    fn validation_rejects_empty_matching_regex_and_bad_utf8_cursor() {
        let (_directory, index) = history();
        let mut request = HistorySearchRequest::forward("a*");
        request.options.regex = true;
        assert!(matches!(
            index.search_history(&request),
            Err(HistorySearchError::EmptyRegexMatch)
        ));

        request.query = "wide".to_owned();
        request.options.regex = false;
        request.cursor = Some(HistorySearchCursor {
            line_id: 3,
            byte_offset: 6,
        });
        assert!(matches!(
            index.search_history(&request),
            Err(HistorySearchError::InvalidCursor(3))
        ));
    }

    #[test]
    fn cumulative_page_byte_limit_returns_a_cursor_instead_of_skipping_rows() {
        let directory = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::create(
            directory.path().join("multi-page-search.csjr"),
            Durability::Ephemeral,
        )
        .unwrap();
        let mut payload = Vec::new();
        for line in 1..=3000 {
            if line == 2500 {
                payload.extend_from_slice(b"needle");
            }
            payload.extend(std::iter::repeat_n(b'x', 2048));
            payload.push(b'\n');
        }
        writer.append(&payload).unwrap();
        let index = writer.line_index();
        let mut request = HistorySearchRequest::forward("needle");
        request.max_scan_lines = 3000;

        let first = index.search_history(&request).unwrap();
        assert!(first.matches.is_empty());
        assert!(first.scanned_lines < 2500);
        assert!(!first.incomplete);
        assert!(first.next_cursor.is_some());

        request.cursor = first.next_cursor;
        let second = index.search_history(&request).unwrap();
        assert_eq!(second.matches.len(), 1);
        assert_eq!(second.matches[0].line_id, 2500);
    }

    #[test]
    fn whole_word_and_unicode_case_match_current_terminal_semantics() {
        let (_directory, index) = history();
        let mut request = HistorySearchRequest::forward("error");
        request.options.whole_word = true;
        let result = index.search_history(&request).unwrap();
        assert_eq!(
            result
                .matches
                .iter()
                .map(|item| item.line_id)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
