use crate::{LogDecorations, LogSurfaceFrame, TerminalDecorations, TerminalSurfaceFrame};
use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style as FontStyle, SwashCache,
    SwashContent, Weight, Wrap, fontdb,
};
use cshell_terminal::{CellWidth, Color, CursorShape, FrameSnapshot, Style};
use guillotiere::{AllocId, AtlasAllocator, size2};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use winit::dpi::PhysicalSize;
use winit::window::Window;

const ATLAS_WIDTH: u32 = 2048;
const ATLAS_HEIGHT: u32 = 2048;
const GLYPH_TILE: u32 = 32;
const GLYPH_PADDING: u32 = 2;
const ATLAS_BYTES_PER_PIXEL: u32 = 4;
const FIRST_GLYPH: u32 = 32;
const LAST_GLYPH: u32 = 126;
const FONT_SIZE: f32 = 16.0;
const CELL_PADDING_X: f32 = 1.0;
const STATIC_GLYPH_COUNT: u32 = LAST_GLYPH - FIRST_GLYPH + 1;
const STATIC_ATLAS_HEIGHT: u32 = STATIC_GLYPH_COUNT
    .div_ceil(ATLAS_WIDTH / GLYPH_TILE)
    .saturating_mul(GLYPH_TILE);

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
    colored: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct GlyphInfo {
    atlas_x: u32,
    atlas_y: u32,
    width: u32,
    height: u32,
    offset_x: f32,
    offset_y: f32,
    colored: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AtlasDirtyRect {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AtlasUpload {
    origin: [u32; 2],
    size: [u32; 2],
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct CachedGlyph {
    glyph: GlyphInfo,
    allocation_id: AllocId,
    last_used: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ShapedGlyph {
    cache_key: cosmic_text::CacheKey,
    offset_x: f32,
    offset_y: f32,
}

struct GlyphAtlas {
    pixels: Vec<u8>,
    glyphs: Vec<GlyphInfo>,
    cell_width: f32,
    cell_height: f32,
    font_system: FontSystem,
    swash_cache: SwashCache,
    dynamic_glyphs: HashMap<cosmic_text::CacheKey, CachedGlyph>,
    shaped_cells: HashMap<ShapeKey, Vec<ShapedGlyph>>,
    dynamic_allocator: AtlasAllocator,
    pinned_glyphs: HashSet<cosmic_text::CacheKey>,
    use_clock: u64,
    evictions: u64,
    revision: u64,
    dirty: Option<AtlasDirtyRect>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ShapeKey {
    text: String,
    bold: bool,
    italic: bool,
    columns: u16,
}

impl GlyphAtlas {
    fn build() -> Result<Self, WindowRendererError> {
        let font = FontArc::try_from_slice(epaint_default_fonts::HACK_REGULAR)
            .map_err(|_| WindowRendererError::BundledFont)?;
        let scale = PxScale::from(FONT_SIZE);
        let scaled = font.as_scaled(scale);
        let ascent = scaled.ascent();
        let cell_width = scaled.h_advance(scaled.glyph_id('M')).ceil() + CELL_PADDING_X * 2.0;
        let cell_height = (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil() + 2.0;
        let mut pixels = vec![0_u8; (ATLAS_WIDTH * ATLAS_HEIGHT * ATLAS_BYTES_PER_PIXEL) as usize];
        pixels[..ATLAS_BYTES_PER_PIXEL as usize].fill(u8::MAX);
        let mut glyphs = vec![GlyphInfo::default(); (LAST_GLYPH - FIRST_GLYPH + 1) as usize];

        for codepoint in FIRST_GLYPH..=LAST_GLYPH {
            let character = char::from_u32(codepoint).unwrap_or('?');
            let glyph_index = (codepoint - FIRST_GLYPH) as usize;
            let tile_x = (glyph_index as u32 % (ATLAS_WIDTH / GLYPH_TILE)) * GLYPH_TILE;
            let tile_y = (glyph_index as u32 / (ATLAS_WIDTH / GLYPH_TILE)) * GLYPH_TILE;
            let mut glyph = scaled.scaled_glyph(character);
            glyph.position = point(0.0, ascent);
            let Some(outlined) = font.outline_glyph(glyph) else {
                continue;
            };
            let bounds = outlined.px_bounds();
            let width = bounds.width().ceil().max(0.0) as u32;
            let height = bounds.height().ceil().max(0.0) as u32;
            if width + GLYPH_PADDING * 2 > GLYPH_TILE
                || height + GLYPH_PADDING * 2 > GLYPH_TILE
                || tile_y + GLYPH_TILE > ATLAS_HEIGHT
            {
                continue;
            }
            outlined.draw(|x, y, coverage| {
                let atlas_x = tile_x + GLYPH_PADDING + x;
                let atlas_y = tile_y + GLYPH_PADDING + y;
                let index = ((atlas_y * ATLAS_WIDTH + atlas_x) * ATLAS_BYTES_PER_PIXEL) as usize;
                pixels[index..index + 3].fill(u8::MAX);
                pixels[index + 3] = (coverage * 255.0).round() as u8;
            });
            glyphs[glyph_index] = GlyphInfo {
                atlas_x: tile_x + GLYPH_PADDING,
                atlas_y: tile_y + GLYPH_PADDING,
                width,
                height,
                offset_x: bounds.min.x,
                offset_y: bounds.min.y,
                colored: false,
            };
        }

        Ok(Self {
            pixels,
            glyphs,
            cell_width,
            cell_height,
            font_system: {
                let source =
                    fontdb::Source::Binary(Arc::new(epaint_default_fonts::HACK_REGULAR.to_vec()));
                let mut font_system = FontSystem::new_with_fonts([source]);
                font_system.db_mut().set_monospace_family("Hack");
                font_system
            },
            swash_cache: SwashCache::new(),
            dynamic_glyphs: HashMap::new(),
            shaped_cells: HashMap::new(),
            dynamic_allocator: AtlasAllocator::new(size2(
                ATLAS_WIDTH as i32,
                (ATLAS_HEIGHT - STATIC_ATLAS_HEIGHT) as i32,
            )),
            pinned_glyphs: HashSet::new(),
            use_clock: 0,
            evictions: 0,
            revision: 0,
            dirty: None,
        })
    }

    fn glyph(&self, character: char) -> GlyphInfo {
        let codepoint = u32::from(character);
        let codepoint = if (FIRST_GLYPH..=LAST_GLYPH).contains(&codepoint) {
            codepoint
        } else {
            u32::from('?')
        };
        self.glyphs[(codepoint - FIRST_GLYPH) as usize]
    }

    fn begin_frame(&mut self) {
        self.pinned_glyphs.clear();
    }

    fn shape_cell(&mut self, cell: &cshell_terminal::Cell) -> Vec<GlyphInfo> {
        if matches!(
            cell.width,
            CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
        ) {
            return Vec::new();
        }
        if cell.zerowidth().is_empty()
            && cell.character.is_ascii()
            && !cell.style.bold
            && !cell.style.italic
        {
            return vec![self.glyph(cell.character)];
        }

        let columns = if cell.width == CellWidth::Wide { 2 } else { 1 };
        self.shape_text(&cell.characters().collect::<String>(), columns, cell.style)
    }

    fn shape_text(
        &mut self,
        text: &str,
        columns: u16,
        style: cshell_terminal::Style,
    ) -> Vec<GlyphInfo> {
        let key = ShapeKey {
            text: text.to_owned(),
            bold: style.bold,
            italic: style.italic,
            columns: columns.max(1),
        };
        let shaped = if let Some(shaped) = self.shaped_cells.get(&key) {
            shaped.clone()
        } else {
            let mut attrs = Attrs::new().family(Family::Monospace);
            if key.bold {
                attrs = attrs.weight(Weight::BOLD);
            }
            if key.italic {
                attrs = attrs.style(FontStyle::Italic);
            }
            let mut buffer = Buffer::new(
                &mut self.font_system,
                Metrics::new(FONT_SIZE, self.cell_height),
            );
            let cell_span = self.cell_width * f32::from(key.columns);
            buffer.set_wrap(Wrap::None);
            buffer.set_size(Some(cell_span), Some(self.cell_height));
            buffer.set_monospace_width(Some(cell_span));
            buffer.set_text(&key.text, &attrs, Shaping::Advanced, None);
            buffer.shape_until_scroll(&mut self.font_system, false);
            let shaped: Vec<_> = buffer
                .layout_runs()
                .flat_map(|run| {
                    run.glyphs.iter().map(move |glyph| {
                        let physical = glyph.physical((0.0, run.line_y), 1.0);
                        ShapedGlyph {
                            cache_key: physical.cache_key,
                            offset_x: physical.x as f32,
                            offset_y: physical.y as f32,
                        }
                    })
                })
                .collect();
            if self.shaped_cells.len() >= 4096 {
                self.shaped_cells.clear();
            }
            self.shaped_cells.insert(key, shaped.clone());
            shaped
        };

        let mut glyphs = Vec::with_capacity(shaped.len());
        for shaped_glyph in shaped {
            if let Some(mut glyph) = self.dynamic_glyph(shaped_glyph.cache_key) {
                glyph.offset_x += shaped_glyph.offset_x;
                glyph.offset_y += shaped_glyph.offset_y;
                glyphs.push(glyph);
            }
        }
        if glyphs.is_empty() {
            glyphs.push(self.glyph('?'));
        }
        glyphs
    }

    fn dynamic_glyph(&mut self, cache_key: cosmic_text::CacheKey) -> Option<GlyphInfo> {
        self.use_clock = self.use_clock.saturating_add(1);
        if let Some(cached) = self.dynamic_glyphs.get_mut(&cache_key) {
            cached.last_used = self.use_clock;
            self.pinned_glyphs.insert(cache_key);
            return Some(cached.glyph);
        }
        let image = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, cache_key)?;
        if image.placement.width == 0 || image.placement.height == 0 {
            return None;
        }
        let width = image.placement.width;
        let height = image.placement.height;
        if width.saturating_add(GLYPH_PADDING * 2) > ATLAS_WIDTH
            || height.saturating_add(GLYPH_PADDING * 2)
                > ATLAS_HEIGHT.saturating_sub(STATIC_ATLAS_HEIGHT)
        {
            return None;
        }
        let allocation = self.allocate_dynamic(
            width.saturating_add(GLYPH_PADDING * 2),
            height.saturating_add(GLYPH_PADDING * 2),
        )?;
        let atlas_x = allocation.rectangle.min.x as u32 + GLYPH_PADDING;
        let atlas_y = allocation.rectangle.min.y as u32 + STATIC_ATLAS_HEIGHT + GLYPH_PADDING;
        for y in 0..height {
            for x in 0..width {
                let source_index = (y * width + x) as usize;
                let atlas_index =
                    (((atlas_y + y) * ATLAS_WIDTH + atlas_x + x) * ATLAS_BYTES_PER_PIXEL) as usize;
                let pixel = atlas_pixel(image.content, &image.data, source_index);
                self.pixels[atlas_index..atlas_index + ATLAS_BYTES_PER_PIXEL as usize]
                    .copy_from_slice(&pixel);
            }
        }
        self.revision = self.revision.saturating_add(1);
        let glyph = GlyphInfo {
            atlas_x,
            atlas_y,
            width,
            height,
            offset_x: image.placement.left as f32,
            offset_y: -image.placement.top as f32,
            colored: image.content == SwashContent::Color,
        };
        self.dynamic_glyphs.insert(
            cache_key,
            CachedGlyph {
                glyph,
                allocation_id: allocation.id,
                last_used: self.use_clock,
            },
        );
        self.pinned_glyphs.insert(cache_key);
        self.mark_dirty(atlas_x, atlas_y, width, height);
        Some(glyph)
    }

    fn allocate_dynamic(&mut self, width: u32, height: u32) -> Option<guillotiere::Allocation> {
        let requested = size2(width as i32, height as i32);
        loop {
            if let Some(allocation) = self.dynamic_allocator.allocate(requested) {
                return Some(allocation);
            }
            let victim = self
                .dynamic_glyphs
                .iter()
                .filter(|(cache_key, _)| !self.pinned_glyphs.contains(cache_key))
                .min_by_key(|(cache_key, cached)| (cached.last_used, **cache_key))
                .map(|(cache_key, _)| *cache_key)?;
            let evicted = self.dynamic_glyphs.remove(&victim)?;
            self.dynamic_allocator.deallocate(evicted.allocation_id);
            self.evictions = self.evictions.saturating_add(1);
        }
    }

    fn mark_dirty(&mut self, x: u32, y: u32, width: u32, height: u32) {
        let next = AtlasDirtyRect {
            min_x: x,
            min_y: y,
            max_x: x.saturating_add(width).min(ATLAS_WIDTH),
            max_y: y.saturating_add(height).min(ATLAS_HEIGHT),
        };
        self.dirty = Some(match self.dirty {
            Some(current) => AtlasDirtyRect {
                min_x: current.min_x.min(next.min_x),
                min_y: current.min_y.min(next.min_y),
                max_x: current.max_x.max(next.max_x),
                max_y: current.max_y.max(next.max_y),
            },
            None => next,
        });
    }

    fn take_dirty_upload(&mut self) -> Option<AtlasUpload> {
        let dirty = self.dirty.take()?;
        let width = dirty.max_x.saturating_sub(dirty.min_x);
        let height = dirty.max_y.saturating_sub(dirty.min_y);
        if width == 0 || height == 0 {
            return None;
        }
        let row_bytes = (width * ATLAS_BYTES_PER_PIXEL) as usize;
        let mut bytes = Vec::with_capacity(row_bytes.saturating_mul(height as usize));
        for y in dirty.min_y..dirty.max_y {
            let start = ((y * ATLAS_WIDTH + dirty.min_x) * ATLAS_BYTES_PER_PIXEL) as usize;
            bytes.extend_from_slice(&self.pixels[start..start + row_bytes]);
        }
        Some(AtlasUpload {
            origin: [dirty.min_x, dirty.min_y],
            size: [width, height],
            bytes,
        })
    }
}

fn atlas_pixel(content: SwashContent, data: &[u8], source_index: usize) -> [u8; 4] {
    match content {
        SwashContent::Mask => {
            let coverage = data.get(source_index).copied().unwrap_or_default();
            [u8::MAX, u8::MAX, u8::MAX, coverage]
        }
        SwashContent::Color => data
            .get(source_index.saturating_mul(4)..source_index.saturating_mul(4) + 4)
            .and_then(|rgba| <[u8; 4]>::try_from(rgba).ok())
            .unwrap_or_default(),
        SwashContent::SubpixelMask => {
            let coverage = data
                .get(source_index.saturating_mul(4)..source_index.saturating_mul(4) + 4)
                .map(|rgba| rgba[0].max(rgba[1]).max(rgba[2]))
                .unwrap_or_default();
            [u8::MAX, u8::MAX, u8::MAX, coverage]
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalGeometryBenchmarkReport {
    pub cells: usize,
    pub iterations: u32,
    pub vertices: usize,
    pub cached_dynamic_glyphs: usize,
    pub atlas_upload_bytes: usize,
    pub atlas_evictions: u64,
    pub cold: Duration,
    pub warm_per_iteration: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtlasPressureBenchmarkReport {
    pub atlas_bytes: usize,
    pub dynamic_capacity_glyphs: usize,
    pub cached_dynamic_glyphs: usize,
    pub requested_churn: u64,
    pub atlas_evictions: u64,
    pub upload_operations: u64,
    pub upload_bytes: u64,
    pub max_upload_bytes: usize,
    pub upload_integrity: bool,
    pub pinned_glyph_survived: bool,
    pub rerasterized_after_eviction: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogGeometryBenchmarkReport {
    pub page_rows: usize,
    pub planned_rows: usize,
    pub iterations: u32,
    pub vertices: usize,
    pub cached_dynamic_glyphs: usize,
    pub atlas_upload_bytes: usize,
    pub atlas_evictions: u64,
    pub cold: Duration,
    pub warm_per_iteration: Duration,
}

pub fn benchmark_terminal_geometry(
    snapshot: &FrameSnapshot,
    iterations: u32,
) -> Result<TerminalGeometryBenchmarkReport, WindowRendererError> {
    let mut atlas = GlyphAtlas::build()?;
    let width = (atlas.cell_width * f32::from(snapshot.cols)).ceil() as u32;
    let height = (atlas.cell_height * f32::from(snapshot.rows)).ceil() as u32;
    let viewport = TerminalViewport {
        x: 0,
        y: 0,
        width: width.max(1),
        height: height.max(1),
    };

    let cold_started = Instant::now();
    let vertices = build_vertices(
        snapshot,
        &mut atlas,
        viewport.width,
        viewport.height,
        viewport,
        0..snapshot.rows,
        &TerminalDecorations::default(),
        true,
    );
    std::hint::black_box(&vertices);
    let cold = cold_started.elapsed();
    let atlas_upload_bytes = atlas
        .take_dirty_upload()
        .map_or(0, |upload| upload.bytes.len());

    let iterations = iterations.max(1);
    let warm_started = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(build_vertices(
            snapshot,
            &mut atlas,
            viewport.width,
            viewport.height,
            viewport,
            0..snapshot.rows,
            &TerminalDecorations::default(),
            true,
        ));
    }
    let warm_per_iteration = warm_started.elapsed() / iterations;

    Ok(TerminalGeometryBenchmarkReport {
        cells: snapshot.cells.len(),
        iterations,
        vertices: vertices.len(),
        cached_dynamic_glyphs: atlas.dynamic_glyphs.len(),
        atlas_upload_bytes,
        atlas_evictions: atlas.evictions,
        cold,
        warm_per_iteration,
    })
}

pub fn benchmark_atlas_pressure(
    requested_churn: u64,
) -> Result<AtlasPressureBenchmarkReport, WindowRendererError> {
    let started = Instant::now();
    let mut atlas = GlyphAtlas::build()?;
    let atlas_bytes = atlas.pixels.len();

    let real_cell = cshell_terminal::Cell::with_zerowidth(
        'e',
        ['\u{301}'],
        CellWidth::Single,
        Style::default(),
    );
    atlas.begin_frame();
    let real_glyphs = atlas.shape_cell(&real_cell);
    let real_key = atlas
        .shaped_cells
        .values()
        .flat_map(|glyphs| glyphs.iter().map(|glyph| glyph.cache_key))
        .next();
    let mut upload_operations = 0_u64;
    let mut upload_bytes = 0_u64;
    let mut max_upload_bytes = 0_usize;
    let mut upload_integrity = drain_atlas_upload(
        &mut atlas,
        &mut upload_operations,
        &mut upload_bytes,
        &mut max_upload_bytes,
    );
    let rerasterized_after_eviction = real_key
        .and_then(|key| atlas.dynamic_glyphs.remove(&key).map(|glyph| (key, glyph)))
        .is_some_and(|(key, evicted)| {
            atlas.dynamic_allocator.deallocate(evicted.allocation_id);
            atlas.begin_frame();
            let revision = atlas.revision;
            let rerasterized = !atlas.shape_cell(&real_cell).is_empty()
                && atlas.dynamic_glyphs.contains_key(&key)
                && atlas.revision > revision;
            upload_integrity &= drain_atlas_upload(
                &mut atlas,
                &mut upload_operations,
                &mut upload_bytes,
                &mut max_upload_bytes,
            );
            rerasterized
        });

    atlas.dynamic_glyphs.clear();
    atlas.dynamic_allocator = AtlasAllocator::new(size2(
        ATLAS_WIDTH as i32,
        (ATLAS_HEIGHT - STATIC_ATLAS_HEIGHT) as i32,
    ));
    atlas.pinned_glyphs.clear();
    atlas.evictions = 0;
    let target_evictions = requested_churn.max(1);
    let mut first_key = None;
    let mut dynamic_capacity_glyphs = 0_usize;
    let mut index = 1_u32;
    while atlas.evictions < target_evictions && index < u32::from(u16::MAX) {
        atlas.begin_frame();
        if let Some(key) = first_key {
            atlas.pinned_glyphs.insert(key);
        }
        let key = synthetic_cache_key(index as u16);
        let width = 20 + index % 6 * 4;
        let height = 20 + index.wrapping_mul(5) % 6 * 4;
        let evictions_before = atlas.evictions;
        let Some(allocation) = atlas.allocate_dynamic(width, height) else {
            break;
        };
        atlas.use_clock = atlas.use_clock.saturating_add(1);
        atlas.dynamic_glyphs.insert(
            key,
            CachedGlyph {
                glyph: GlyphInfo {
                    atlas_x: allocation.rectangle.min.x as u32,
                    atlas_y: allocation.rectangle.min.y as u32 + STATIC_ATLAS_HEIGHT,
                    width,
                    height,
                    ..GlyphInfo::default()
                },
                allocation_id: allocation.id,
                last_used: atlas.use_clock,
            },
        );
        atlas.pinned_glyphs.insert(key);
        first_key.get_or_insert(key);
        if evictions_before == 0 && atlas.evictions > 0 {
            dynamic_capacity_glyphs = atlas.dynamic_glyphs.len();
        }
        atlas.mark_dirty(
            allocation.rectangle.min.x as u32,
            allocation.rectangle.min.y as u32 + STATIC_ATLAS_HEIGHT,
            width,
            height,
        );
        upload_integrity &= drain_atlas_upload(
            &mut atlas,
            &mut upload_operations,
            &mut upload_bytes,
            &mut max_upload_bytes,
        );
        index = index.saturating_add(1);
    }

    Ok(AtlasPressureBenchmarkReport {
        atlas_bytes,
        dynamic_capacity_glyphs,
        cached_dynamic_glyphs: atlas.dynamic_glyphs.len(),
        requested_churn,
        atlas_evictions: atlas.evictions,
        upload_operations,
        upload_bytes,
        max_upload_bytes,
        upload_integrity,
        pinned_glyph_survived: first_key.is_some_and(|key| atlas.dynamic_glyphs.contains_key(&key)),
        rerasterized_after_eviction: rerasterized_after_eviction && !real_glyphs.is_empty(),
        elapsed: started.elapsed(),
    })
}

fn synthetic_cache_key(glyph_id: u16) -> cosmic_text::CacheKey {
    cosmic_text::CacheKey::new(
        fontdb::ID::dummy(),
        glyph_id,
        FONT_SIZE,
        (0.0, 0.0),
        Weight::NORMAL,
        cosmic_text::CacheKeyFlags::empty(),
    )
    .0
}

fn drain_atlas_upload(
    atlas: &mut GlyphAtlas,
    operations: &mut u64,
    bytes: &mut u64,
    max_bytes: &mut usize,
) -> bool {
    if let Some(upload) = atlas.take_dirty_upload() {
        let valid = upload.bytes.len()
            == (upload.size[0] * upload.size[1] * ATLAS_BYTES_PER_PIXEL) as usize
            && upload.origin[0].saturating_add(upload.size[0]) <= ATLAS_WIDTH
            && upload.origin[1].saturating_add(upload.size[1]) <= ATLAS_HEIGHT;
        *operations = operations.saturating_add(1);
        *bytes = bytes.saturating_add(upload.bytes.len() as u64);
        *max_bytes = (*max_bytes).max(upload.bytes.len());
        valid
    } else {
        true
    }
}

pub fn benchmark_log_geometry(
    frame: &LogSurfaceFrame,
    viewport_columns: u16,
    viewport_rows: u16,
    iterations: u32,
) -> Result<LogGeometryBenchmarkReport, WindowRendererError> {
    let mut atlas = GlyphAtlas::build()?;
    let width = (atlas.cell_width * f32::from(viewport_columns)).ceil() as u32;
    let height = (atlas.cell_height * f32::from(viewport_rows)).ceil() as u32;
    let viewport = TerminalViewport {
        x: 0,
        y: 0,
        width: width.max(1),
        height: height.max(1),
    };

    let decorations = LogDecorations::default();
    let cold_started = Instant::now();
    let vertices = build_log_vertices(
        frame,
        &mut atlas,
        viewport.width,
        viewport.height,
        viewport,
        &decorations,
    );
    std::hint::black_box(&vertices);
    let cold = cold_started.elapsed();
    let atlas_upload_bytes = atlas
        .take_dirty_upload()
        .map_or(0, |upload| upload.bytes.len());

    let iterations = iterations.max(1);
    let warm_started = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(build_log_vertices(
            frame,
            &mut atlas,
            viewport.width,
            viewport.height,
            viewport,
            &decorations,
        ));
    }
    let warm_per_iteration = warm_started.elapsed() / iterations;

    Ok(LogGeometryBenchmarkReport {
        page_rows: frame.page.rows.len(),
        planned_rows: frame.visible_rows.len(),
        iterations,
        vertices: vertices.len(),
        cached_dynamic_glyphs: atlas.dynamic_glyphs.len(),
        atlas_upload_bytes,
        atlas_evictions: atlas.evictions,
        cold,
        warm_per_iteration,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOutcome {
    Presented,
    Skipped,
    Reconfigured,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalViewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl TerminalViewport {
    #[must_use]
    pub fn from_logical_rect(rect: egui::Rect, pixels_per_point: f32) -> Self {
        let pixels_per_point = pixels_per_point.max(0.01);
        let min_x = (rect.min.x * pixels_per_point).floor().max(0.0) as u32;
        let min_y = (rect.min.y * pixels_per_point).floor().max(0.0) as u32;
        let max_x = (rect.max.x * pixels_per_point).ceil().max(0.0) as u32;
        let max_y = (rect.max.y * pixels_per_point).ceil().max(0.0) as u32;
        Self {
            x: min_x,
            y: min_y,
            width: max_x.saturating_sub(min_x),
            height: max_y.saturating_sub(min_y),
        }
    }

    fn clamp(self, width: u32, height: u32) -> Self {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Self {
            x,
            y,
            width: self.width.min(width.saturating_sub(x)),
            height: self.height.min(height.saturating_sub(y)),
        }
    }

    fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Debug)]
pub struct EguiFrame<'a> {
    pub paint_jobs: &'a [egui::ClippedPrimitive],
    pub textures_delta: &'a mut egui::TexturesDelta,
    pub pixels_per_point: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GeometryKey {
    Terminal {
        generation: u64,
        decorations_revision: u64,
        cursor_visible: bool,
        surface_width: u32,
        surface_height: u32,
        viewport: TerminalViewport,
        first_row: u16,
        last_row: u16,
    },
    Log {
        source_id: u64,
        revision: u64,
        decorations_revision: u64,
        surface_width: u32,
        surface_height: u32,
        viewport: TerminalViewport,
        first_row: usize,
        last_row: usize,
        first_viewport_row: usize,
        cell_offset: u32,
        layout_columns: u16,
    },
}

enum GeometryFrame<'a> {
    Terminal {
        frame: Option<&'a TerminalSurfaceFrame>,
        decorations: &'a TerminalDecorations,
        cursor_visible: bool,
    },
    Log {
        frame: Option<&'a LogSurfaceFrame>,
        decorations: &'a LogDecorations,
    },
}

#[derive(Debug, Error)]
pub enum WindowRendererError {
    #[error("cannot create wgpu window surface: {0}")]
    CreateSurface(#[from] wgpu::CreateSurfaceError),
    #[error("no compatible adapter for the window surface: {0}")]
    Adapter(#[from] wgpu::RequestAdapterError),
    #[error("cannot create window rendering device: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    #[error("window surface has no supported configuration")]
    SurfaceConfiguration,
    #[error("bundled monospace font cannot be parsed")]
    BundledFont,
    #[error("wgpu device was lost")]
    DeviceLost,
    #[error("wgpu surface returned a validation failure")]
    SurfaceValidation,
    #[error("wgpu submission did not complete: {0}")]
    Poll(#[from] wgpu::PollError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeadlessRenderReport {
    pub vertices: u32,
    pub gpu_completion: Duration,
}

/// Persistent offscreen renderer used by transport-to-GPU performance gates.
///
/// Device, pipeline and atlas initialization happen once. Every measured frame
/// follows the production terminal path: shape visible cells, upload dirty atlas
/// rectangles and vertices, execute `terminal.wgsl`, submit, then wait for that
/// exact submission to complete.
pub struct HeadlessTerminalRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    pipeline: wgpu::RenderPipeline,
    atlas_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    atlas: GlyphAtlas,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
    width: u32,
    height: u32,
    adapter_name: String,
    backend: String,
    device_type: String,
    device_lost: Arc<AtomicBool>,
}

impl std::fmt::Debug for HeadlessTerminalRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HeadlessTerminalRenderer")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("adapter_name", &self.adapter_name)
            .field("backend", &self.backend)
            .field("device_type", &self.device_type)
            .finish_non_exhaustive()
    }
}

impl HeadlessTerminalRenderer {
    pub async fn new(width: u32, height: u32) -> Result<Self, WindowRendererError> {
        let width = width.max(1);
        let height = height.max(1);
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await?;
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("cshell-headless-terminal-device"),
                ..Default::default()
            })
            .await?;
        let device_lost = Arc::new(AtomicBool::new(false));
        let callback_flag = Arc::clone(&device_lost);
        device.set_device_lost_callback(move |_reason, _message| {
            callback_flag.store(true, Ordering::Release);
        });

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cshell-headless-terminal-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas = GlyphAtlas::build()?;
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cshell-headless-glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            atlas_texture.as_image_copy(),
            &atlas.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_WIDTH * ATLAS_BYTES_PER_PIXEL),
                rows_per_image: Some(ATLAS_HEIGHT),
            },
            wgpu::Extent3d {
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("cshell-headless-glyph-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cshell-headless-glyph-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cshell-headless-glyph-bind-group"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cshell-headless-terminal-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("terminal.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cshell-headless-terminal-pipeline-layout"),
            bind_group_layouts: &[Some(&atlas_layout)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2,
                1 => Float32x2,
                2 => Float32x4,
                3 => Uint32
            ],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cshell-headless-terminal-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let vertex_capacity = 1024;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cshell-headless-terminal-vertices"),
            size: vertex_capacity as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            device,
            queue,
            target,
            target_view,
            pipeline,
            atlas_texture,
            atlas_bind_group,
            atlas,
            vertex_buffer,
            vertex_capacity,
            width,
            height,
            adapter_name: info.name,
            backend: format!("{:?}", info.backend),
            device_type: format!("{:?}", info.device_type),
            device_lost,
        })
    }

    #[must_use]
    pub fn adapter_info(&self) -> (&str, &str, &str) {
        (&self.adapter_name, &self.backend, &self.device_type)
    }

    pub fn render_frame(
        &mut self,
        frame: &TerminalSurfaceFrame,
    ) -> Result<HeadlessRenderReport, WindowRendererError> {
        if self.device_lost.load(Ordering::Acquire) {
            return Err(WindowRendererError::DeviceLost);
        }
        let viewport = TerminalViewport {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        };
        let vertices = build_vertices(
            &frame.snapshot,
            &mut self.atlas,
            self.width,
            self.height,
            viewport,
            frame.plan.visible_rows.clone(),
            &TerminalDecorations::default(),
            true,
        );
        if let Some(upload) = self.atlas.take_dirty_upload() {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: upload.origin[0],
                        y: upload.origin[1],
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &upload.bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(upload.size[0] * ATLAS_BYTES_PER_PIXEL),
                    rows_per_image: Some(upload.size[1]),
                },
                wgpu::Extent3d {
                    width: upload.size[0],
                    height: upload.size[1],
                    depth_or_array_layers: 1,
                },
            );
        }
        let bytes = bytemuck::cast_slice(&vertices);
        if bytes.len() > self.vertex_capacity {
            self.vertex_capacity = bytes.len().next_power_of_two();
            self.vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cshell-headless-terminal-vertices"),
                size: self.vertex_capacity as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !bytes.is_empty() {
            self.queue.write_buffer(&self.vertex_buffer, 0, bytes);
        }
        let vertex_count = vertices.len().min(u32::MAX as usize) as u32;
        let started = Instant::now();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cshell-headless-terminal-frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cshell-headless-terminal-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.012,
                            g: 0.016,
                            b: 0.024,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if vertex_count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.atlas_bind_group, &[]);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.draw(0..vertex_count, 0..1);
            }
        }
        let submission = self.queue.submit([encoder.finish()]);
        self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(5)),
        })?;
        let gpu_completion = started.elapsed();
        std::hint::black_box(&self.target);
        Ok(HeadlessRenderReport {
            vertices: vertex_count,
            gpu_completion,
        })
    }
}

