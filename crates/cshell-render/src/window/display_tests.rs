use super::*;
use cshell_domain::TerminalSize;
use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};

fn snapshot(output: &str) -> FrameSnapshot {
    let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 8));
    terminal.feed(output.as_bytes());
    terminal.snapshot()
}

fn geometry(
    snapshot: &FrameSnapshot,
    atlas: &mut GlyphAtlas,
    decorations: &TerminalDecorations,
    theme: TerminalTheme,
) -> Vec<Vertex> {
    build_vertices_themed(
        snapshot,
        atlas,
        256,
        128,
        TerminalViewport {
            x: 0,
            y: 0,
            width: 256,
            height: 128,
        },
        0..2,
        decorations,
        true,
        theme,
    )
}

#[test]
fn wide_glyph_ink_is_not_painted_over_by_spacer_backgrounds_or_selection() {
    let mut atlas = GlyphAtlas::build().unwrap();
    let white = white_uv();
    for output in [
        "\x1b[?25l\x1b[48;2;0;0;0m中文",
        "\x1b[?25l\x1b[7m中文",
        "\x1b[?25l中文",
    ] {
        let snapshot = snapshot(output);
        let decorations = TerminalDecorations {
            selection: Some(crate::TerminalSelection {
                anchor: crate::TerminalCellPoint { row: 0, column: 0 },
                focus: crate::TerminalCellPoint { row: 0, column: 3 },
                mode: crate::TerminalSelectionMode::Character,
            }),
            ..Default::default()
        };
        let vertices = geometry(&snapshot, &mut atlas, &decorations, TerminalTheme::Dark);
        let mut ink = Vec::<&[Vertex]>::new();
        for quad in vertices.chunks_exact(6) {
            if quad.iter().any(|v| v.uv != [white[0], white[1]]) {
                ink.push(quad);
            } else {
                for glyph in &ink {
                    let overlaps = glyph[0].position[0] < quad[2].position[0]
                        && quad[0].position[0] < glyph[2].position[0]
                        && glyph[2].position[1] < quad[0].position[1]
                        && quad[2].position[1] < glyph[0].position[1];
                    assert!(
                        !overlaps,
                        "later background covers a previously drawn wide glyph: {output:?}"
                    );
                }
            }
        }
        assert!(ink.len() >= 2);
    }
}

#[test]
fn light_theme_block_cursor_keeps_the_character_readable() {
    let mut atlas = GlyphAtlas::build().unwrap();
    let snapshot = snapshot("X\r");
    let vertices = geometry(
        &snapshot,
        &mut atlas,
        &TerminalDecorations::default(),
        TerminalTheme::Light,
    );
    let white = white_uv();
    let ink = vertices
        .iter()
        .find(|v| v.uv != [white[0], white[1]])
        .unwrap();
    let background = vertices[0].color;
    let luminance = |c: [f32; 4]| c[0] * 0.2126 + c[1] * 0.7152 + c[2] * 0.0722;
    let (a, b) = (luminance(ink.color), luminance(background));
    let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
    assert!(
        contrast >= 4.5,
        "cursor hides its character: contrast {contrast}"
    );
}

#[test]
fn wide_character_cursor_covers_both_columns_even_when_addressed_at_the_spacer() {
    let mut atlas = GlyphAtlas::build().unwrap();
    for shape in [
        CursorShape::Block,
        CursorShape::Underline,
        CursorShape::HollowBlock,
    ] {
        for cursor_col in [0, 1] {
            let mut snapshot = snapshot("中\r");
            snapshot.cursor_col = cursor_col;
            snapshot.cursor_appearance.shape = shape;
            snapshot.cursor_appearance.color = Some(Color::Rgb(0, 255, 0));
            let vertices = geometry(
                &snapshot,
                &mut atlas,
                &TerminalDecorations::default(),
                TerminalTheme::Dark,
            );
            let cursor: Vec<_> = vertices
                .iter()
                .filter(|v| v.color == [0.0, 1.0, 0.0, 1.0])
                .collect();
            assert!(!cursor.is_empty());
            let min = cursor
                .iter()
                .map(|v| v.position[0])
                .fold(f32::INFINITY, f32::min);
            let max = cursor
                .iter()
                .map(|v| v.position[0])
                .fold(f32::NEG_INFINITY, f32::max);
            assert_eq!(
                min, -1.0,
                "cursor must start at the wide glyph for {shape:?}"
            );
            assert!(
                (max - min - atlas.cell_width * 4.0 / 256.0).abs() < 0.0001,
                "cursor covers only part of the wide glyph for {shape:?}"
            );
        }
    }
}

