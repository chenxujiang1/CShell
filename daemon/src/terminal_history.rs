use cshell_ipc::{LogPage, LogPageRequest, LogRow, LogStyleSpan, TerminalStyle};
use cshell_terminal::{CellWidth, HistorySlice, Style};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) fn page(slice: HistorySlice, request: &LogPageRequest) -> LogPage {
    let total = slice.total;
    let first_retained = slice.first_line;
    let anchor = slice.anchor;
    let generation = slice.generation;
    // Budget each selected row, guaranteeing that the requested anchor survives
    // dense combining characters and alternating styles. Simplify extras, never
    // drop the tail/anchor merely because earlier rows exhausted style storage.
    let per_row_text = (1024 * 1024 / slice.len().max(1)).max(4096);
    let per_row_spans = cshell_ipc::MAX_LOG_PAGE_STYLE_SPANS / slice.len().max(1);
    let mut result = Vec::new();
    for (line_id, cells) in slice.into_rows() {
        let soft_wrap = cells.iter().any(cshell_terminal::Cell::soft_wrap);
        let mut truncated = false;
        let mut text = String::new();
        let mut styles = Vec::new();
        let mut remaining_base = cells
            .iter()
            .filter(|cell| {
                !matches!(
                    cell.width,
                    CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
                )
            })
            .count()
            * 4;
        for cell in cells {
            if matches!(
                cell.width,
                CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
            ) {
                continue;
            }
            let start = text.len();
            remaining_base = remaining_base.saturating_sub(4);
            for (index, character) in cell.characters().filter(|c| !c.is_control()).enumerate() {
                if index == 0 || text.len() + character.len_utf8() + remaining_base <= per_row_text
                {
                    text.push(character);
                } else {
                    truncated = true;
                }
            }
            styles.push((start, text.len(), cell.style));
        }
        let text = text.trim_end_matches(' ').to_owned();
        let mut style_spans: Vec<LogStyleSpan> = Vec::new();
        let mut style_index = 0;
        for (start, grapheme) in text.grapheme_indices(true) {
            while style_index + 1 < styles.len() && styles[style_index].1 <= start {
                style_index += 1;
            }
            let style = styles.get(style_index).map_or(Style::default(), |s| s.2);
            if style == Style::default() {
                continue;
            }
            let style = Some(TerminalStyle::from(style));
            let end = (start + grapheme.len()) as u32;
            if let Some(last) = style_spans.last_mut()
                && last.end == start as u32
                && last.style == style
            {
                last.end = end;
            } else {
                style_spans.push(LogStyleSpan {
                    start: start as u32,
                    end,
                    style,
                });
                if style_spans.len() > per_row_spans {
                    break;
                }
            }
        }
        if style_spans.len() > per_row_spans {
            style_spans = Vec::new();
            truncated = true;
        }
        result.push(LogRow {
            soft_wrap,
            line_id,
            text,
            style_spans,
            truncated,
        });
    }
    let first = result.first().map_or(0, |r| r.line_id);
    let last = result.last().map_or(0, |r| r.line_id);
    LogPage {
        first_line_id: first_retained,
        session_id: request.session_id.clone(),
        revision: generation,
        anchor_line_id: if result.is_empty() {
            0
        } else {
            anchor.clamp(first, last)
        },
        has_before: first > first_retained,
        has_after: last < total,
        total_line_count: total,
        rows: result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TerminalPipeline;
    use cshell_domain::{SessionId, TerminalSize};
    #[test]
    fn nonrecorded_history_survives_worker_exit_and_validates_without_disk_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disabled.csjr");
        let pipeline =
            TerminalPipeline::spawn_configured(&path, TerminalSize::cells(4, 40), 8, None, false)
                .unwrap();
        let snapshots = pipeline.snapshots();
        pipeline
            .try_submit(
                "first-command\r\n\x1b[31m你好 e\u{301}\x1b[0m\r\n"
                    .as_bytes()
                    .to_vec(),
            )
            .unwrap();
        pipeline
            .try_submit(
                (0..100)
                    .map(|i| format!("line {i}\r\n"))
                    .collect::<String>()
                    .into_bytes(),
            )
            .unwrap();
        snapshots
            .wait_for_generation(2, std::time::Duration::from_secs(2))
            .unwrap();
        pipeline.shutdown().unwrap();
        let page = snapshots
            .history_page(&LogPageRequest {
                terminal_history: true,
                session_id: SessionId::new().as_uuid().as_bytes().to_vec(),
                anchor_line_id: Some(1),
                rows_before: 0,
                rows_after: 10,
                cell_offset: 0,
            })
            .unwrap();
        page.validate().unwrap();
        assert!(page.rows[0].text.contains("first-command"));
        assert!(page.rows[1].text.contains("你好 e\u{301}"));
        assert!(!page.rows[1].style_spans.is_empty());
        assert!(page.has_after);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[cfg(test)]
mod dense_page_tests {
    use super::*;
    use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};
    #[test]
    fn dense_styles_and_combining_marks_keep_requested_anchor_and_text() {
        let mut terminal =
            AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(24, 240));
        let row = (0..239)
            .map(|i| format!("\x1b[3{}mx{}", i % 7, "\u{301}".repeat(64)))
            .collect::<String>()
            + "\r\n";
        for _ in 0..300 {
            terminal.feed(row.as_bytes());
        }
        let request = LogPageRequest {
            terminal_history: true,
            session_id: cshell_domain::SessionId::new()
                .as_uuid()
                .as_bytes()
                .to_vec(),
            anchor_line_id: None,
            rows_before: 4096,
            rows_after: 0,
            cell_offset: 0,
        };
        let slice = terminal.history_slice(None, 4096, 0);
        let anchor = slice.anchor;
        let page = page(slice, &request);
        page.validate().unwrap();
        assert!(page.rows.iter().any(|row| row.line_id == anchor));
        assert!(page.rows.iter().map(|row| row.text.len()).sum::<usize>() <= 2 * 1024 * 1024);
        assert!(
            page.rows
                .iter()
                .map(|row| row.style_spans.len())
                .sum::<usize>()
                <= cshell_ipc::MAX_LOG_PAGE_STYLE_SPANS
        );
        assert!(page.rows.iter().any(|row| row.truncated));
        assert_eq!(
            page.rows.iter().filter(|row| !row.text.is_empty()).count(),
            page.rows.len() - 1
        );
    }
}