pub struct WindowRenderer {
    instance: wgpu::Instance,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    egui_renderer: egui_wgpu::Renderer,
    atlas_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    atlas: GlyphAtlas,
    uploaded_atlas_revision: u64,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
    vertex_count: u32,
    geometry_key: Option<GeometryKey>,
    device_lost: Arc<AtomicBool>,
}

impl std::fmt::Debug for WindowRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowRenderer")
            .field("surface_width", &self.config.width)
            .field("surface_height", &self.config.height)
            .field("vertex_capacity", &self.vertex_capacity)
            .field("vertex_count", &self.vertex_count)
            .field("geometry_key", &self.geometry_key)
            .finish_non_exhaustive()
    }
}

impl WindowRenderer {
    pub async fn new(window: Arc<Window>) -> Result<Self, WindowRendererError> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("cshell-window-device"),
                ..Default::default()
            })
            .await?;
        let device_lost = Arc::new(AtomicBool::new(false));
        let callback_flag = Arc::clone(&device_lost);
        device.set_device_lost_callback(move |_reason, _message| {
            callback_flag.store(true, Ordering::Release);
        });

        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        let mut config = surface
            .get_default_config(&adapter, width, height)
            .ok_or(WindowRendererError::SurfaceConfiguration)?;
        let capabilities = surface.get_capabilities(&adapter);
        if let Some(srgb) = capabilities.formats.iter().find(|format| format.is_srgb()) {
            config.format = *srgb;
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let atlas = GlyphAtlas::build()?;
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cshell-glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            atlas_texture.as_image_copy(),
            &atlas.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_WIDTH * ATLAS_BYTES_PER_PIXEL),
                rows_per_image: Some(ATLAS_HEIGHT),
            },
            wgpu::Extent3d {
                width: ATLAS_WIDTH,
                height: ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("cshell-glyph-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cshell-glyph-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cshell-glyph-bind-group"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cshell-terminal-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("terminal.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cshell-terminal-pipeline-layout"),
            bind_group_layouts: &[Some(&atlas_layout)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2,
                1 => Float32x2,
                2 => Float32x4,
                3 => Uint32
            ],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cshell-terminal-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            config.format,
            egui_wgpu::RendererOptions::default(),
        );
        let vertex_capacity = 1024;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cshell-terminal-vertices"),
            size: vertex_capacity as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            instance,
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            egui_renderer,
            atlas_texture,
            atlas_bind_group,
            atlas,
            uploaded_atlas_revision: 0,
            vertex_buffer,
            vertex_capacity,
            vertex_count: 0,
            geometry_key: None,
            device_lost,
        })
    }

    #[must_use]
    pub fn viewport_rows(&self, viewport: TerminalViewport) -> u16 {
        let viewport = viewport.clamp(self.config.width, self.config.height);
        ((viewport.height as f32 / self.atlas.cell_height).floor() as u32).min(u32::from(u16::MAX))
            as u16
    }

    #[must_use]
    pub fn viewport_columns(&self, viewport: TerminalViewport) -> u16 {
        let viewport = viewport.clamp(self.config.width, self.config.height);
        ((viewport.width as f32 / self.atlas.cell_width).floor() as u32)
            .clamp(1, u32::from(u16::MAX)) as u16
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.geometry_key = None;
    }

    pub fn render(
        &mut self,
        frame: Option<&TerminalSurfaceFrame>,
        viewport: TerminalViewport,
        decorations: &TerminalDecorations,
        cursor_visible: bool,
        egui_frame: Option<EguiFrame<'_>>,
    ) -> Result<RenderOutcome, WindowRendererError> {
        self.render_inner(
            GeometryFrame::Terminal {
                frame,
                decorations,
                cursor_visible,
            },
            viewport,
            egui_frame,
        )
    }

    pub fn render_log(
        &mut self,
        frame: Option<&LogSurfaceFrame>,
        viewport: TerminalViewport,
        decorations: &LogDecorations,
        egui_frame: Option<EguiFrame<'_>>,
    ) -> Result<RenderOutcome, WindowRendererError> {
        self.render_inner(
            GeometryFrame::Log { frame, decorations },
            viewport,
            egui_frame,
        )
    }

    fn render_inner(
        &mut self,
        frame: GeometryFrame<'_>,
        viewport: TerminalViewport,
        mut egui_frame: Option<EguiFrame<'_>>,
    ) -> Result<RenderOutcome, WindowRendererError> {
        if self.device_lost.load(Ordering::Acquire) {
            if let Some(egui_frame) = &mut egui_frame {
                egui_frame.textures_delta.clear();
            }
            return Err(WindowRendererError::DeviceLost);
        }
        let mut textures_to_free = Vec::new();
        if let Some(egui_frame) = &mut egui_frame {
            for (texture_id, image_deltas) in egui_frame.textures_delta.set.drain() {
                for image_delta in image_deltas {
                    self.egui_renderer.update_texture(
                        &self.device,
                        &self.queue,
                        texture_id,
                        &image_delta,
                    );
                }
            }
            textures_to_free.extend(egui_frame.textures_delta.free.drain());
        }
        let viewport = viewport.clamp(self.config.width, self.config.height);
        match frame {
            GeometryFrame::Terminal {
                frame: Some(frame),
                decorations,
                cursor_visible,
            } => self.update_geometry(frame, viewport, decorations, cursor_visible),
            GeometryFrame::Log {
                frame: Some(frame),
                decorations,
            } => self.update_log_geometry(frame, viewport, decorations),
            GeometryFrame::Terminal { frame: None, .. }
            | GeometryFrame::Log { frame: None, .. } => {
                self.vertex_count = 0;
                self.geometry_key = None;
            }
        }
        let (output, reconfigure_after_present) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => (output, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.free_egui_textures(&textures_to_free);
                return Ok(RenderOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                self.free_egui_textures(&textures_to_free);
                return Ok(RenderOutcome::Reconfigured);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(Arc::clone(&self.window))?;
                self.surface.configure(&self.device, &self.config);
                self.free_egui_textures(&textures_to_free);
                return Ok(RenderOutcome::Reconfigured);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                self.free_egui_textures(&textures_to_free);
                return Err(WindowRendererError::SurfaceValidation);
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cshell-terminal-frame"),
            });
        let (screen_descriptor, user_command_buffers) = if let Some(egui_frame) = &mut egui_frame {
            let descriptor = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.config.width, self.config.height],
                pixels_per_point: egui_frame.pixels_per_point,
            };
            let commands = self.egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                egui_frame.paint_jobs,
                &descriptor,
            );
            (Some(descriptor), commands)
        } else {
            (None, Vec::new())
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cshell-terminal-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.012,
                            g: 0.016,
                            b: 0.024,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if self.vertex_count > 0 && !viewport.is_empty() {
                pass.set_scissor_rect(viewport.x, viewport.y, viewport.width, viewport.height);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.atlas_bind_group, &[]);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.draw(0..self.vertex_count, 0..1);
            }
            if let (Some(egui_frame), Some(screen_descriptor)) = (&egui_frame, &screen_descriptor) {
                self.egui_renderer.render(
                    &mut pass.forget_lifetime(),
                    egui_frame.paint_jobs,
                    screen_descriptor,
                );
            }
        }
        self.queue
            .submit(user_command_buffers.into_iter().chain([encoder.finish()]));
        self.queue.present(output);
        self.free_egui_textures(&textures_to_free);
        if reconfigure_after_present {
            self.surface.configure(&self.device, &self.config);
            Ok(RenderOutcome::Reconfigured)
        } else {
            Ok(RenderOutcome::Presented)
        }
    }

    fn free_egui_textures(&mut self, texture_ids: &[egui::TextureId]) {
        for texture_id in texture_ids {
            self.egui_renderer.free_texture(texture_id);
        }
    }

    fn update_geometry(
        &mut self,
        frame: &TerminalSurfaceFrame,
        viewport: TerminalViewport,
        decorations: &TerminalDecorations,
        cursor_visible: bool,
    ) {
        let key = GeometryKey::Terminal {
            generation: frame.snapshot.generation,
            decorations_revision: decorations.revision,
            cursor_visible,
            surface_width: self.config.width,
            surface_height: self.config.height,
            viewport,
            first_row: frame.plan.visible_rows.start,
            last_row: frame.plan.visible_rows.end,
        };
        if self.geometry_key == Some(key) {
            return;
        }
        let vertices = build_vertices(
            &frame.snapshot,
            &mut self.atlas,
            self.config.width,
            self.config.height,
            viewport,
            frame.plan.visible_rows.clone(),
            decorations,
            cursor_visible,
        );
        self.upload_geometry(vertices, key);
    }

    fn update_log_geometry(
        &mut self,
        frame: &LogSurfaceFrame,
        viewport: TerminalViewport,
        decorations: &LogDecorations,
    ) {
        let key = GeometryKey::Log {
            source_id: frame.page.source_id.0,
            revision: frame.page.revision,
            decorations_revision: decorations.revision,
            surface_width: self.config.width,
            surface_height: self.config.height,
            viewport,
            first_row: frame.visible_rows.start,
            last_row: frame.visible_rows.end,
            first_viewport_row: frame.first_viewport_row,
            cell_offset: frame.cell_offset,
            layout_columns: frame.layout_columns,
        };
        if self.geometry_key == Some(key) {
            return;
        }
        let vertices = build_log_vertices(
            frame,
            &mut self.atlas,
            self.config.width,
            self.config.height,
            viewport,
            decorations,
        );
        self.upload_geometry(vertices, key);
    }

    fn upload_geometry(&mut self, vertices: Vec<Vertex>, key: GeometryKey) {
        if self.uploaded_atlas_revision != self.atlas.revision {
            if let Some(upload) = self.atlas.take_dirty_upload() {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.atlas_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: upload.origin[0],
                            y: upload.origin[1],
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &upload.bytes,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(upload.size[0] * ATLAS_BYTES_PER_PIXEL),
                        rows_per_image: Some(upload.size[1]),
                    },
                    wgpu::Extent3d {
                        width: upload.size[0],
                        height: upload.size[1],
                        depth_or_array_layers: 1,
                    },
                );
            }
            self.uploaded_atlas_revision = self.atlas.revision;
        }
        let bytes = bytemuck::cast_slice(&vertices);
        if bytes.len() > self.vertex_capacity {
            self.vertex_capacity = bytes.len().next_power_of_two();
            self.vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cshell-terminal-vertices"),
                size: self.vertex_capacity as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !bytes.is_empty() {
            self.queue.write_buffer(&self.vertex_buffer, 0, bytes);
        }
        self.vertex_count = vertices.len().min(u32::MAX as usize) as u32;
        self.geometry_key = Some(key);
    }
}

