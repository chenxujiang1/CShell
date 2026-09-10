use cshell_domain::TerminalSize;
use cshell_render::{LogPage, LogRow, LogSourceId, LogStyleSpan};
use cshell_terminal::{AlacrittyTerminalEngine, Color, FrameSnapshot, Style, TerminalEngine};
use std::sync::Arc;

pub const ARGUMENT: &str = "--visual-corpus";
pub const LOG_ARGUMENT: &str = "--log-corpus";

const SOURCE: &str = include_str!("../../../tests/visual/terminal-unicode-v1.ansi.txt");

pub fn snapshot() -> FrameSnapshot {
    let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(30, 100));
    terminal.feed(&decoded_bytes());
    terminal.snapshot()
}

pub fn log_page() -> Arc<LogPage> {
    let rows: Arc<[LogRow]> = vec![
        log_row(
            1,
            "ASCII log alignment 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        ),
        log_row(2, "CJK 中文终端 日本語ログ 한국어 로그 檔案傳輸"),
        log_row(3, "NFD café naïve résumé | stack ā́ ê̈ ȭ"),
        log_row(4, "Emoji 😀 🚀 🔒 ⚠️ ✅ | sequences 👨‍💻 👩‍🚀 👍🏽"),
        LogRow {
            line_id: 5,
            text: Arc::from("red green underline inverse"),
            style_spans: Arc::from([
                LogStyleSpan {
                    byte_range: 0..3,
                    style: Style {
                        foreground: Color::Rgb(255, 64, 64),
                        ..Style::default()
                    },
                },
                LogStyleSpan {
                    byte_range: 4..9,
                    style: Style {
                        foreground: Color::Indexed(46),
                        ..Style::default()
                    },
                },
                LogStyleSpan {
                    byte_range: 10..19,
                    style: Style {
                        underline: true,
                        ..Style::default()
                    },
                },
                LogStyleSpan {
                    byte_range: 20..27,
                    style: Style {
                        inverse: true,
                        ..Style::default()
                    },
                },
            ]),
            truncated: false,
        },
        log_row(6, "Box ┌────────┬────────┐ ╔════════╦════════╗ ░▒▓█"),
    ]
    .into();
    Arc::new(LogPage {
        source_id: LogSourceId(1),
        revision: 1,
        anchor_line_id: 1,
        total_line_count: rows.len() as u64,
        rows,
        has_before: false,
        has_after: false,
    })
}

fn log_row(line_id: u64, text: &'static str) -> LogRow {
    LogRow {
        line_id,
        text: Arc::from(text),
        style_spans: Arc::from([]),
        truncated: false,
    }
}

fn decoded_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SOURCE.len());
    for line in SOURCE.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut remaining = line.as_bytes();
        while let Some(position) = remaining.windows(4).position(|window| window == b"\\x1b") {
            bytes.extend_from_slice(&remaining[..position]);
            bytes.push(0x1b);
            remaining = &remaining[position + 4..];
        }
        bytes.extend_from_slice(remaining);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{SOURCE, decoded_bytes, log_page, snapshot};
    use cshell_render::LogSurfaceModel;
    use cshell_terminal::{CellWidth, Color};

    #[test]
    fn corpus_is_reviewable_utf8_and_decodes_terminal_escapes() {
        assert!(SOURCE.contains("中文终端"));
        assert!(SOURCE.contains("👨‍💻"));
        let bytes = decoded_bytes();
        assert!(bytes.contains(&0x1b));
        assert!(!bytes.windows(4).any(|window| window == b"\\x1b"));
    }

    #[test]
    fn corpus_snapshot_contains_unicode_width_combining_and_true_color_cells() {
        let snapshot = snapshot();
        assert_eq!((snapshot.rows, snapshot.cols), (30, 100));
        assert!(
            snapshot
                .cells
                .iter()
                .any(|cell| cell.width == CellWidth::Wide)
        );
        assert!(
            snapshot
                .cells
                .iter()
                .any(|cell| !cell.zerowidth().is_empty())
        );
        assert!(
            snapshot
                .cells
                .iter()
                .any(|cell| matches!(cell.style.foreground, Color::Rgb(255, 80, 80)))
        );
    }

    #[test]
    fn log_corpus_is_a_valid_styled_page() {
        let mut surface = LogSurfaceModel::default();
        assert!(
            surface
                .submit_page(log_page())
                .unwrap_or_else(|error| panic!("{error}"))
        );
        let frame = surface
            .prepare_frame(20)
            .unwrap_or_else(|| panic!("log corpus should prepare a frame"));
        assert_eq!(frame.page.rows.len(), 6);
        assert_eq!(frame.page.rows[4].style_spans.len(), 4);
    }
}
