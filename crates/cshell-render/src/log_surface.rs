use crate::ScrollMode;
use cshell_terminal::Style;
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const MAX_LOG_PAGE_ROWS: usize = 4096;
pub const MAX_LOG_PAGE_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_LOG_PAGE_STYLE_SPANS: usize = 16 * 1024;
pub const MAX_LOG_REFLOW_ROWS: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LogSourceId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogRow {
    pub line_id: u64,
    pub text: Arc<str>,
    pub style_spans: Arc<[LogStyleSpan]>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogStyleSpan {
    pub byte_range: Range<u32>,
    pub style: Style,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogPage {
    pub source_id: LogSourceId,
    pub revision: u64,
    pub anchor_line_id: u64,
    pub rows: Arc<[LogRow]>,
    pub total_line_count: u64,
    pub has_before: bool,
    pub has_after: bool,
}

#[derive(Clone, Debug)]
pub struct LogSurfaceFrame {
    pub page: Arc<LogPage>,
    pub visual_rows: Arc<[LogVisualRow]>,
    pub layout_columns: u16,
    pub visible_rows: Range<usize>,
    /// Index inside `visible_rows` corresponding to the first on-screen row.
    /// Rows before it are overscan and must be placed above the viewport.
    pub first_viewport_row: usize,
    pub cell_offset: u32,
    pub new_lines_available: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogVisualRow {
    pub logical_row_index: u32,
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_cell: u32,
    pub end_cell: u32,
}

#[derive(Clone, Debug)]
pub struct LogReflowRequest {
    page: Arc<LogPage>,
    columns: u16,
}

#[derive(Clone, Debug)]
pub struct LogReflowLayout {
    page: Arc<LogPage>,
    columns: u16,
    visual_rows: Arc<[LogVisualRow]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReflowKey {
    source_id: LogSourceId,
    revision: u64,
    columns: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogPageRequest {
    pub source_id: LogSourceId,
    pub follow_tail: bool,
    pub anchor_line_id: u64,
    pub cell_offset: u32,
    pub rows_before: u16,
    pub rows_after: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogScrollbarState {
    pub top_line_id: u64,
    pub total_line_count: u64,
    pub viewport_rows: u16,
    pub follow_tail: bool,
}

#[derive(Clone, Debug)]
pub struct LogSurfaceModel {
    page: Option<Arc<LogPage>>,
    layout: Option<Arc<LogReflowLayout>>,
    mode: ScrollMode,
    overscan_rows: u16,
    new_lines_available: u64,
    desired_columns: Option<u16>,
    pending_reflow: Option<ReflowKey>,
    failed_reflow: Option<ReflowKey>,
    pending_scroll_rows: i32,
    last_viewport_rows: u16,
    pending_page_anchor: Option<(u64, u32)>,
    page_request_dirty: bool,
    last_page_request: Option<LogPageRequest>,
}

impl Default for LogSurfaceModel {
    fn default() -> Self {
        Self::new(2)
    }
}

impl LogSurfaceModel {
    #[must_use]
    pub const fn new(overscan_rows: u16) -> Self {
        Self {
            page: None,
            layout: None,
            mode: ScrollMode::FollowTail,
            overscan_rows,
            new_lines_available: 0,
            desired_columns: None,
            pending_reflow: None,
            failed_reflow: None,
            pending_scroll_rows: 0,
            last_viewport_rows: 0,
            pending_page_anchor: None,
            page_request_dirty: false,
            last_page_request: None,
        }
    }

    #[must_use]
    pub const fn mode(&self) -> ScrollMode {
        self.mode
    }

    #[must_use]
    pub const fn new_lines_available(&self) -> u64 {
        self.new_lines_available
    }

    pub fn follow_tail(&mut self) {
        self.page_request_dirty |=
            !matches!(self.mode, ScrollMode::FollowTail) || self.pending_page_anchor.is_some();
        self.mode = ScrollMode::FollowTail;
        self.new_lines_available = 0;
        self.pending_scroll_rows = 0;
        self.pending_page_anchor = None;
    }

    pub fn anchor(&mut self, line_id: u64, cell_offset: u32) {
        self.page_request_dirty |=
            matches!(self.mode, ScrollMode::FollowTail) || self.pending_page_anchor.is_some();
        self.mode = ScrollMode::Anchored {
            line_id,
            cell_offset,
        };
        self.new_lines_available = 0;
        self.pending_scroll_rows = 0;
        self.pending_page_anchor = None;
    }

    /// Requests an arbitrary logical line without blanking the currently
    /// visible page. The anchor becomes active only after its page and reflow
    /// layout are both ready.
    pub fn jump_to_line(&mut self, line_id: u64, cell_offset: u32) -> bool {
        let Some(page) = self.page.as_ref() else {
            return false;
        };
        if page.total_line_count == 0 {
            return false;
        }
        let line_id = line_id.clamp(1, page.total_line_count);
        if self
            .layout
            .as_ref()
            .and_then(|layout| layout.visual_row_for_anchor(line_id, cell_offset))
            .is_some()
        {
            self.anchor(line_id, cell_offset);
        } else {
            self.pending_page_anchor = Some((line_id, cell_offset));
            self.pending_scroll_rows = 0;
            self.page_request_dirty = true;
        }
        true
    }

    /// Moves the top of the viewport by visual rows. Positive values move
    /// toward the tail; negative values move into history.
    pub fn scroll_visual_rows(&mut self, delta: i32, viewport_rows: u16) -> bool {
        self.last_viewport_rows = viewport_rows;
        self.pending_page_anchor = None;
        if delta == 0 {
            return false;
        }
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        if layout.visual_rows.is_empty() {
            return false;
        }
        let visual_row_count = layout.visual_rows.len();
        let has_before = layout.page.has_before;
        let has_after = layout.page.has_after;
        let current = match self.mode {
            ScrollMode::FollowTail => layout
                .visual_rows
                .len()
                .saturating_sub(usize::from(viewport_rows)),
            ScrollMode::Anchored {
                line_id,
                cell_offset,
            } => layout
                .visual_row_for_anchor(line_id, cell_offset)
                .unwrap_or(0),
        };
        let target = current as i64 + i64::from(delta);
        if target < 0 {
            self.set_anchor_from_visual(0);
            self.pending_scroll_rows = target.max(i64::from(i32::MIN)) as i32;
            if !has_before {
                self.pending_scroll_rows = 0;
            }
            return true;
        }
        let target = target as usize;
        if target >= visual_row_count {
            if has_after {
                self.set_anchor_from_visual(visual_row_count - 1);
                self.pending_scroll_rows = target
                    .saturating_sub(visual_row_count - 1)
                    .min(i32::MAX as usize) as i32;
            } else {
                self.follow_tail();
            }
            return true;
        }
        if !has_after && target.saturating_add(usize::from(viewport_rows)) >= visual_row_count {
            self.follow_tail();
        } else {
            self.set_anchor_from_visual(target);
            self.pending_scroll_rows = 0;
        }
        true
    }

    fn set_anchor_from_visual(&mut self, visual_index: usize) {
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let Some(visual) = layout.visual_rows.get(visual_index) else {
            return;
        };
        let Some(row) = layout.page.rows.get(visual.logical_row_index as usize) else {
            return;
        };
        self.page_request_dirty |= matches!(self.mode, ScrollMode::FollowTail);
        self.mode = ScrollMode::Anchored {
            line_id: row.line_id,
            cell_offset: visual.start_cell,
        };
        self.new_lines_available = 0;
    }

    /// Accepts a bounded immutable page without copying its rows. A delayed page
    /// is ignored. While anchored, a replacement page must still contain the
    /// anchor so asynchronous disk reads cannot move the viewport.
    pub fn submit_page(&mut self, page: Arc<LogPage>) -> Result<bool, LogSurfaceError> {
        validate_page(&page)?;
        let first_page = self.page.is_none();
        if let Some(current) = &self.page
            && page.source_id == current.source_id
            && page.revision <= current.revision
        {
            return Ok(false);
        }

        let source_changed = self
            .page
            .as_ref()
            .is_some_and(|current| current.source_id != page.source_id);
        if source_changed {
            self.follow_tail();
        }

        let required_anchor = self.pending_page_anchor.or(match self.mode {
            ScrollMode::Anchored {
                line_id,
                cell_offset,
            } => Some((line_id, cell_offset)),
            ScrollMode::FollowTail => None,
        });
        if let Some((line_id, _)) = required_anchor
            && page
                .rows
                .binary_search_by_key(&line_id, |row| row.line_id)
                .is_err()
        {
            return Err(LogSurfaceError::AnchorMissing(line_id));
        }

        if matches!(self.mode, ScrollMode::Anchored { .. }) {
            let previous_total = self
                .page
                .as_ref()
                .filter(|current| current.source_id == page.source_id)
                .map_or(0, |current| current.total_line_count);
            self.new_lines_available = self
                .new_lines_available
                .saturating_add(page.total_line_count.saturating_sub(previous_total));
        } else {
            self.new_lines_available = 0;
        }
        let install_identity =
            source_changed || self.layout.is_none() || self.desired_columns.is_none();
        if install_identity {
            self.layout = Some(Arc::new(LogReflowLayout::identity(Arc::clone(&page))?));
        }
        self.page = Some(page);
        self.page_request_dirty |= first_page || source_changed;
        Ok(true)
    }

    /// Returns at most one in-flight background reflow request. The current
    /// layout remains renderable until the matching result is submitted.
    #[must_use]
    pub fn request_reflow(&mut self, columns: u16) -> Option<LogReflowRequest> {
        let columns = columns.max(1);
        self.desired_columns = Some(columns);
        let page = self.page.as_ref()?.clone();
        let key = ReflowKey::new(&page, columns);
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.key() == key)
            || self.pending_reflow.is_some()
            || self.failed_reflow == Some(key)
        {
            return None;
        }
        self.pending_reflow = Some(key);
        Some(LogReflowRequest { page, columns })
    }

    /// Atomically promotes a background layout only when it still matches the
    /// latest page and viewport width. Delayed resize results are discarded.
    pub fn submit_reflow(&mut self, layout: LogReflowLayout) -> bool {
        let key = layout.key();
        if self.pending_reflow == Some(key) {
            self.pending_reflow = None;
        }
        let is_current = self
            .page
            .as_ref()
            .is_some_and(|page| ReflowKey::new(page, layout.columns) == key)
            && self.desired_columns == Some(layout.columns);
        if !is_current {
            return false;
        }
        self.layout = Some(Arc::new(layout));
        self.failed_reflow = None;
        if let Some((line_id, cell_offset)) = self.pending_page_anchor
            && self
                .layout
                .as_ref()
                .and_then(|layout| layout.visual_row_for_anchor(line_id, cell_offset))
                .is_some()
        {
            self.mode = ScrollMode::Anchored {
                line_id,
                cell_offset,
            };
            self.pending_page_anchor = None;
            self.new_lines_available = 0;
        }
        let pending_scroll = std::mem::take(&mut self.pending_scroll_rows);
        if pending_scroll != 0 {
            self.scroll_visual_rows(pending_scroll, self.last_viewport_rows);
        }
        true
    }

    /// Releases the single-flight gate and suppresses retries for the same
    /// pathological page/width pair. A new page or width may still retry.
    pub fn reflow_failed(&mut self) {
        self.failed_reflow = self.pending_reflow.take();
    }

    #[must_use]
    pub fn page_request(&self, viewport_rows: u16) -> Option<LogPageRequest> {
        let page = self.page.as_ref()?;
        let tail_rows = viewport_rows
            .saturating_add(self.overscan_rows)
            .min((MAX_LOG_PAGE_ROWS - 1) as u16);
        let anchored_rows = tail_rows.min(((MAX_LOG_PAGE_ROWS - 1) / 2) as u16);
        let request_mode = self
            .pending_page_anchor
            .map(|(line_id, cell_offset)| ScrollMode::Anchored {
                line_id,
                cell_offset,
            })
            .unwrap_or(self.mode);
        let (anchor_line_id, cell_offset, rows_before, rows_after) = match request_mode {
            ScrollMode::FollowTail => (page.rows.last()?.line_id, 0, tail_rows, 0),
            ScrollMode::Anchored {
                line_id,
                cell_offset,
            } => (line_id, cell_offset, anchored_rows, anchored_rows),
        };
        Some(LogPageRequest {
            source_id: page.source_id,
            follow_tail: matches!(request_mode, ScrollMode::FollowTail),
            anchor_line_id,
            cell_offset,
            rows_before,
            rows_after,
        })
    }

    /// Emits a paging command only when the mode changed, an off-page jump is
    /// pending, or the anchor approaches a loaded-page boundary.
    pub fn take_page_request(&mut self, viewport_rows: u16) -> Option<LogPageRequest> {
        let request = self.page_request(viewport_rows)?;
        let pending = self.pending_page_anchor.is_some() || self.pending_scroll_rows != 0;
        let margin = usize::from((viewport_rows / 2).saturating_add(self.overscan_rows));
        let near_boundary = if request.follow_tail {
            false
        } else {
            self.page
                .as_ref()
                .and_then(|page| {
                    page.rows
                        .binary_search_by_key(&request.anchor_line_id, |row| row.line_id)
                        .ok()
                        .map(|index| {
                            (page.has_before && index <= margin)
                                || (page.has_after
                                    && page.rows.len().saturating_sub(index + 1) <= margin)
                        })
                })
                .unwrap_or(true)
        };
        if !(self.page_request_dirty || pending || near_boundary) {
            return None;
        }
        self.page_request_dirty = false;
        if self.last_page_request == Some(request) {
            return None;
        }
        self.last_page_request = Some(request);
        Some(request)
    }

    #[must_use]
    pub fn scrollbar_state(&self, viewport_rows: u16) -> Option<LogScrollbarState> {
        let layout = self.layout.as_ref()?;
        let top_line_id = match self.mode {
            ScrollMode::FollowTail => layout.page.total_line_count,
            ScrollMode::Anchored {
                line_id,
                cell_offset,
            } => {
                layout
                    .visual_row_for_anchor(line_id, cell_offset)
                    .and_then(|index| layout.visual_rows.get(index))
                    .and_then(|visual| layout.page.rows.get(visual.logical_row_index as usize))?
                    .line_id
            }
        };
        Some(LogScrollbarState {
            top_line_id,
            total_line_count: self.page.as_ref()?.total_line_count,
            viewport_rows,
            follow_tail: matches!(self.mode, ScrollMode::FollowTail),
        })
    }

    #[must_use]
    pub fn prepare_frame(&self, viewport_rows: u16) -> Option<LogSurfaceFrame> {
        let layout = self.layout.as_ref()?;
        let page = Arc::clone(&layout.page);
        let (first_viewport, cell_offset) = match self.mode {
            ScrollMode::FollowTail => (
                layout
                    .visual_rows
                    .len()
                    .saturating_sub(usize::from(viewport_rows)),
                0,
            ),
            ScrollMode::Anchored {
                line_id,
                cell_offset,
            } => (
                layout.visual_row_for_anchor(line_id, cell_offset)?,
                cell_offset,
            ),
        };
        let start = first_viewport.saturating_sub(usize::from(self.overscan_rows));
        let end = first_viewport
            .saturating_add(usize::from(viewport_rows))
            .saturating_add(usize::from(self.overscan_rows))
            .min(layout.visual_rows.len());
        Some(LogSurfaceFrame {
            page,
            visual_rows: Arc::clone(&layout.visual_rows),
            layout_columns: layout.columns,
            visible_rows: start..end,
            first_viewport_row: first_viewport.saturating_sub(start),
            cell_offset,
            new_lines_available: self.new_lines_available,
        })
    }
}

impl LogReflowRequest {
    #[must_use]
    pub const fn columns(&self) -> u16 {
        self.columns
    }

    pub fn execute(self) -> Result<LogReflowLayout, LogSurfaceError> {
        LogReflowLayout::reflow(self.page, self.columns)
    }
}

impl ReflowKey {
    fn new(page: &LogPage, columns: u16) -> Self {
        Self {
            source_id: page.source_id,
            revision: page.revision,
            columns,
        }
    }
}

impl LogReflowLayout {
    fn identity(page: Arc<LogPage>) -> Result<Self, LogSurfaceError> {
        let visual_rows = page
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| LogVisualRow {
                logical_row_index: index as u32,
                start_byte: 0,
                end_byte: row.text.len() as u32,
                start_cell: 0,
                end_cell: u32::MAX,
            })
            .collect::<Vec<_>>()
            .into();
        Ok(Self {
            page,
            columns: 0,
            visual_rows,
        })
    }

    fn reflow(page: Arc<LogPage>, columns: u16) -> Result<Self, LogSurfaceError> {
        let columns = u32::from(columns.max(1));
        let mut visual_rows = Vec::new();
        for (logical_row_index, row) in page.rows.iter().enumerate() {
            let mut start_byte = 0_usize;
            let mut start_cell = 0_u32;
            let mut cell = 0_u32;
            let mut segment_width = 0_u32;
            for (byte, grapheme) in row.text.grapheme_indices(true) {
                let width = if grapheme == "\t" {
                    8 - cell % 8
                } else {
                    UnicodeWidthStr::width(grapheme).clamp(1, 2) as u32
                };
                if segment_width > 0 && segment_width.saturating_add(width) > columns {
                    push_visual_row(
                        &mut visual_rows,
                        logical_row_index,
                        start_byte,
                        byte,
                        start_cell,
                        cell,
                    )?;
                    start_byte = byte;
                    start_cell = cell;
                    segment_width = 0;
                }
                cell = cell
                    .checked_add(width)
                    .ok_or(LogSurfaceError::LineTooWide(row.line_id))?;
                segment_width = segment_width.saturating_add(width);
            }
            push_visual_row(
                &mut visual_rows,
                logical_row_index,
                start_byte,
                row.text.len(),
                start_cell,
                cell,
            )?;
        }
        Ok(Self {
            columns: columns as u16,
            page,
            visual_rows: visual_rows.into(),
        })
    }

    fn key(&self) -> ReflowKey {
        ReflowKey::new(&self.page, self.columns)
    }

    fn visual_row_for_anchor(&self, line_id: u64, cell_offset: u32) -> Option<usize> {
        let logical_row = self
            .page
            .rows
            .binary_search_by_key(&line_id, |row| row.line_id)
            .ok()? as u32;
        let range = self
            .visual_rows
            .partition_point(|row| row.logical_row_index < logical_row)
            ..self
                .visual_rows
                .partition_point(|row| row.logical_row_index <= logical_row);
        let rows = self.visual_rows.get(range.clone())?;
        rows.iter()
            .position(|row| {
                (row.start_cell <= cell_offset && cell_offset < row.end_cell)
                    || (row.start_cell == row.end_cell && cell_offset == row.start_cell)
            })
            .map(|position| range.start + position)
            .or_else(|| range.end.checked_sub(1))
    }
}

fn push_visual_row(
    rows: &mut Vec<LogVisualRow>,
    logical_row_index: usize,
    start_byte: usize,
    end_byte: usize,
    start_cell: u32,
    end_cell: u32,
) -> Result<(), LogSurfaceError> {
    if rows.len() >= MAX_LOG_REFLOW_ROWS {
        return Err(LogSurfaceError::TooManyReflowRows(
            rows.len().saturating_add(1),
        ));
    }
    rows.push(LogVisualRow {
        logical_row_index: logical_row_index as u32,
        start_byte: start_byte as u32,
        end_byte: end_byte as u32,
        start_cell,
        end_cell,
    });
    Ok(())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LogSurfaceError {
    #[error("log page exceeds the {MAX_LOG_PAGE_ROWS}-row safety limit: {0}")]
    TooManyRows(usize),
    #[error("log page rows are not strictly ordered by line id")]
    UnorderedRows,
    #[error("log page total line count {total} is smaller than its {page_rows} rows")]
    InvalidTotal { total: u64, page_rows: usize },
    #[error("log page text exceeds the {MAX_LOG_PAGE_TEXT_BYTES}-byte safety limit: {0}")]
    TooManyTextBytes(usize),
    #[error("log page exceeds the {MAX_LOG_PAGE_STYLE_SPANS}-style-span safety limit: {0}")]
    TooManyStyleSpans(usize),
    #[error("log row {line_id} contains a line break or unsupported control character")]
    InvalidRowText { line_id: u64 },
    #[error("log row {line_id} contains an invalid, overlapping, or non-grapheme style range")]
    InvalidStyleSpan { line_id: u64 },
    #[error("log page does not contain its declared anchor line {0}")]
    DeclaredAnchorMissing(u64),
    #[error("replacement log page does not contain viewport anchor line {0}")]
    AnchorMissing(u64),
    #[error("logical log line {0} is too wide to address with a 32-bit cell offset")]
    LineTooWide(u64),
    #[error("reflow exceeds the {MAX_LOG_REFLOW_ROWS}-visual-row safety limit: {0}")]
    TooManyReflowRows(usize),
}

fn validate_page(page: &LogPage) -> Result<(), LogSurfaceError> {
    if page.rows.len() > MAX_LOG_PAGE_ROWS {
        return Err(LogSurfaceError::TooManyRows(page.rows.len()));
    }
    if page
        .rows
        .windows(2)
        .any(|rows| rows[0].line_id >= rows[1].line_id)
    {
        return Err(LogSurfaceError::UnorderedRows);
    }
    if page.total_line_count < page.rows.len() as u64 {
        return Err(LogSurfaceError::InvalidTotal {
            total: page.total_line_count,
            page_rows: page.rows.len(),
        });
    }
    let text_bytes = page
        .rows
        .iter()
        .try_fold(0_usize, |total, row| total.checked_add(row.text.len()))
        .unwrap_or(usize::MAX);
    if text_bytes > MAX_LOG_PAGE_TEXT_BYTES {
        return Err(LogSurfaceError::TooManyTextBytes(text_bytes));
    }
    let style_span_count = page
        .rows
        .iter()
        .try_fold(0_usize, |total, row| {
            total.checked_add(row.style_spans.len())
        })
        .unwrap_or(usize::MAX);
    if style_span_count > MAX_LOG_PAGE_STYLE_SPANS {
        return Err(LogSurfaceError::TooManyStyleSpans(style_span_count));
    }
    for row in page.rows.iter() {
        if row
            .text
            .chars()
            .any(|character| character.is_control() && character != '\t')
        {
            return Err(LogSurfaceError::InvalidRowText {
                line_id: row.line_id,
            });
        }
        let grapheme_boundaries: std::collections::HashSet<_> = row
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(row.text.len()))
            .collect();
        let mut previous_end = 0_usize;
        for span in row.style_spans.iter() {
            let start = span.byte_range.start as usize;
            let end = span.byte_range.end as usize;
            if start >= end
                || start < previous_end
                || !grapheme_boundaries.contains(&start)
                || !grapheme_boundaries.contains(&end)
            {
                return Err(LogSurfaceError::InvalidStyleSpan {
                    line_id: row.line_id,
                });
            }
            previous_end = end;
        }
    }
    if !page.rows.is_empty()
        && page
            .rows
            .binary_search_by_key(&page.anchor_line_id, |row| row.line_id)
            .is_err()
    {
        return Err(LogSurfaceError::DeclaredAnchorMissing(page.anchor_line_id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LogPage, LogRow, LogSourceId, LogStyleSpan, LogSurfaceError, LogSurfaceModel,
        MAX_LOG_PAGE_ROWS, MAX_LOG_PAGE_TEXT_BYTES,
    };
    use crate::ScrollMode;
    use cshell_terminal::{Color, Style};
    use std::sync::Arc;

    fn page(revision: u64, ids: std::ops::Range<u64>, total: u64) -> Arc<LogPage> {
        let rows: Arc<[LogRow]> = ids
            .map(|line_id| LogRow {
                line_id,
                text: Arc::from(format!("line {line_id}")),
                style_spans: Arc::from([]),
                truncated: false,
            })
            .collect::<Vec<_>>()
            .into();
        Arc::new(LogPage {
            source_id: LogSourceId(7),
            revision,
            anchor_line_id: rows.first().map_or(0, |row| row.line_id),
            rows,
            total_line_count: total,
            has_before: true,
            has_after: false,
        })
    }

    #[test]
    fn follow_tail_plans_only_viewport_and_overscan_without_copying_rows() {
        let page = page(1, 100..120, 120);
        let mut surface = LogSurfaceModel::new(2);
        assert!(
            surface
                .submit_page(Arc::clone(&page))
                .unwrap_or_else(|error| panic!("{error}"))
        );

        let frame = surface
            .prepare_frame(5)
            .unwrap_or_else(|| panic!("submitted page should render"));
        assert!(Arc::ptr_eq(&frame.page, &page));
        assert_eq!(frame.visible_rows, 13..20);
        assert_eq!(frame.first_viewport_row, 2);
        assert_eq!(frame.page.rows[15].line_id, 115);
    }

    #[test]
    fn anchored_view_does_not_move_when_new_tail_lines_arrive() {
        let mut surface = LogSurfaceModel::new(1);
        surface
            .submit_page(page(1, 100..120, 120))
            .unwrap_or_else(|error| panic!("{error}"));
        surface.anchor(105, 17);
        let before = surface
            .prepare_frame(4)
            .unwrap_or_else(|| panic!("anchor should be visible"));

        surface
            .submit_page(page(2, 100..120, 130))
            .unwrap_or_else(|error| panic!("{error}"));
        let after = surface
            .prepare_frame(4)
            .unwrap_or_else(|| panic!("anchor should remain visible"));
        assert_eq!(
            surface.mode(),
            ScrollMode::Anchored {
                line_id: 105,
                cell_offset: 17,
            }
        );
        assert_eq!(before.visible_rows, after.visible_rows);
        assert_eq!(after.page.rows[5].line_id, 105);
        assert_eq!(after.cell_offset, 17);
        assert_eq!(after.new_lines_available, 10);
    }

    #[test]
    fn page_missing_the_active_anchor_is_rejected_without_replacing_current_data() {
        let original = page(1, 100..120, 120);
        let mut surface = LogSurfaceModel::default();
        surface
            .submit_page(Arc::clone(&original))
            .unwrap_or_else(|error| panic!("{error}"));
        surface.anchor(105, 0);

        assert_eq!(
            surface.submit_page(page(2, 200..220, 220)),
            Err(LogSurfaceError::AnchorMissing(105))
        );
        let frame = surface
            .prepare_frame(5)
            .unwrap_or_else(|| panic!("original page should remain available"));
        assert!(Arc::ptr_eq(&frame.page, &original));
    }

    #[test]
    fn stale_pages_are_ignored_and_source_changes_return_to_tail() {
        let mut surface = LogSurfaceModel::default();
        surface
            .submit_page(page(3, 10..20, 20))
            .unwrap_or_else(|error| panic!("{error}"));
        surface.anchor(12, 0);
        assert!(
            !surface
                .submit_page(page(2, 10..20, 20))
                .unwrap_or_else(|error| panic!("{error}"))
        );

        let mut replacement = (*page(1, 500..510, 510)).clone();
        replacement.source_id = LogSourceId(8);
        surface
            .submit_page(Arc::new(replacement))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(surface.mode(), ScrollMode::FollowTail);
        assert_eq!(surface.new_lines_available(), 0);
    }

    #[test]
    fn page_request_keeps_stable_line_and_cell_anchor() {
        let mut surface = LogSurfaceModel::new(3);
        surface
            .submit_page(page(1, 40..60, 60))
            .unwrap_or_else(|error| panic!("{error}"));
        surface.anchor(47, 9);
        let request = surface
            .page_request(20)
            .unwrap_or_else(|| panic!("loaded source should produce a request"));
        assert_eq!(request.source_id, LogSourceId(7));
        assert!(!request.follow_tail);
        assert_eq!(request.anchor_line_id, 47);
        assert_eq!(request.cell_offset, 9);
        assert_eq!(request.rows_before, 23);
        assert_eq!(request.rows_after, 23);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn paging_requests_are_coalesced_until_the_view_nears_a_page_boundary() {
        let mut surface = LogSurfaceModel::new(10);
        surface.submit_page(page(1, 100..300, 300)).unwrap();
        let layout = surface.request_reflow(80).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));

        let initial = surface.take_page_request(10).unwrap();
        assert!(initial.follow_tail);
        assert!(surface.take_page_request(10).is_none());

        assert!(surface.scroll_visual_rows(-80, 10));
        let anchored = surface.take_page_request(10).unwrap();
        assert!(!anchored.follow_tail);
        assert_eq!(anchored.anchor_line_id, 210);

        // Scrolling inside the already-loaded page is entirely local.
        assert!(surface.scroll_visual_rows(-1, 10));
        assert!(surface.take_page_request(10).is_none());

        // Approaching an edge asks the journal for the adjacent bounded page.
        surface.anchor(103, 0);
        let boundary = surface.take_page_request(10).unwrap();
        assert_eq!(boundary.anchor_line_id, 103);
        assert!(!boundary.follow_tail);
        assert!(surface.take_page_request(10).is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn resize_storm_and_continuous_scroll_preserve_the_visual_anchor() {
        let mut wrapped = (*page(1, 100..180, 500)).clone();
        wrapped.rows = wrapped
            .rows
            .iter()
            .map(|row| LogRow {
                line_id: row.line_id,
                text: Arc::from(format!("line {:03} {}", row.line_id, "x".repeat(48))),
                style_spans: Arc::from([]),
                truncated: false,
            })
            .collect::<Vec<_>>()
            .into();
        wrapped.anchor_line_id = 140;
        wrapped.has_after = true;

        let mut surface = LogSurfaceModel::new(2);
        surface.submit_page(Arc::new(wrapped)).unwrap();
        let initial = surface.request_reflow(20).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(initial));
        surface.anchor(140, 0);

        for cycle in 0..32 {
            let (stale_columns, latest_columns) = if cycle % 2 == 0 { (9, 13) } else { (11, 7) };
            let stale = surface
                .request_reflow(stale_columns)
                .unwrap()
                .execute()
                .unwrap();
            assert!(surface.request_reflow(latest_columns).is_none());
            let old_rows = surface.prepare_frame(8).unwrap().visual_rows;
            assert!(!surface.submit_reflow(stale));
            assert!(Arc::ptr_eq(
                &surface.prepare_frame(8).unwrap().visual_rows,
                &old_rows
            ));

            let latest = surface
                .request_reflow(latest_columns)
                .unwrap()
                .execute()
                .unwrap();
            assert!(surface.submit_reflow(latest));
            let frame = surface.prepare_frame(8).unwrap();
            assert_eq!(frame.layout_columns, latest_columns);
            let first_visual =
                frame.visual_rows[frame.visible_rows.start + frame.first_viewport_row];
            let first_line = frame.page.rows[first_visual.logical_row_index as usize].line_id;
            let ScrollMode::Anchored { line_id, .. } = surface.mode() else {
                panic!("resize must not return an anchored view to follow-tail mode");
            };
            assert_eq!(first_line, line_id);

            let delta = if cycle % 2 == 0 { 1 } else { -1 };
            assert!(surface.scroll_visual_rows(delta, 8));
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn wheel_scroll_moves_by_visual_rows_and_rejoins_the_tail() {
        let mut surface = LogSurfaceModel::new(1);
        surface.submit_page(page(1, 100..120, 120)).unwrap();
        let layout = surface.request_reflow(80).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));

        assert!(surface.scroll_visual_rows(-3, 5));
        assert_eq!(
            surface.mode(),
            ScrollMode::Anchored {
                line_id: 112,
                cell_offset: 0,
            }
        );
        assert!(surface.scroll_visual_rows(3, 5));
        assert_eq!(surface.mode(), ScrollMode::FollowTail);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn wrapped_scroll_anchor_uses_the_visual_segments_cell_offset() {
        let mut long_page = (*page(1, 1..2, 1)).clone();
        long_page.rows = Arc::from([LogRow {
            line_id: 1,
            text: Arc::from("abcdefghijkl"),
            style_spans: Arc::from([]),
            truncated: false,
        }]);
        let mut surface = LogSurfaceModel::new(0);
        surface.submit_page(Arc::new(long_page)).unwrap();
        let layout = surface.request_reflow(4).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));

        assert!(surface.scroll_visual_rows(-1, 1));
        assert_eq!(
            surface.mode(),
            ScrollMode::Anchored {
                line_id: 1,
                cell_offset: 4,
            }
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn scroll_beyond_loaded_history_is_applied_after_the_new_page_reflows() {
        let mut first = (*page(1, 50..60, 60)).clone();
        first.anchor_line_id = 50;
        first.has_before = true;
        let mut surface = LogSurfaceModel::new(0);
        surface.submit_page(Arc::new(first)).unwrap();
        let layout = surface.request_reflow(80).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));
        surface.anchor(50, 0);
        assert!(surface.scroll_visual_rows(-3, 5));

        let mut expanded = (*page(2, 47..60, 60)).clone();
        expanded.anchor_line_id = 50;
        expanded.has_before = true;
        surface.submit_page(Arc::new(expanded)).unwrap();
        let layout = surface.request_reflow(80).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));
        assert_eq!(
            surface.mode(),
            ScrollMode::Anchored {
                line_id: 47,
                cell_offset: 0,
            }
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn scrollbar_jump_keeps_old_frame_until_target_page_and_layout_are_ready() {
        let mut surface = LogSurfaceModel::new(0);
        surface.submit_page(page(1, 1..11, 1_000)).unwrap();
        let layout = surface.request_reflow(80).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));
        let old_page = surface.prepare_frame(5).unwrap().page;

        assert!(surface.jump_to_line(900, 0));
        let request = surface.page_request(5).unwrap();
        assert!(!request.follow_tail);
        assert_eq!(request.anchor_line_id, 900);
        assert!(Arc::ptr_eq(
            &surface.prepare_frame(5).unwrap().page,
            &old_page
        ));

        let mut target = (*page(2, 895..906, 1_000)).clone();
        target.anchor_line_id = 900;
        target.has_after = true;
        surface.submit_page(Arc::new(target)).unwrap();
        assert!(Arc::ptr_eq(
            &surface.prepare_frame(5).unwrap().page,
            &old_page
        ));
        let layout = surface.request_reflow(80).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));
        assert_eq!(
            surface.mode(),
            ScrollMode::Anchored {
                line_id: 900,
                cell_offset: 0,
            }
        );
        let frame = surface.prepare_frame(5).unwrap();
        assert_eq!(frame.page.revision, 2);
        assert_eq!(surface.scrollbar_state(5).unwrap().top_line_id, 900);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn reflow_maps_logical_rows_to_bounded_visual_cell_ranges() {
        let rows: Arc<[LogRow]> = Arc::from([
            LogRow {
                line_id: 1,
                text: Arc::from("abcd中ef"),
                style_spans: Arc::from([]),
                truncated: false,
            },
            LogRow {
                line_id: 2,
                text: Arc::from("\tX"),
                style_spans: Arc::from([]),
                truncated: false,
            },
        ]);
        let page = Arc::new(LogPage {
            source_id: LogSourceId(7),
            revision: 1,
            anchor_line_id: 1,
            rows,
            total_line_count: 2,
            has_before: false,
            has_after: false,
        });
        let mut surface = LogSurfaceModel::default();
        surface.submit_page(page).unwrap();
        let layout = surface.request_reflow(4).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(layout));
        let frame = surface.prepare_frame(10).unwrap();
        assert_eq!(
            frame.visual_rows.as_ref(),
            [
                super::LogVisualRow {
                    logical_row_index: 0,
                    start_byte: 0,
                    end_byte: 4,
                    start_cell: 0,
                    end_cell: 4,
                },
                super::LogVisualRow {
                    logical_row_index: 0,
                    start_byte: 4,
                    end_byte: 9,
                    start_cell: 4,
                    end_cell: 8,
                },
                super::LogVisualRow {
                    logical_row_index: 1,
                    start_byte: 0,
                    end_byte: 1,
                    start_cell: 0,
                    end_cell: 8,
                },
                super::LogVisualRow {
                    logical_row_index: 1,
                    start_byte: 1,
                    end_byte: 2,
                    start_cell: 8,
                    end_cell: 9,
                },
            ]
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn resize_keeps_old_layout_until_latest_width_can_switch_atomically() {
        let mut surface = LogSurfaceModel::new(0);
        surface.submit_page(page(1, 100..103, 103)).unwrap();
        surface.anchor(101, 6);

        let first = surface.request_reflow(8).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(first));
        let old_rows = surface.prepare_frame(4).unwrap().visual_rows;

        let stale = surface.request_reflow(4).unwrap().execute().unwrap();
        assert!(surface.request_reflow(12).is_none());
        assert!(Arc::ptr_eq(
            &surface.prepare_frame(4).unwrap().visual_rows,
            &old_rows
        ));
        assert!(!surface.submit_reflow(stale));
        assert!(Arc::ptr_eq(
            &surface.prepare_frame(4).unwrap().visual_rows,
            &old_rows
        ));

        let latest = surface.request_reflow(12).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(latest));
        let frame = surface.prepare_frame(4).unwrap();
        assert!(!Arc::ptr_eq(&frame.visual_rows, &old_rows));
        let first_visual = frame.visual_rows[frame.visible_rows.start + frame.first_viewport_row];
        assert_eq!(
            frame.page.rows[first_visual.logical_row_index as usize].line_id,
            101
        );
        assert!(first_visual.start_cell <= 6 && 6 < first_visual.end_cell);
        assert_eq!(frame.cell_offset, 6);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn delayed_reflow_for_an_older_page_cannot_replace_newer_content() {
        let mut surface = LogSurfaceModel::default();
        surface.submit_page(page(1, 1..4, 4)).unwrap();
        let old_layout = surface.request_reflow(80).unwrap().execute().unwrap();
        surface.submit_page(page(2, 1..5, 5)).unwrap();
        assert!(!surface.submit_reflow(old_layout));
        let latest = surface.request_reflow(80).unwrap().execute().unwrap();
        assert!(surface.submit_reflow(latest));
        assert_eq!(surface.prepare_frame(10).unwrap().page.revision, 2);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn reflow_metadata_has_an_explicit_memory_bound() {
        let text: Arc<str> = "x".repeat(super::MAX_LOG_REFLOW_ROWS + 1).into();
        let page = Arc::new(LogPage {
            source_id: LogSourceId(7),
            revision: 1,
            anchor_line_id: 1,
            rows: Arc::from([LogRow {
                line_id: 1,
                text,
                style_spans: Arc::from([]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        });
        let mut surface = LogSurfaceModel::default();
        surface.submit_page(page).unwrap();
        assert!(matches!(
            surface.request_reflow(1).unwrap().execute(),
            Err(LogSurfaceError::TooManyReflowRows(count))
                if count == super::MAX_LOG_REFLOW_ROWS + 1
        ));
        surface.reflow_failed();
        assert!(surface.request_reflow(1).is_none());
        assert!(surface.request_reflow(2).is_some());
    }

    #[test]
    fn tail_page_request_loads_history_before_the_last_line() {
        let mut surface = LogSurfaceModel::new(3);
        surface
            .submit_page(page(1, 40..60, 60))
            .unwrap_or_else(|error| panic!("{error}"));
        let request = surface
            .page_request(20)
            .unwrap_or_else(|| panic!("loaded source should produce a request"));
        assert!(request.follow_tail);
        assert_eq!(request.anchor_line_id, 59);
        assert_eq!(request.rows_before, 23);
        assert_eq!(request.rows_after, 0);
    }

    #[test]
    fn malformed_and_unbounded_pages_are_rejected() {
        let mut unordered = (*page(1, 1..4, 4)).clone();
        unordered.rows = Arc::from([
            LogRow {
                line_id: 2,
                text: Arc::from("two"),
                style_spans: Arc::from([]),
                truncated: false,
            },
            LogRow {
                line_id: 1,
                text: Arc::from("one"),
                style_spans: Arc::from([]),
                truncated: false,
            },
        ]);
        assert_eq!(
            LogSurfaceModel::default().submit_page(Arc::new(unordered)),
            Err(LogSurfaceError::UnorderedRows)
        );

        let oversized_rows: Arc<[LogRow]> = (0..=MAX_LOG_PAGE_ROWS)
            .map(|line_id| LogRow {
                line_id: line_id as u64,
                text: Arc::from("x"),
                style_spans: Arc::from([]),
                truncated: false,
            })
            .collect::<Vec<_>>()
            .into();
        let oversized = Arc::new(LogPage {
            source_id: LogSourceId(1),
            revision: 1,
            anchor_line_id: 0,
            total_line_count: oversized_rows.len() as u64,
            rows: oversized_rows,
            has_before: false,
            has_after: false,
        });
        assert_eq!(
            LogSurfaceModel::default().submit_page(oversized),
            Err(LogSurfaceError::TooManyRows(MAX_LOG_PAGE_ROWS + 1))
        );

        let huge_text: Arc<str> = "x".repeat(MAX_LOG_PAGE_TEXT_BYTES + 1).into();
        let text_bytes = huge_text.len();
        let oversized_text = Arc::new(LogPage {
            source_id: LogSourceId(1),
            revision: 1,
            anchor_line_id: 1,
            rows: Arc::from([LogRow {
                line_id: 1,
                text: huge_text,
                style_spans: Arc::from([]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        });
        assert_eq!(
            LogSurfaceModel::default().submit_page(oversized_text),
            Err(LogSurfaceError::TooManyTextBytes(text_bytes))
        );
    }

    #[test]
    fn style_spans_must_be_ordered_and_end_on_grapheme_boundaries() {
        let styled = Arc::new(LogPage {
            source_id: LogSourceId(1),
            revision: 1,
            anchor_line_id: 1,
            rows: Arc::from([LogRow {
                line_id: 1,
                text: Arc::from("A中e\u{301}"),
                style_spans: Arc::from([
                    LogStyleSpan {
                        byte_range: 0..1,
                        style: Style {
                            foreground: Color::Indexed(1),
                            ..Style::default()
                        },
                    },
                    LogStyleSpan {
                        byte_range: 1..4,
                        style: Style {
                            foreground: Color::Rgb(1, 2, 3),
                            ..Style::default()
                        },
                    },
                ]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        });
        assert!(
            LogSurfaceModel::default()
                .submit_page(styled)
                .unwrap_or_else(|error| panic!("{error}"))
        );

        let invalid = Arc::new(LogPage {
            source_id: LogSourceId(1),
            revision: 2,
            anchor_line_id: 1,
            rows: Arc::from([LogRow {
                line_id: 1,
                text: Arc::from("A中e\u{301}"),
                style_spans: Arc::from([LogStyleSpan {
                    byte_range: 2..4,
                    style: Style::default(),
                }]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        });
        assert_eq!(
            LogSurfaceModel::default().submit_page(invalid),
            Err(LogSurfaceError::InvalidStyleSpan { line_id: 1 })
        );
    }

    #[test]
    fn embedded_line_breaks_are_rejected_from_log_rows() {
        let invalid = Arc::new(LogPage {
            source_id: LogSourceId(1),
            revision: 1,
            anchor_line_id: 1,
            rows: Arc::from([LogRow {
                line_id: 1,
                text: Arc::from("first\nsecond"),
                style_spans: Arc::from([]),
                truncated: false,
            }]),
            total_line_count: 1,
            has_before: false,
            has_after: false,
        });
        assert_eq!(
            LogSurfaceModel::default().submit_page(invalid),
            Err(LogSurfaceError::InvalidRowText { line_id: 1 })
        );
    }
}