// Keeping the complete render context explicit here makes the allocation-free hot path easier to
// audit at each call site than hiding these values behind mutable renderer state.
#[allow(clippy::too_many_arguments)]
fn build_vertices(
    snapshot: &FrameSnapshot,
    atlas: &mut GlyphAtlas,
    width: u32,
    height: u32,
    viewport: TerminalViewport,
    rows: std::ops::Range<u16>,
    decorations: &TerminalDecorations,
    cursor_visible: bool,
) -> Vec<Vertex> {
    atlas.begin_frame();
    let first_row = rows.start;
    let row_count = usize::from(rows.end.saturating_sub(rows.start));
    let column_count = usize::from(snapshot.cols);
    let mut search_layers = vec![0_u8; row_count.saturating_mul(column_count)];
    for (match_index, search_match) in decorations.search.matches.iter().enumerate() {
        if search_match.start.row < rows.start || search_match.start.row >= rows.end {
            continue;
        }
        let row_offset = usize::from(search_match.start.row - rows.start) * column_count;
        let first_column = search_match
            .start
            .column
            .min(snapshot.cols.saturating_sub(1));
        let last_column = search_match.end.column.min(snapshot.cols.saturating_sub(1));
        let layer = if decorations.active_search_match == Some(match_index) {
            2
        } else {
            1
        };
        for column in first_column..=last_column {
            search_layers[row_offset + usize::from(column)] = layer;
        }
    }
    let mut vertices = Vec::with_capacity(
        usize::from(rows.end.saturating_sub(rows.start))
            .saturating_mul(usize::from(snapshot.cols))
            .saturating_mul(12),
    );
    for row in rows {
        let Some(cells) = snapshot.row(row) else {
            continue;
        };
        let y = viewport.y as f32 + f32::from(row.saturating_sub(first_row)) * atlas.cell_height;
        if y >= viewport.y.saturating_add(viewport.height) as f32 {
            break;
        }
        for (column, cell) in cells.iter().enumerate() {
            let x = viewport.x as f32 + column as f32 * atlas.cell_width;
            if x >= viewport.x.saturating_add(viewport.width) as f32 {
                break;
            }
            let is_cursor_cell =
                row == snapshot.cursor_row && column == usize::from(snapshot.cursor_col);
            let cursor_shape =
                (cursor_visible && is_cursor_cell).then_some(snapshot.cursor_appearance.shape);
            let (mut foreground, mut background) = (
                resolve_color(cell.style.foreground, true),
                resolve_color(cell.style.background, false),
            );
            if cell.style.inverse {
                std::mem::swap(&mut foreground, &mut background);
            }
            let cursor_background = (cursor_shape == Some(CursorShape::Block)).then(|| {
                let cursor_color = snapshot
                    .cursor_appearance
                    .color
                    .map_or(foreground, |color| resolve_color(color, true));
                foreground = if background[3] > 0.0 {
                    background
                } else {
                    [0.0, 0.0, 0.0, 1.0]
                };
                cursor_color
            });
            if background[3] > 0.0 {
                push_quad(
                    &mut vertices,
                    [x, y],
                    [atlas.cell_width, atlas.cell_height],
                    white_uv(),
                    background,
                    false,
                    [width, height],
                );
            }
            let overlay_index = usize::from(row - first_row) * column_count + column;
            let search_color = match search_layers[overlay_index] {
                1 => Some([0.95, 0.55, 0.10, 0.35]),
                2 => Some([1.00, 0.75, 0.15, 0.55]),
                _ => None,
            };
            if let Some(color) = search_color {
                push_cell_overlay(
                    &mut vertices,
                    [x, y],
                    [atlas.cell_width, atlas.cell_height],
                    color,
                    [width, height],
                );
            }
            if decorations
                .selection
                .is_some_and(|selection| selection.contains(snapshot, row, column as u16))
            {
                push_cell_overlay(
                    &mut vertices,
                    [x, y],
                    [atlas.cell_width, atlas.cell_height],
                    [0.20, 0.45, 0.90, 0.50],
                    [width, height],
                );
            }
            if let Some(color) = cursor_background {
                push_cell_overlay(
                    &mut vertices,
                    [x, y],
                    [atlas.cell_width, atlas.cell_height],
                    color,
                    [width, height],
                );
            }
            if cell.character != ' '
                && cell.character != '\0'
                && !matches!(
                    cell.width,
                    CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
                )
            {
                for glyph in atlas.shape_cell(cell) {
                    if glyph.width == 0 || glyph.height == 0 {
                        continue;
                    }
                    let uv = [
                        glyph.atlas_x as f32 / ATLAS_WIDTH as f32,
                        glyph.atlas_y as f32 / ATLAS_HEIGHT as f32,
                        (glyph.atlas_x + glyph.width) as f32 / ATLAS_WIDTH as f32,
                        (glyph.atlas_y + glyph.height) as f32 / ATLAS_HEIGHT as f32,
                    ];
                    push_quad(
                        &mut vertices,
                        [x + CELL_PADDING_X + glyph.offset_x, y + glyph.offset_y],
                        [glyph.width as f32, glyph.height as f32],
                        uv,
                        foreground,
                        glyph.colored,
                        [width, height],
                    );
                }
            }
            if cell.style.underline {
                push_quad(
                    &mut vertices,
                    [x, y + atlas.cell_height - 2.0],
                    [atlas.cell_width, 1.0],
                    white_uv(),
                    foreground,
                    false,
                    [width, height],
                );
            }
            if let Some(
                shape @ (CursorShape::Underline | CursorShape::Beam | CursorShape::HollowBlock),
            ) = cursor_shape
            {
                let color = snapshot
                    .cursor_appearance
                    .color
                    .map_or(foreground, |color| resolve_color(color, true));
                push_cursor_decoration(
                    &mut vertices,
                    shape,
                    [x, y],
                    [atlas.cell_width, atlas.cell_height],
                    color,
                    [width, height],
                );
            }
        }
    }
    vertices
}