fn history_frame(text: &str) -> LogSurfaceFrame {
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
                        italic: true,
                        background: Color::Rgb(0, 0, 0),
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
    let layout = surface.request_reflow(8).unwrap().execute().unwrap();
    surface.submit_reflow(layout);
    surface.prepare_frame(2).unwrap()
}

fn matching_history(snapshot: &FrameSnapshot) -> LogSurfaceFrame {
    let mut text = String::new();
    let mut spans = Vec::new();
    for cell in snapshot.row(0).unwrap() {
        if matches!(
            cell.width,
            CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
        ) {
            continue;
        }
        let start = text.len() as u32;
        text.extend(cell.characters());
        spans.push(crate::LogStyleSpan {
            byte_range: start..text.len() as u32,
            style: cell.style,
        });
    }
    let mut model = crate::LogSurfaceModel::default();
    model
        .submit_page(Arc::new(crate::LogPage {
            source_id: crate::LogSourceId(0),
            revision: 1,
            first_line_id: 1,
            anchor_line_id: 1,
            rows: Arc::from([crate::LogRow {
                line_id: 1,
                text: text.into(),
                soft_wrap: false,
                style_spans: spans.into(),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        }))
        .unwrap();
    let layout = model
        .request_reflow(snapshot.cols)
        .unwrap()
        .execute()
        .unwrap();
    model.submit_reflow(layout);
    model.prepare_frame(2).unwrap()
}

fn ink(vertices: &[Vertex]) -> Vec<Vertex> {
    let white = white_uv();
    vertices
        .chunks_exact(6)
        .filter(|q| q.iter().any(|v| v.uv != [white[0], white[1]]))
        .flatten()
        .copied()
        .collect()
}

#[test]
fn live_and_history_selection_keep_identical_glyphs_metrics_and_ansi_ink() {
    for pixels in [12, 18, 28, 40] {
        let mut atlas = GlyphAtlas::build_at_size(pixels).unwrap();
        for output in [
            "ABC xyz 0123 /var/log",
            "\x1b[31mRED \x1b[1mBold\x1b[0m plain",
            "中文 e\u{301} \x1b[3mItalic\x1b[0m",
        ] {
            let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(2, 32));
            terminal.feed(format!("\x1b[?25l{output}").as_bytes());
            let snapshot = terminal.snapshot();
            let history = matching_history(&snapshot);
            let viewport = TerminalViewport {
                x: 0,
                y: 0,
                width: 1600,
                height: 160,
            };
            for theme in [TerminalTheme::Dark, TerminalTheme::Light] {
                let plain = build_vertices_themed(
                    &snapshot,
                    &mut atlas,
                    1600,
                    160,
                    viewport,
                    0..2,
                    &TerminalDecorations::default(),
                    false,
                    theme,
                );
                let expected = ink(&plain);
                assert!(!expected.is_empty());
                let selected = crate::TerminalSelection {
                    anchor: crate::TerminalCellPoint { row: 0, column: 0 },
                    focus: crate::TerminalCellPoint { row: 0, column: 31 },
                    mode: crate::TerminalSelectionMode::Character,
                };
                for selection in [false, true, false] {
                    let live = build_vertices_themed(
                        &snapshot,
                        &mut atlas,
                        1600,
                        160,
                        viewport,
                        0..2,
                        &TerminalDecorations {
                            selection: selection.then_some(selected),
                            ..Default::default()
                        },
                        false,
                        theme,
                    );
                    let log = build_log_vertices_themed(
                        &history,
                        &mut atlas,
                        1600,
                        160,
                        viewport,
                        &LogDecorations {
                            selection_ranges: if selection {
                                Arc::from([crate::LogSearchMatch {
                                    line_id: 1,
                                    byte_range: 0..history.page.rows[0].text.len() as u32,
                                }])
                            } else {
                                Arc::from([])
                            },
                            ..Default::default()
                        },
                        theme,
                    );
                    assert_eq!(
                        bytemuck::cast_slice::<Vertex, u8>(&expected),
                        bytemuck::cast_slice::<Vertex, u8>(&ink(&live)),
                        "live selection changed ink: {pixels}px {output:?}"
                    );
                    assert_eq!(
                        bytemuck::cast_slice::<Vertex, u8>(&expected),
                        bytemuck::cast_slice::<Vertex, u8>(&ink(&log)),
                        "history changed font, baseline, or ANSI ink: {pixels}px {output:?}"
                    );
                    if selection {
                        assert!(log.iter().any(|v| v.color == theme.selection_color()));
                    }
                }
            }
        }
    }
}

#[test]
fn equal_history_ids_from_different_sessions_do_not_reuse_old_geometry() {
    let first = history_frame("first server");
    let second = history_frame("second server");
    assert_eq!(first.page.source_id, second.page.source_id);
    assert_eq!(first.page.revision, second.page.revision);
    assert_eq!(first.visible_rows, second.visible_rows);
    assert_eq!(first.layout_columns, second.layout_columns);
    let source = GeometrySource::Log(first.page.clone());
    assert!(source.matches_log(&first.clone()));
    assert!(!source.matches_log(&second));
    let mut atlas = GlyphAtlas::build().unwrap();
    let viewport = TerminalViewport {
        x: 0,
        y: 0,
        width: 256,
        height: 128,
    };
    let first_ink = build_log_vertices(
        &first,
        &mut atlas,
        256,
        128,
        viewport,
        &LogDecorations::default(),
    );
    let second_ink = build_log_vertices(
        &second,
        &mut atlas,
        256,
        128,
        viewport,
        &LogDecorations::default(),
    );
    assert_ne!(
        bytemuck::cast_slice::<Vertex, u8>(&first_ink),
        bytemuck::cast_slice::<Vertex, u8>(&second_ink)
    );
}

#[test]
fn history_backgrounds_precede_all_text_ink() {
    let frame = history_frame("中文fff");
    let mut atlas = GlyphAtlas::build().unwrap();
    let vertices = build_log_vertices(
        &frame,
        &mut atlas,
        256,
        128,
        TerminalViewport {
            x: 0,
            y: 0,
            width: 256,
            height: 128,
        },
        &LogDecorations::default(),
    );
    let white = white_uv();
    let mut saw_ink = false;
    for quad in vertices.chunks_exact(6) {
        let glyph = quad.iter().any(|v| v.uv != [white[0], white[1]]);
        assert!(
            glyph || !saw_ink,
            "history background can cover an earlier italic glyph"
        );
        saw_ink |= glyph;
    }
    assert!(saw_ink);
}

fn read_pixels(renderer: &HeadlessTerminalRenderer) -> Vec<u8> {
    let stride = (renderer.width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = renderer.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("display-regression-readback"),
        size: u64::from(stride) * u64::from(renderer.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = renderer.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        renderer.target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(renderer.height),
            },
        },
        wgpu::Extent3d {
            width: renderer.width,
            height: renderer.height,
            depth_or_array_layers: 1,
        },
    );
    renderer.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
    renderer
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(10)),
        })
        .unwrap();
    rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    let mapped = buffer.slice(..).get_mapped_range().unwrap();
    let pixels = mapped
        .chunks_exact(stride as usize)
        .flat_map(|row| row[..renderer.width as usize * 4].iter().copied())
        .collect();
    drop(mapped);
    buffer.unmap();
    pixels
}

