use cshell_domain::TerminalSize;
use cshell_terminal::{
    AlacrittyTerminalEngine, Cell, CellWidth, Color, CursorAppearance, CursorShape, FrameSnapshot,
    Style, TerminalEngine,
};

#[derive(Clone, Debug)]
struct CellSpec {
    row: u16,
    col: u16,
    text: &'static str,
    width: CellWidth,
    style: Style,
    hyperlink_uri: Option<&'static str>,
}

#[derive(Clone, Debug)]
struct CorpusCase {
    name: &'static str,
    size: TerminalSize,
    input: &'static str,
    cursor: (u16, u16),
    cells: Vec<CellSpec>,
}

fn spec(row: u16, col: u16, text: &'static str) -> CellSpec {
    CellSpec {
        row,
        col,
        text,
        width: CellWidth::Single,
        style: Style::default(),
        hyperlink_uri: None,
    }
}

fn indexed(foreground: u8, background: u8) -> Style {
    Style {
        foreground: Color::Indexed(foreground),
        background: Color::Indexed(background),
        ..Style::default()
    }
}

fn corpus() -> Vec<CorpusCase> {
    let mut decorated = indexed(1, 4);
    decorated.bold = true;
    decorated.italic = true;
    decorated.underline = true;
    let true_color = Style {
        foreground: Color::Rgb(1, 2, 3),
        background: Color::Indexed(200),
        ..Style::default()
    };
    let mut inverse = true_color;
    inverse.inverse = true;

    vec![
        CorpusCase {
            name: "sgr-semicolon-and-colon-colors",
            size: TerminalSize::cells(2, 12),
            input: "\x1b[31;44;1;3;4mA\x1b[0mB\x1b[38:2::1:2:3;48:5:200mC\x1b[7mD",
            cursor: (0, 4),
            cells: vec![
                CellSpec {
                    style: decorated,
                    ..spec(0, 0, "A")
                },
                spec(0, 1, "B"),
                CellSpec {
                    style: true_color,
                    ..spec(0, 2, "C")
                },
                CellSpec {
                    style: inverse,
                    ..spec(0, 3, "D")
                },
            ],
        },
        CorpusCase {
            name: "wide-and-combining-cells",
            size: TerminalSize::cells(2, 10),
            input: "你e\u{301}",
            cursor: (0, 3),
            cells: vec![
                CellSpec {
                    width: CellWidth::Wide,
                    ..spec(0, 0, "你")
                },
                CellSpec {
                    width: CellWidth::WideSpacer,
                    ..spec(0, 1, " ")
                },
                spec(0, 2, "e\u{301}"),
            ],
        },
        CorpusCase {
            name: "alternate-screen-is-isolated",
            size: TerminalSize::cells(2, 10),
            input: "MAIN\x1b[?1049hALT",
            cursor: (0, 7),
            cells: vec![spec(0, 4, "A"), spec(0, 5, "L"), spec(0, 6, "T")],
        },
        CorpusCase {
            name: "primary-screen-and-cursor-are-restored",
            size: TerminalSize::cells(2, 10),
            input: "MAIN\x1b[?1049hALT\x1b[?1049l",
            cursor: (0, 4),
            cells: vec![
                spec(0, 0, "M"),
                spec(0, 1, "A"),
                spec(0, 2, "I"),
                spec(0, 3, "N"),
            ],
        },
        CorpusCase {
            name: "full-screen-scroll-keeps-visible-tail",
            size: TerminalSize::cells(3, 6),
            input: "one\r\ntwo\r\nthree\r\nfour",
            cursor: (2, 4),
            cells: vec![
                spec(0, 0, "t"),
                spec(0, 1, "w"),
                spec(0, 2, "o"),
                spec(1, 0, "t"),
                spec(1, 1, "h"),
                spec(1, 2, "r"),
                spec(1, 3, "e"),
                spec(1, 4, "e"),
                spec(2, 0, "f"),
                spec(2, 1, "o"),
                spec(2, 2, "u"),
                spec(2, 3, "r"),
            ],
        },
        CorpusCase {
            name: "cursor-motion-and-erase-in-line",
            size: TerminalSize::cells(2, 8),
            input: "abcdef\x1b[3D\x1b[KZ",
            cursor: (0, 4),
            cells: vec![
                spec(0, 0, "a"),
                spec(0, 1, "b"),
                spec(0, 2, "c"),
                spec(0, 3, "Z"),
            ],
        },
        CorpusCase {
            name: "scrolling-region-preserves-outside-rows",
            size: TerminalSize::cells(4, 6),
            input: "A\r\nB\r\nC\r\nD\x1b[2;4r\x1b[4;1H\nX",
            cursor: (3, 1),
            cells: vec![
                spec(0, 0, "A"),
                spec(1, 0, "C"),
                spec(2, 0, "D"),
                spec(3, 0, "X"),
            ],
        },
        CorpusCase {
            name: "insert-and-delete-characters",
            size: TerminalSize::cells(2, 10),
            input: "abcdef\x1b[3G\x1b[2@XY\x1b[2P",
            cursor: (0, 4),
            cells: vec![
                spec(0, 0, "a"),
                spec(0, 1, "b"),
                spec(0, 2, "X"),
                spec(0, 3, "Y"),
                spec(0, 4, "e"),
                spec(0, 5, "f"),
            ],
        },
        CorpusCase {
            name: "insert-and-delete-lines",
            size: TerminalSize::cells(4, 6),
            input: "111\x1b[2;1H222\x1b[3;1H333\x1b[4;1H444\x1b[2;1H\x1b[L\x1b[3;1H\x1b[M",
            cursor: (2, 0),
            cells: vec![
                spec(0, 0, "1"),
                spec(0, 1, "1"),
                spec(0, 2, "1"),
                spec(2, 0, "3"),
                spec(2, 1, "3"),
                spec(2, 2, "3"),
            ],
        },
        CorpusCase {
            name: "origin-mode-addresses-relative-to-scroll-region",
            size: TerminalSize::cells(4, 6),
            input: "\x1b[2;4r\x1b[?6h\x1b[1;1HO",
            cursor: (1, 1),
            cells: vec![spec(1, 0, "O")],
        },
        CorpusCase {
            name: "default-tab-stops",
            size: TerminalSize::cells(2, 12),
            input: "A\tB",
            cursor: (0, 9),
            cells: vec![spec(0, 0, "A"), spec(0, 8, "B")],
        },
        CorpusCase {
            name: "delayed-wrap-moves-next-character",
            size: TerminalSize::cells(2, 4),
            input: "ABCDE",
            cursor: (1, 1),
            cells: vec![
                spec(0, 0, "A"),
                spec(0, 1, "B"),
                spec(0, 2, "C"),
                spec(0, 3, "D"),
                spec(1, 0, "E"),
            ],
        },
        CorpusCase {
            name: "osc-dynamic-indexed-color",
            size: TerminalSize::cells(2, 8),
            input: "\x1b]4;1;rgb:01/02/03\x07\x1b[31mA",
            cursor: (0, 1),
            cells: vec![CellSpec {
                style: Style {
                    foreground: Color::Rgb(1, 2, 3),
                    ..Style::default()
                },
                ..spec(0, 0, "A")
            }],
        },
        CorpusCase {
            name: "osc-8-hyperlink-boundary",
            size: TerminalSize::cells(2, 8),
            input: "\x1b]8;id=docs;https://example.test/a\x1b\\A\x1b]8;;\x1b\\B",
            cursor: (0, 2),
            cells: vec![
                CellSpec {
                    hyperlink_uri: Some("https://example.test/a"),
                    ..spec(0, 0, "A")
                },
                spec(0, 1, "B"),
            ],
        },
    ]
}

