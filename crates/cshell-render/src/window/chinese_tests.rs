#![allow(clippy::unwrap_used)]
use super::*;
use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};

// Keep exactly the fonts supplied by the application, so developer/CI system
// fonts cannot hide missing CJK coverage on another user's Mac.
fn without_system_fonts(atlas: &mut GlyphAtlas) {
    let mut db = atlas.font_system.db().clone();
    let external: Vec<_> = db
        .faces()
        .filter(|face| !matches!(face.source, fontdb::Source::Binary(_)))
        .map(|face| face.id)
        .collect();
    for id in external {
        db.remove_face(id);
    }
    atlas.font_system = FontSystem::new_with_locale_and_db("zh-CN".into(), db);
    atlas.shaped_cells.clear();
}

#[test]
fn ssh_timeout_chinese_has_real_glyphs_without_system_fonts() {
    let text = "等待输入超时：自动登出";
    let mut terminal = AlacrittyTerminalEngine::new(cshell_domain::TerminalSize::cells(2, 64));
    // Exercise the SSH/PTY stream boundary, including every split UTF-8 byte.
    for byte in text.as_bytes() {
        terminal.feed(std::slice::from_ref(byte));
    }
    let snapshot = terminal.snapshot();
    let cells: Vec<_> = snapshot
        .row(0)
        .unwrap()
        .iter()
        .filter(|cell| cell.width != CellWidth::WideSpacer && cell.character != ' ')
        .collect();
    assert_eq!(
        cells.iter().map(|cell| cell.character).collect::<String>(),
        text
    );

    for size in [12, cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS, 40] {
        let mut atlas = GlyphAtlas::build_at_size(size).unwrap();
        without_system_fonts(&mut atlas);
        for cell in &cells {
            let glyphs = atlas.shape_cell(cell);
            let shaped = atlas
                .shaped_cells
                .get(&ShapeKey {
                    text: cell.character.to_string(),
                    columns: 2,
                    bold: false,
                    italic: false,
                })
                .unwrap();
            assert!(
                !shaped.is_empty(),
                "no shaped glyph for {} at {size}px",
                cell.character
            );
            for glyph in shaped {
                assert_ne!(
                    glyph.cache_key.glyph_id, 0,
                    "missing glyph for {} at {size}px",
                    cell.character
                );
                let face = atlas
                    .font_system
                    .db()
                    .face(glyph.cache_key.font_id)
                    .unwrap();
                assert!(
                    face.families
                        .iter()
                        .any(|(name, _)| name.starts_with("Noto Sans SC")),
                    "non-CJK fallback for {}",
                    cell.character
                );
                assert!(
                    atlas.dynamic_glyphs.contains_key(&glyph.cache_key),
                    "unrasterized glyph for {}",
                    cell.character
                );
            }
            assert!(!glyphs.is_empty());
            assert!(
                glyphs.iter().all(|glyph| *glyph != atlas.glyph('?')),
                "question-mark substitution for {}",
                cell.character
            );
        }
    }
}

#[test]
fn styled_chinese_rerasterizes_after_atlas_eviction_without_question_marks() {
    let mut atlas = GlyphAtlas::build().unwrap();
    without_system_fonts(&mut atlas);
    for bold in [false, true] {
        for italic in [false, true] {
            let cell = cshell_terminal::Cell::new(
                '等',
                CellWidth::Wide,
                Style {
                    bold,
                    italic,
                    ..Style::default()
                },
            );
            let first = atlas.shape_cell(&cell);
            assert!(first.iter().all(|glyph| *glyph != atlas.glyph('?')));
            let key = ShapeKey {
                text: "等".into(),
                columns: 2,
                bold,
                italic,
            };
            let cache_keys: Vec<_> = atlas.shaped_cells[&key]
                .iter()
                .map(|glyph| glyph.cache_key)
                .collect();
            assert!(!cache_keys.is_empty());
            for key in cache_keys {
                assert_ne!(key.glyph_id, 0);
                let cached = atlas.dynamic_glyphs.remove(&key).unwrap();
                atlas.dynamic_allocator.deallocate(cached.allocation_id);
            }
            let revision = atlas.revision;
            let redrawn = atlas.shape_cell(&cell);
            assert!(atlas.revision > revision);
            assert!(redrawn.iter().all(|glyph| *glyph != atlas.glyph('?')));
        }
    }
}