#[tokio::test]
async fn gpu_wide_chinese_right_half_survives_an_opaque_background() {
    let mut renderer = match HeadlessTerminalRenderer::new(128, 64).await {
        Ok(renderer) => renderer,
        Err(WindowRendererError::Adapter(error))
            if std::env::var_os("CSHELL_REQUIRE_GPU").is_none() =>
        {
            eprintln!("no GPU adapter: {error}");
            return;
        }
        Err(error) => panic!("{error}"),
    };
    eprintln!(
        "display pixel readback adapter: {:?}",
        renderer.adapter_info()
    );
    let cell_width = renderer.atlas.cell_width as usize;
    let cell_height = renderer.atlas.cell_height as usize;
    let mut masks = Vec::new();
    for output in [
        "\x1b[?25l\x1b[38;2;255;255;255m中",
        "\x1b[?25l\x1b[38;2;255;255;255;48;2;0;0;0m中",
    ] {
        let mut surface = crate::TerminalSurfaceModel::default();
        surface.submit_snapshot(Arc::new(snapshot(output)));
        renderer
            .render_frame(&surface.prepare_frame(0, 2).unwrap())
            .unwrap();
        let pixels = read_pixels(&renderer);
        let mask: Vec<bool> = (0..cell_height)
            .flat_map(|y| (cell_width..cell_width * 2).map(move |x| (y * 128 + x) * 4))
            .map(|i| pixels[i] > 128 && pixels[i + 1] > 128 && pixels[i + 2] > 128)
            .collect();
        masks.push(mask);
    }
    assert!(
        masks[0].iter().any(|ink| *ink),
        "fixture must have ink in its right half"
    );
    assert_eq!(
        masks[0], masks[1],
        "opaque spacer background erased Chinese strokes"
    );
}