fn push_cell_overlay(
    vertices: &mut Vec<Vertex>,
    origin: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    surface_size: [u32; 2],
) {
    push_quad(
        vertices,
        origin,
        size,
        white_uv(),
        color,
        false,
        surface_size,
    );
}

fn push_cursor_decoration(
    vertices: &mut Vec<Vertex>,
    shape: CursorShape,
    origin: [f32; 2],
    cell_size: [f32; 2],
    color: [f32; 4],
    surface_size: [u32; 2],
) {
    let [x, y] = origin;
    let [cell_width, cell_height] = cell_size;
    match shape {
        CursorShape::Underline => push_quad(
            vertices,
            [x, y + cell_height - 2.0],
            [cell_width, 2.0],
            white_uv(),
            color,
            false,
            surface_size,
        ),
        CursorShape::Beam => push_quad(
            vertices,
            [x, y],
            [2.0, cell_height],
            white_uv(),
            color,
            false,
            surface_size,
        ),
        CursorShape::HollowBlock => {
            push_quad(
                vertices,
                [x, y],
                [cell_width, 1.0],
                white_uv(),
                color,
                false,
                surface_size,
            );
            push_quad(
                vertices,
                [x, y + cell_height - 1.0],
                [cell_width, 1.0],
                white_uv(),
                color,
                false,
                surface_size,
            );
            push_quad(
                vertices,
                [x, y],
                [1.0, cell_height],
                white_uv(),
                color,
                false,
                surface_size,
            );
            push_quad(
                vertices,
                [x + cell_width - 1.0, y],
                [1.0, cell_height],
                white_uv(),
                color,
                false,
                surface_size,
            );
        }
        CursorShape::Block | CursorShape::Hidden => {}
    }
}