fn expected_cell(spec: &CellSpec) -> Cell {
    let mut characters = spec.text.chars();
    let character = characters.next().unwrap_or(' ');
    let mut cell = Cell::with_zerowidth(character, characters, spec.width, spec.style);
    if let Some(uri) = spec.hyperlink_uri {
        cell = cell.with_hyperlink_uri(uri);
    }
    cell
}

fn assert_case(case: &CorpusCase, snapshot: &FrameSnapshot) {
    assert_eq!(
        (snapshot.cursor_row, snapshot.cursor_col),
        case.cursor,
        "{} cursor",
        case.name
    );
    let mut expected = vec![Cell::default(); snapshot.cells.len()];
    for cell in &case.cells {
        let index = usize::from(cell.row) * usize::from(snapshot.cols) + usize::from(cell.col);
        expected[index] = expected_cell(cell);
    }
    assert_eq!(snapshot.cells, expected, "{} cells", case.name);
}

#[test]
fn xterm_cell_golden_corpus_survives_fragmented_input() {
    const CHUNK_PATTERN: [usize; 7] = [1, 2, 5, 3, 8, 1, 13];

    for case in corpus() {
        let mut terminal = AlacrittyTerminalEngine::new(case.size);
        let bytes = case.input.as_bytes();
        let mut offset = 0;
        let mut chunk = 0;
        while offset < bytes.len() {
            let end = (offset + CHUNK_PATTERN[chunk % CHUNK_PATTERN.len()]).min(bytes.len());
            terminal.feed(&bytes[offset..end]);
            offset = end;
            chunk += 1;
        }
        assert_case(&case, &terminal.snapshot());
    }
}

#[test]
fn xterm_cursor_appearance_survives_fragmented_input() {
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
        terminal.snapshot().cursor_appearance.shape,
        CursorShape::Hidden
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
