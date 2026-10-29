//! Optional semantic expansion; ordinary double-click keeps punctuation boundaries.
use super::*;

pub(super) fn expand(
    selection: TerminalSelection,
    snapshot: &cshell_terminal::FrameSnapshot,
    settings: &terminal_clipboard_settings::Settings,
) -> TerminalSelection {
    let ordinary = selection.expanded_with_delimiters(snapshot, &settings.word_delimiters);
    if selection.mode != TerminalSelectionMode::Word || !settings.smart_selection {
        return ordinary;
    }
    let at = |point| {
        token(snapshot, point).unwrap_or_else(|| {
            TerminalSelection {
                anchor: point,
                focus: point,
                mode: TerminalSelectionMode::Word,
            }
            .expanded_with_delimiters(snapshot, &settings.word_delimiters)
        })
    };
    let first = at(selection.anchor);
    let last = at(selection.focus);
    let key = |p: TerminalCellPoint| (p.row, p.column);
    let anchor = if key(first.anchor) < key(last.anchor) {
        first.anchor
    } else {
        last.anchor
    };
    let focus = if key(first.focus) > key(last.focus) {
        first.focus
    } else {
        last.focus
    };
    TerminalSelection {
        anchor,
        focus,
        mode: TerminalSelectionMode::Character,
    }
}

fn token(
    snapshot: &cshell_terminal::FrameSnapshot,
    point: TerminalCellPoint,
) -> Option<TerminalSelection> {
    let cell = snapshot.row(point.row)?.get(usize::from(point.column))?;
    if cell.character.is_whitespace() {
        return None;
    }
    let candidate = TerminalSelection {
        anchor: point,
        focus: point,
        mode: TerminalSelectionMode::Word,
    }
    .expanded_with_delimiters(snapshot, "\"'`<>{}()|,;：，；（）【】「」『』“”‘’");
    let text = candidate.text(snapshot).ok()?;
    let address = text.parse::<std::net::IpAddr>().is_ok()
        || text.parse::<std::net::SocketAddr>().is_ok()
        || text
            .strip_prefix('[')
            .and_then(|t| t.strip_suffix(']'))
            .is_some_and(|t| t.parse::<std::net::Ipv6Addr>().is_ok());
    let url = ["https://", "http://", "ssh://", "ftp://", "file://"]
        .iter()
        .any(|prefix| text.starts_with(prefix) && text.len() > prefix.len());
    let path = ["/", "./", "../", "~/"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
        || (text.as_bytes().get(1) == Some(&b':')
            && text.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && matches!(text.as_bytes().get(2), Some(b'/' | b'\\')));
    (address || url || path).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};

    fn selected(text: &str, column: u16, smart_selection: bool) -> String {
        let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(2, 100));
        terminal.feed(text.as_bytes());
        let point = TerminalCellPoint { row: 0, column };
        expand(
            TerminalSelection {
                anchor: point,
                focus: point,
                mode: TerminalSelectionMode::Word,
            },
            &terminal.snapshot(),
            &terminal_clipboard_settings::Settings {
                smart_selection,
                ..Default::default()
            },
        )
        .text(&terminal.snapshot())
        .unwrap()
    }

    #[test]
    fn ordinary_json_boundaries_are_kept_even_with_smart_selection() {
        for smart in [false, true] {
            for (text, column, expected) in [
                ("\"uid\": 123456", 9, "123456"),
                ("\"uid\":123456", 8, "123456"),
                ("\"uid\": 123456", 2, "uid"),
                ("\"uid\": 123456", 5, ":"),
                ("\"uid\": 123456", 6, " "),
                ("uid：123456，", 7, "123456"),
            ] {
                assert_eq!(
                    selected(text, column, smart),
                    expected,
                    "{text}, smart={smart}"
                );
            }
        }
    }

    #[test]
    fn smart_addresses_and_paths_are_opt_in() {
        for text in [
            "https://example.org:8080/a?x=1&y=2",
            "ssh://user@host:22/path",
            "2001:db8::1",
            "[2001:db8::1]:22",
            "127.0.0.1:8080",
            "/var/log/app.log",
            "C:\\logs\\app.log",
        ] {
            assert_eq!(selected(text, 2, true), text);
        }
        assert_eq!(selected("127.0.0.1:8080", 2, false), "127.0.0.1");
        assert_eq!(selected("/var/log/app.log", 7, false), "/var/log/app.log");
    }
}