fn build_log_vertices(
    frame: &LogSurfaceFrame,
    atlas: &mut GlyphAtlas,
    width: u32,
    height: u32,
    viewport: TerminalViewport,
    decorations: &LogDecorations,
) -> Vec<Vertex> {
    atlas.begin_frame();
    let rows = frame.visible_rows.clone();
    let mut vertices = Vec::with_capacity(
        rows.clone()
            .filter_map(|index| frame.visual_rows.get(index))
            .filter_map(|visual_row| frame.page.rows.get(visual_row.logical_row_index as usize))
            .map(|row| row.text.len().min(256))
            .sum::<usize>()
            .saturating_mul(6),
    );
    for (relative_row, visual_index) in rows.enumerate() {
        let Some(visual_row) = frame.visual_rows.get(visual_index) else {
            continue;
        };
        let Some(row) = frame.page.rows.get(visual_row.logical_row_index as usize) else {
            continue;
        };
        let y = viewport.y as f32
            + (relative_row as f32 - frame.first_viewport_row as f32) * atlas.cell_height;
        if y >= viewport.y.saturating_add(viewport.height) as f32 {
            break;
        }
        if y + atlas.cell_height <= viewport.y as f32 {
            continue;
        }
        let first_cell = if relative_row == frame.first_viewport_row {
            visual_row.start_cell.max(frame.cell_offset)
        } else {
            visual_row.start_cell
        };
        let last_cell = visual_row.end_cell;
        let start_byte = visual_row.start_byte as usize;
        let end_byte = visual_row.end_byte as usize;
        let Some(text) = row.text.get(start_byte..end_byte) else {
            continue;
        };
        let active_search = decorations
            .active_search_match
            .as_ref()
            .filter(|matched| matched.line_id == row.line_id);
        let mut cell_column = visual_row.start_cell;
        let mut style_index = row
            .style_spans
            .partition_point(|span| span.byte_range.end as usize <= start_byte);
        for (relative_byte_index, grapheme) in text.grapheme_indices(true) {
            let byte_index = start_byte + relative_byte_index;
            let grapheme_end = byte_index.saturating_add(grapheme.len());
            while row
                .style_spans
                .get(style_index)
                .is_some_and(|span| span.byte_range.end as usize <= byte_index)
            {
                style_index += 1;
            }
            let style = row
                .style_spans
                .get(style_index)
                .filter(|span| {
                    span.byte_range.start as usize <= byte_index
                        && byte_index < span.byte_range.end as usize
                })
                .map_or_else(Style::default, |span| span.style);
            let columns = if grapheme == "\t" {
                8 - cell_column % 8
            } else {
                UnicodeWidthStr::width(grapheme).clamp(1, 2) as u32
            };
            let next_column = cell_column.saturating_add(columns);
            if cell_column >= last_cell {
                break;
            }
            if next_column <= first_cell {
                cell_column = next_column;
                continue;
            }
            let x = viewport.x as f32
                + cell_column.saturating_sub(first_cell) as f32 * atlas.cell_width;
            if x >= viewport.x.saturating_add(viewport.width) as f32 {
                break;
            }
            let mut foreground = resolve_color(style.foreground, true);
            let mut background = resolve_color(style.background, false);
            if style.inverse {
                std::mem::swap(&mut foreground, &mut background);
            }
            if active_search.is_some_and(|matched| {
                (matched.byte_range.start as usize) < grapheme_end
                    && byte_index < matched.byte_range.end as usize
            }) {
                foreground = [0.05, 0.05, 0.05, 1.0];
                background = [1.0, 0.72, 0.12, 0.92];
            }
            let span_width = atlas.cell_width * columns as f32;
            if background[3] > 0.0 {
                push_quad(
                    &mut vertices,
                    [x, y],
                    [span_width, atlas.cell_height],
                    white_uv(),
                    background,
                    false,
                    [width, height],
                );
            }
            if grapheme != "\t" && grapheme != " " {
                for glyph in atlas.shape_text(grapheme, columns as u16, style) {
                    if glyph.width == 0 || glyph.height == 0 {
                        continue;
                    }
                    let uv = [
                        glyph.atlas_x as f32 / ATLAS_WIDTH as f32,
                        glyph.atlas_y as f32 / ATLAS_HEIGHT as f32,
                        (glyph.atlas_x + glyph.width) as f32 / ATLAS_WIDTH as f32,
                        (glyph.atlas_y + glyph.height) as f32 / ATLAS_HEIGHT as f32,
                    ];
                    push_quad(
                        &mut vertices,
                        [x + CELL_PADDING_X + glyph.offset_x, y + glyph.offset_y],
                        [glyph.width as f32, glyph.height as f32],
                        uv,
                        foreground,
                        glyph.colored,
                        [width, height],
                    );
                }
            }
            if style.underline {
                push_quad(
                    &mut vertices,
                    [x, y + atlas.cell_height - 2.0],
                    [span_width, 1.0],
                    white_uv(),
                    foreground,
                    false,
                    [width, height],
                );
            }
            cell_column = next_column;
        }
    }
    vertices
}

