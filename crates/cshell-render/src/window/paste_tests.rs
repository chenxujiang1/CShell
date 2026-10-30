use super::*;
use cshell_domain::TerminalSize;
use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};

fn themes() -> [TerminalTheme; 3] {
    [
        TerminalTheme::Dark,
        TerminalTheme::Light,
        TerminalTheme::Custom(cshell_domain::TerminalPalette {
            foreground: [220, 210, 200],
            background: [12, 22, 32],
            cursor: [140, 150, 160],
            selection: [44, 54, 64],
            ansi: [[70, 80, 90]; 16],
        }),
    ]
}

fn glyphs(vertices: &[Vertex]) -> Vec<&Vertex> {
    let white = white_uv();
    vertices
        .iter()
        .filter(|vertex| vertex.uv != [white[0], white[1]])
        .collect()
}

#[test]
fn highlighted_paste_is_visible_before_enter_in_all_themes() {
    let mut atlas = GlyphAtlas::build().unwrap();
    let viewport = TerminalViewport {
        x: 0,
        y: 0,
        width: 1000,
        height: 100,
    };
    for theme in themes() {
        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 80));
        // Readline paints the bracketed-paste region with SGR 7. No Enter or
        // command output is needed to reproduce the invisible glyphs.
        terminal.feed(b"\x1b[7mprintf CSHELL_PASTE_VISIBLE\x1b[27m");
        let snapshot = terminal.snapshot();
        assert!(snapshot.cells[0].style.inverse);
        let vertices = build_vertices_themed(
            &snapshot,
            &mut atlas,
            1000,
            100,
            viewport,
            0..2,
            &TerminalDecorations::default(),
            false,
            theme,
        );
        let ink = glyphs(&vertices);
        assert!(!ink.is_empty());
        assert!(
            ink.iter().all(|vertex| vertex.color == theme.background()),
            "inverse paste glyphs must be opaque and use the theme background: {theme:?}"
        );
        assert!(ink.iter().all(|vertex| vertex.color[3] == 1.0));
    }
}

#[test]
fn inverse_paste_history_is_visible_in_all_themes() {
    let mut atlas = GlyphAtlas::build().unwrap();
    let viewport = TerminalViewport {
        x: 0,
        y: 0,
        width: 1000,
        height: 100,
    };
    let text = "printf PASTE_HISTORY";
    let mut surface = crate::LogSurfaceModel::default();
    surface
        .submit_page(Arc::new(crate::LogPage {
            source_id: crate::LogSourceId(0),
            revision: 1,
            first_line_id: 1,
            anchor_line_id: 1,
            rows: Arc::from([crate::LogRow {
                line_id: 1,
                text: text.into(),
                soft_wrap: false,
                style_spans: Arc::from([crate::LogStyleSpan {
                    byte_range: 0..text.len() as u32,
                    style: Style {
                        inverse: true,
                        ..Default::default()
                    },
                }]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        }))
        .unwrap();
    let layout = surface.request_reflow(80).unwrap().execute().unwrap();
    surface.submit_reflow(layout);
    let frame = surface.prepare_frame(2).unwrap();
    for theme in themes() {
        let vertices = build_log_vertices_themed(
            &frame,
            &mut atlas,
            1000,
            100,
            viewport,
            &LogDecorations::default(),
            theme,
        );
        let ink = glyphs(&vertices);
        assert!(!ink.is_empty());
        assert!(
            ink.iter().all(|vertex| vertex.color == theme.background()),
            "{theme:?}"
        );
    }
}

#[test]
fn inverse_explicit_colors_and_sgr_reset_keep_their_original_meaning() {
    let mut atlas = GlyphAtlas::build().unwrap();
    let viewport = TerminalViewport {
        x: 0,
        y: 0,
        width: 1000,
        height: 100,
    };
    for theme in themes() {
        for (sequence, expected) in [
            (
                b"\x1b[38;2;90;100;110;48;2;20;30;40;7mX".as_slice(),
                linear_rgb([20, 30, 40]),
            ),
            (
                b"\x1b[7m\x1b[27mX".as_slice(),
                resolve_color_themed(Color::Default, true, theme),
            ),
        ] {
            let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 80));
            terminal.feed(sequence);
            let vertices = build_vertices_themed(
                &terminal.snapshot(),
                &mut atlas,
                1000,
                100,
                viewport,
                0..2,
                &TerminalDecorations::default(),
                false,
                theme,
            );
            let ink = glyphs(&vertices);
            assert!(!ink.is_empty());
            assert!(ink.iter().all(|vertex| vertex.color == expected));
        }
    }
}