fn push_quad(
    vertices: &mut Vec<Vertex>,
    origin: [f32; 2],
    size: [f32; 2],
    uv: [f32; 4],
    color: [f32; 4],
    colored: bool,
    surface_size: [u32; 2],
) {
    let [width, height] = surface_size;
    let x0 = origin[0] / width as f32 * 2.0 - 1.0;
    let x1 = (origin[0] + size[0]) / width as f32 * 2.0 - 1.0;
    let y0 = 1.0 - origin[1] / height as f32 * 2.0;
    let y1 = 1.0 - (origin[1] + size[1]) / height as f32 * 2.0;
    let corners = [
        Vertex {
            position: [x0, y0],
            uv: [uv[0], uv[1]],
            color,
            colored: u32::from(colored),
        },
        Vertex {
            position: [x0, y1],
            uv: [uv[0], uv[3]],
            color,
            colored: u32::from(colored),
        },
        Vertex {
            position: [x1, y1],
            uv: [uv[2], uv[3]],
            color,
            colored: u32::from(colored),
        },
        Vertex {
            position: [x1, y0],
            uv: [uv[2], uv[1]],
            color,
            colored: u32::from(colored),
        },
    ];
    vertices.extend_from_slice(&[
        corners[0], corners[1], corners[2], corners[0], corners[2], corners[3],
    ]);
}

fn white_uv() -> [f32; 4] {
    let x = 0.5 / ATLAS_WIDTH as f32;
    let y = 0.5 / ATLAS_HEIGHT as f32;
    [x, y, x, y]
}

fn resolve_color(color: Color, foreground: bool) -> [f32; 4] {
    let rgb = match color {
        Color::Default if foreground => [216, 222, 233],
        Color::Default => return [0.0, 0.0, 0.0, 0.0],
        Color::Indexed(index) => xterm_color(index),
        Color::Rgb(red, green, blue) => [red, green, blue],
    };
    [
        srgb_to_linear(rgb[0]),
        srgb_to_linear(rgb[1]),
        srgb_to_linear(rgb[2]),
        1.0,
    ]
}

fn xterm_color(index: u8) -> [u8; 3] {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [229, 229, 229],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [255, 255, 255],
    ];
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let cube = index - 16;
            [
                LEVELS[usize::from(cube / 36)],
                LEVELS[usize::from((cube % 36) / 6)],
                LEVELS[usize::from(cube % 6)],
            ]
        }
        232..=255 => {
            let gray = 8 + (index - 232) * 10;
            [gray, gray, gray]
        }
    }
}

fn srgb_to_linear(value: u8) -> f32 {
    let value = f32::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ATLAS_BYTES_PER_PIXEL, CachedGlyph, GlyphAtlas, GlyphInfo, TerminalViewport, atlas_pixel,
        build_log_vertices, build_vertices, xterm_color,
    };
    use crate::{
        LogDecorations, LogPage, LogRow, LogSearchMatch, LogSourceId, LogStyleSpan,
        LogSurfaceFrame, LogSurfaceModel, LogVisualRow, TerminalCellPoint, TerminalDecorations,
        TerminalSearchMatch, TerminalSearchResult, TerminalSelection, TerminalSelectionMode,
    };
    use cosmic_text::{CacheKey, CacheKeyFlags, SwashContent, Weight, fontdb};
    use cshell_terminal::{
        Cell, CellWidth, Color, CursorAppearance, CursorShape, FrameSnapshot, Style, TerminalModes,
    };
    use guillotiere::{AtlasAllocator, size2};
    use std::sync::Arc;

    fn cache_key(glyph_id: u16) -> CacheKey {
        CacheKey::new(
            fontdb::ID::dummy(),
            glyph_id,
            super::FONT_SIZE,
            (0.0, 0.0),
            Weight::NORMAL,
            CacheKeyFlags::empty(),
        )
        .0
    }

    #[test]
    fn bundled_font_builds_ascii_atlas_and_unknown_uses_fallback() {
        let atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        assert!(atlas.glyph('A').width > 0);
        assert_eq!(atlas.glyph('中').width, atlas.glyph('?').width);
        assert!(atlas.cell_width > 8.0);
        assert!(atlas.cell_height > 16.0);
    }

    #[test]
    fn terminal_cells_generate_bounded_background_glyph_and_underline_quads() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        let snapshot = FrameSnapshot {
            generation: 1,
            rows: 1,
            cols: 2,
            cursor_row: 0,
            cursor_col: 1,
            cursor_appearance: Default::default(),
            terminal_modes: TerminalModes::default(),
            cells: vec![
                Cell::new(
                    'A',
                    CellWidth::Single,
                    Style {
                        foreground: Color::Indexed(196),
                        background: Color::Indexed(17),
                        underline: true,
                        ..Style::default()
                    },
                ),
                Cell::default(),
            ],
        };
        let vertices = build_vertices(
            &snapshot,
            &mut atlas,
            200,
            100,
            TerminalViewport {
                x: 10,
                y: 10,
                width: 190,
                height: 90,
            },
            0..1,
            &TerminalDecorations::default(),
            true,
        );
        assert_eq!(vertices.len(), 24);
        assert!(vertices.iter().all(|vertex| {
            vertex.position[0] >= -1.0
                && vertex.position[0] <= 1.0
                && vertex.position[1] >= -1.0
                && vertex.position[1] <= 1.0
        }));
    }

    #[test]
    fn cursor_shapes_generate_bounded_geometry_and_apply_explicit_color() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        let mut snapshot = FrameSnapshot {
            generation: 1,
            rows: 1,
            cols: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: CursorAppearance::default(),
            terminal_modes: TerminalModes::default(),
            cells: vec![Cell::default()],
        };
        let viewport = TerminalViewport {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let mut render = |appearance, cursor_visible| {
            snapshot.cursor_appearance = appearance;
            build_vertices(
                &snapshot,
                &mut atlas,
                100,
                100,
                viewport,
                0..1,
                &TerminalDecorations::default(),
                cursor_visible,
            )
        };

        assert_eq!(render(CursorAppearance::default(), true).len(), 6);
        assert_eq!(render(CursorAppearance::default(), false).len(), 0);
        assert_eq!(
            render(
                CursorAppearance {
                    shape: CursorShape::Hidden,
                    ..CursorAppearance::default()
                },
                true
            )
            .len(),
            0
        );
        assert_eq!(
            render(
                CursorAppearance {
                    shape: CursorShape::Underline,
                    ..CursorAppearance::default()
                },
                true
            )
            .len(),
            6
        );
        assert_eq!(
            render(
                CursorAppearance {
                    shape: CursorShape::HollowBlock,
                    ..CursorAppearance::default()
                },
                true
            )
            .len(),
            24
        );
        let beam = render(
            CursorAppearance {
                shape: CursorShape::Beam,
                blinking: true,
                color: Some(Color::Rgb(255, 0, 0)),
            },
            true,
        );
        assert_eq!(beam.len(), 6);
        assert!(
            beam.iter()
                .all(|vertex| vertex.color == [1.0, 0.0, 0.0, 1.0])
        );
    }

    #[test]
    fn search_selection_and_cursor_overlays_follow_the_documented_layer_order() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        let point = TerminalCellPoint { row: 0, column: 0 };
        let snapshot = FrameSnapshot {
            generation: 1,
            rows: 1,
            cols: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: CursorAppearance::default(),
            terminal_modes: TerminalModes::default(),
            cells: vec![Cell::default()],
        };
        let decorations = TerminalDecorations {
            revision: 1,
            selection: Some(TerminalSelection {
                anchor: point,
                focus: point,
                mode: TerminalSelectionMode::Character,
            }),
            search: TerminalSearchResult {
                matches: Arc::from([TerminalSearchMatch {
                    start: point,
                    end: point,
                }]),
                truncated: false,
            },
            active_search_match: Some(0),
        };
        let vertices = build_vertices(
            &snapshot,
            &mut atlas,
            100,
            100,
            TerminalViewport {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            0..1,
            &decorations,
            true,
        );

        assert_eq!(vertices.len(), 18);
        assert!(
            vertices[..6]
                .iter()
                .all(|vertex| vertex.color == [1.0, 0.75, 0.15, 0.55])
        );
        assert!(
            vertices[6..12]
                .iter()
                .all(|vertex| vertex.color == [0.20, 0.45, 0.90, 0.50])
        );
        let cursor_color = super::resolve_color(Color::Default, true);
        assert!(
            vertices[12..]
                .iter()
                .all(|vertex| vertex.color == cursor_color)
        );
    }

    #[test]
    fn log_rows_generate_only_visible_styled_unicode_geometry() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        let page = Arc::new(LogPage {
            source_id: LogSourceId(4),
            revision: 1,
            anchor_line_id: 9,
            rows: Arc::from([LogRow {
                line_id: 9,
                text: Arc::from("A中"),
                style_spans: Arc::from([LogStyleSpan {
                    byte_range: 0..1,
                    style: Style {
                        foreground: Color::Indexed(196),
                        background: Color::Indexed(17),
                        underline: true,
                        ..Style::default()
                    },
                }]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        });
        let viewport = TerminalViewport {
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        };
        let frame = LogSurfaceFrame {
            page,
            visual_rows: Arc::from([LogVisualRow {
                logical_row_index: 0,
                start_byte: 0,
                end_byte: 4,
                start_cell: 0,
                end_cell: u32::MAX,
            }]),
            layout_columns: 0,
            visible_rows: 0..1,
            first_viewport_row: 0,
            cell_offset: 0,
            new_lines_available: 0,
        };

        let vertices = build_log_vertices(
            &frame,
            &mut atlas,
            200,
            100,
            viewport,
            &LogDecorations::default(),
        );
        assert_eq!(vertices.len(), 24);

        let highlighted = build_log_vertices(
            &frame,
            &mut atlas,
            200,
            100,
            viewport,
            &LogDecorations {
                revision: 1,
                active_search_match: Some(LogSearchMatch {
                    line_id: 9,
                    byte_range: 1..4,
                }),
            },
        );
        assert_eq!(highlighted.len(), 30);
        assert!(highlighted.chunks_exact(6).any(|quad| {
            quad.iter()
                .all(|vertex| vertex.color == [1.0, 0.72, 0.12, 0.92])
        }));

        let clipped = LogSurfaceFrame {
            cell_offset: 1,
            ..frame
        };
        let vertices = build_log_vertices(
            &clipped,
            &mut atlas,
            200,
            100,
            viewport,
            &LogDecorations::default(),
        );
        assert_eq!(vertices.len(), 6);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn wrapped_log_geometry_scans_each_visual_text_slice_once() {
        let page = Arc::new(LogPage {
            source_id: LogSourceId(5),
            revision: 1,
            anchor_line_id: 1,
            rows: Arc::from([LogRow {
                line_id: 1,
                text: Arc::from("ABCD"),
                style_spans: Arc::from([]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        });
        let mut surface = LogSurfaceModel::new(0);
        surface.submit_page(page).unwrap();
        let layout = surface.request_reflow(2).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));
        let frame = surface.prepare_frame(2).unwrap();
        assert_eq!(frame.visual_rows.len(), 2);

        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        let vertices = build_log_vertices(
            &frame,
            &mut atlas,
            200,
            100,
            TerminalViewport {
                x: 0,
                y: 0,
                width: 200,
                height: 100,
            },
            &LogDecorations::default(),
        );
        assert_eq!(vertices.len(), 24);
    }

    #[test]
    fn xterm_palette_covers_ansi_cube_and_grayscale() {
        assert_eq!(xterm_color(1), [205, 49, 49]);
        assert_eq!(xterm_color(16), [0, 0, 0]);
        assert_eq!(xterm_color(231), [255, 255, 255]);
        assert_eq!(xterm_color(232), [8, 8, 8]);
        assert_eq!(xterm_color(255), [238, 238, 238]);
    }

    #[test]
    fn cosmic_text_shapes_and_caches_a_combining_cell() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        let cell = Cell::with_zerowidth('e', ['\u{301}'], CellWidth::Single, Style::default());
        let first = atlas.shape_cell(&cell);
        assert!(!first.is_empty());
        assert!(
            first
                .iter()
                .all(|glyph| glyph.width > 0 && glyph.height > 0)
        );
        let revision = atlas.revision;
        assert!(revision > 0);
        let Some(upload) = atlas.take_dirty_upload() else {
            panic!("new glyph should be dirty");
        };
        assert_eq!(
            upload.bytes.len(),
            (upload.size[0] * upload.size[1] * ATLAS_BYTES_PER_PIXEL) as usize
        );
        assert!(upload.size[0] < super::ATLAS_WIDTH);
        assert!(upload.size[1] < super::ATLAS_HEIGHT);
        assert_eq!(atlas.shape_cell(&cell), first);
        assert_eq!(atlas.revision, revision);
        assert!(atlas.take_dirty_upload().is_none());
    }

    #[test]
    fn shaping_cache_rasterizes_a_glyph_again_after_atlas_eviction() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        let cell = Cell::with_zerowidth('e', ['\u{301}'], CellWidth::Single, Style::default());
        assert!(!atlas.shape_cell(&cell).is_empty());
        let Some(cache_key) = atlas
            .shaped_cells
            .values()
            .flat_map(|glyphs| glyphs.iter().map(|glyph| glyph.cache_key))
            .next()
        else {
            panic!("combining cell should produce a shaped glyph");
        };
        let Some(evicted) = atlas.dynamic_glyphs.remove(&cache_key) else {
            panic!("shaped glyph should have an atlas allocation");
        };
        atlas.dynamic_allocator.deallocate(evicted.allocation_id);
        atlas.begin_frame();
        let revision = atlas.revision;

        assert!(!atlas.shape_cell(&cell).is_empty());
        assert!(atlas.dynamic_glyphs.contains_key(&cache_key));
        assert!(atlas.revision > revision);
    }

    #[test]
    fn dirty_glyph_regions_merge_into_one_compact_upload() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        atlas.mark_dirty(10, 20, 3, 4);
        atlas.mark_dirty(20, 15, 2, 2);

        let Some(upload) = atlas.take_dirty_upload() else {
            panic!("dirty atlas upload");
        };
        assert_eq!(upload.origin, [10, 15]);
        assert_eq!(upload.size, [12, 9]);
        assert_eq!(upload.bytes.len(), 12 * 9 * 4);
        assert!(atlas.take_dirty_upload().is_none());
    }

    #[test]
    fn dynamic_atlas_evicts_the_oldest_unpinned_glyph() {
        let mut atlas = GlyphAtlas::build().unwrap_or_else(|error| panic!("{error}"));
        atlas.dynamic_allocator = AtlasAllocator::new(size2(30, 10));
        let keys = [cache_key(1), cache_key(2), cache_key(3)];
        for (index, key) in keys.into_iter().enumerate() {
            let Some(allocation) = atlas.dynamic_allocator.allocate(size2(10, 10)) else {
                panic!("test atlas should fit three allocations");
            };
            atlas.dynamic_glyphs.insert(
                key,
                CachedGlyph {
                    glyph: GlyphInfo::default(),
                    allocation_id: allocation.id,
                    last_used: index as u64,
                },
            );
        }
        atlas.pinned_glyphs.insert(keys[0]);

        let Some(_replacement) = atlas.allocate_dynamic(10, 10) else {
            panic!("an unpinned glyph should be evicted");
        };
        assert!(atlas.dynamic_glyphs.contains_key(&keys[0]));
        assert!(!atlas.dynamic_glyphs.contains_key(&keys[1]));
        assert!(atlas.dynamic_glyphs.contains_key(&keys[2]));
    }

    #[test]
    fn full_atlas_pressure_preserves_bounds_and_rerasterizes() {
        let report = super::benchmark_atlas_pressure(8).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(report.atlas_bytes, (2048 * 2048 * 4) as usize);
        assert!(report.dynamic_capacity_glyphs > 0);
        assert!(report.atlas_evictions >= 8);
        assert!(report.pinned_glyph_survived);
        assert!(report.rerasterized_after_eviction);
        assert!(report.upload_integrity);
        assert!(report.max_upload_bytes < report.atlas_bytes);
    }

    #[test]
    fn swash_pixels_preserve_color_and_convert_masks_to_white_alpha() {
        assert_eq!(
            atlas_pixel(SwashContent::Mask, &[17], 0),
            [255, 255, 255, 17]
        );
        assert_eq!(
            atlas_pixel(SwashContent::Color, &[10, 20, 30, 40], 0),
            [10, 20, 30, 40]
        );
        assert_eq!(
            atlas_pixel(SwashContent::SubpixelMask, &[10, 80, 30, 0], 0),
            [255, 255, 255, 80]
        );
    }

    #[test]
    fn logical_terminal_rect_converts_to_an_outward_rounded_physical_viewport() {
        let viewport = TerminalViewport::from_logical_rect(
            egui::Rect::from_min_max(egui::pos2(10.25, 20.5), egui::pos2(100.1, 80.1)),
            2.0,
        );
        assert_eq!(
            viewport,
            TerminalViewport {
                x: 20,
                y: 41,
                width: 181,
                height: 120,
            }
        );
        assert_eq!(
            viewport.clamp(150, 100),
            TerminalViewport {
                x: 20,
                y: 41,
                width: 130,
                height: 59,
            }
        );
    }
}
