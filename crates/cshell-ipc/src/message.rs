use cshell_domain::{
    ControlAction, InputAction, KeyCode, KeyEvent, Modifiers, SessionId, TerminalSize,
};
use cshell_terminal::{
    Cell, CellWidth, Color, CursorAppearance, CursorShape, FrameSnapshot, MAX_HYPERLINK_URI_BYTES,
    MAX_ZERO_WIDTH_CHARS_PER_CELL, Style, TerminalModes, is_zero_width_character,
};
use prost::{Enumeration, Message};
use std::collections::HashSet;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 11;
pub const TERMINAL_FRAME_SCHEMA_VERSION: u32 = 2;
pub const MAX_TERMINAL_HYPERLINK_URI_BYTES: usize = MAX_HYPERLINK_URI_BYTES;
pub const MAX_LOG_PAGE_ROWS: usize = 4096;
pub const MAX_LOG_PAGE_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_LOG_PAGE_STYLE_SPANS: usize = 16 * 1024;
pub const MAX_TERMINAL_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_HISTORY_SEARCH_QUERY_BYTES: usize = 4 * 1024;
pub const MAX_HISTORY_SEARCH_SCAN_LINES: usize = 4096;
pub const MAX_HISTORY_SEARCH_MATCHES: usize = 4096;

pub mod features {
    pub const FULL_FRAME_RECOVERY: u64 = 1 << 0;
    pub const CANCELLATION: u64 = 1 << 1;
    pub const PRIORITY_STREAMS: u64 = 1 << 2;
    pub const LOG_PAGING: u64 = 1 << 3;
    pub const TERMINAL_CONTROL: u64 = 1 << 4;
    pub const HISTORY_SEARCH: u64 = 1 << 5;
    pub const PROFILE_CONTROL: u64 = 1 << 6;
    pub const SSH_PROFILE_TARGET: u64 = 1 << 7;
    pub const SSH_PROFILE_SESSION: u64 = 1 << 8;
    pub const SSH_PROFILE_AUTH: u64 = 1 << 9;
    pub const SSH_HOST_KEY_IMPORT: u64 = 1 << 10;
}

#[derive(Clone, PartialEq, Message)]
pub struct Handshake {
    #[prost(fixed32, tag = "1")]
    pub magic: u32,
    #[prost(uint32, tag = "2")]
    pub protocol_major: u32,
    #[prost(uint32, tag = "3")]
    pub protocol_minor: u32,
    #[prost(fixed64, tag = "4")]
    pub feature_bits: u64,
    #[prost(bytes = "vec", tag = "5")]
    pub daemon_instance_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "6")]
    pub instance_token: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HandshakeAck {
    #[prost(uint32, tag = "1")]
    pub protocol_major: u32,
    #[prost(uint32, tag = "2")]
    pub protocol_minor: u32,
    #[prost(fixed64, tag = "3")]
    pub negotiated_feature_bits: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub daemon_instance_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FullFrame {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(fixed64, tag = "2")]
    pub generation: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub payload: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalFramePayload {
    #[prost(uint32, tag = "1")]
    pub schema_version: u32,
    #[prost(uint32, tag = "2")]
    pub rows: u32,
    #[prost(uint32, tag = "3")]
    pub cols: u32,
    #[prost(uint32, tag = "4")]
    pub cursor_row: u32,
    #[prost(uint32, tag = "5")]
    pub cursor_col: u32,
    #[prost(bool, tag = "6")]
    pub application_cursor: bool,
    #[prost(bool, tag = "7")]
    pub bracketed_paste: bool,
    #[prost(message, repeated, tag = "8")]
    pub cells: Vec<TerminalCell>,
    #[prost(message, optional, tag = "9")]
    pub cursor_appearance: Option<TerminalCursorAppearance>,
    #[prost(uint32, tag = "10")]
    pub kitty_keyboard_flags: u32,
    #[prost(uint32, tag = "11")]
    pub modify_other_keys: u32,
    #[prost(bool, tag = "12")]
    pub format_other_keys: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalCell {
    #[prost(uint32, tag = "1")]
    pub scalar: u32,
    #[prost(message, optional, tag = "2")]
    pub style: Option<TerminalStyle>,
    #[prost(uint32, repeated, tag = "3")]
    pub zerowidth_scalars: Vec<u32>,
    #[prost(enumeration = "TerminalCellWidth", tag = "4")]
    pub width: i32,
    #[prost(string, tag = "5")]
    pub hyperlink_uri: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalCellWidth {
    Single = 0,
    Wide = 1,
    WideSpacer = 2,
    LeadingWideSpacer = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalCursorAppearance {
    #[prost(enumeration = "TerminalCursorShape", tag = "1")]
    pub shape: i32,
    #[prost(bool, tag = "2")]
    pub blinking: bool,
    #[prost(message, optional, tag = "3")]
    pub color: Option<TerminalColor>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalCursorShape {
    Block = 0,
    Underline = 1,
    Beam = 2,
    HollowBlock = 3,
    Hidden = 4,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalStyle {
    #[prost(message, optional, tag = "1")]
    pub foreground: Option<TerminalColor>,
    #[prost(message, optional, tag = "2")]
    pub background: Option<TerminalColor>,
    #[prost(bool, tag = "3")]
    pub bold: bool,
    #[prost(bool, tag = "4")]
    pub italic: bool,
    #[prost(bool, tag = "5")]
    pub underline: bool,
    #[prost(bool, tag = "6")]
    pub inverse: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalColor {
    #[prost(enumeration = "ColorKind", tag = "1")]
    pub kind: i32,
    /// Indexed color number or packed `0xRRGGBB`, depending on `kind`.
    #[prost(uint32, tag = "2")]
    pub value: u32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum ColorKind {
    Default = 0,
    Indexed = 1,
    Rgb = 2,
}

#[derive(Debug, Error)]
pub enum SnapshotCodecError {
    #[error("cannot decode terminal frame payload: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("unsupported terminal frame schema version {0}")]
    UnsupportedSchema(u32),
    #[error("terminal frame dimensions {rows}x{cols} are invalid")]
    InvalidDimensions { rows: u32, cols: u32 },
    #[error("terminal frame has {actual} cells; expected {expected}")]
    InvalidCellCount { expected: usize, actual: usize },
    #[error("terminal cursor ({row},{col}) is outside {rows}x{cols}")]
    InvalidCursor {
        row: u32,
        col: u32,
        rows: u32,
        cols: u32,
    },
    #[error("terminal cell contains invalid Unicode scalar U+{0:04X}")]
    InvalidScalar(u32),
    #[error("terminal cell contains {actual} zero-width scalars; maximum is {maximum}")]
    TooManyZeroWidthScalars { actual: usize, maximum: usize },
    #[error("terminal cell width kind {0} is unknown")]
    UnknownCellWidth(i32),
    #[error("terminal cell combining scalar U+{0:04X} has non-zero display width")]
    NonZeroWidthCombiningScalar(u32),
    #[error("terminal cell hyperlink URI contains {actual} bytes; maximum is {maximum}")]
    HyperlinkUriTooLong { actual: usize, maximum: usize },
    #[error("terminal cursor shape {0} is unknown")]
    UnknownCursorShape(i32),
    #[error("terminal color kind {0} is unknown")]
    UnknownColorKind(i32),
    #[error("indexed terminal color {0} exceeds 255")]
    InvalidIndexedColor(u32),
    #[error("RGB terminal color 0x{0:X} exceeds 24 bits")]
    InvalidRgbColor(u32),
}

impl FullFrame {
    #[must_use]
    pub fn from_terminal_snapshot(session_id: SessionId, snapshot: &FrameSnapshot) -> Self {
        let payload = TerminalFramePayload {
            schema_version: TERMINAL_FRAME_SCHEMA_VERSION,
            rows: u32::from(snapshot.rows),
            cols: u32::from(snapshot.cols),
            cursor_row: u32::from(snapshot.cursor_row),
            cursor_col: u32::from(snapshot.cursor_col),
            application_cursor: snapshot.terminal_modes.application_cursor,
            bracketed_paste: snapshot.terminal_modes.bracketed_paste,
            cells: snapshot.cells.iter().map(TerminalCell::from).collect(),
            cursor_appearance: Some(TerminalCursorAppearance::from(snapshot.cursor_appearance)),
            kitty_keyboard_flags: u32::from(snapshot.terminal_modes.kitty_keyboard_flags),
            modify_other_keys: u32::from(snapshot.terminal_modes.modify_other_keys),
            format_other_keys: snapshot.terminal_modes.format_other_keys,
        };
        Self {
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            generation: snapshot.generation,
            payload: payload.encode_to_vec(),
        }
    }

    pub fn decode_terminal_snapshot(&self) -> Result<FrameSnapshot, SnapshotCodecError> {
        let payload = TerminalFramePayload::decode(self.payload.as_slice())?;
        if payload.schema_version != TERMINAL_FRAME_SCHEMA_VERSION {
            return Err(SnapshotCodecError::UnsupportedSchema(
                payload.schema_version,
            ));
        }
        if payload.rows == 0
            || payload.cols == 0
            || payload.rows > u32::from(u16::MAX)
            || payload.cols > u32::from(u16::MAX)
        {
            return Err(SnapshotCodecError::InvalidDimensions {
                rows: payload.rows,
                cols: payload.cols,
            });
        }
        let expected = usize::try_from(payload.rows)
            .ok()
            .and_then(|rows| {
                usize::try_from(payload.cols)
                    .ok()
                    .and_then(|cols| rows.checked_mul(cols))
            })
            .ok_or(SnapshotCodecError::InvalidDimensions {
                rows: payload.rows,
                cols: payload.cols,
            })?;
        if payload.cells.len() != expected {
            return Err(SnapshotCodecError::InvalidCellCount {
                expected,
                actual: payload.cells.len(),
            });
        }
        if payload.cursor_row >= payload.rows || payload.cursor_col >= payload.cols {
            return Err(SnapshotCodecError::InvalidCursor {
                row: payload.cursor_row,
                col: payload.cursor_col,
                rows: payload.rows,
                cols: payload.cols,
            });
        }

        Ok(FrameSnapshot {
            generation: self.generation,
            rows: payload.rows as u16,
            cols: payload.cols as u16,
            cursor_row: payload.cursor_row as u16,
            cursor_col: payload.cursor_col as u16,
            cursor_appearance: payload.cursor_appearance.map_or_else(
                || Ok(CursorAppearance::default()),
                CursorAppearance::try_from,
            )?,
            terminal_modes: TerminalModes {
                application_cursor: payload.application_cursor,
                bracketed_paste: payload.bracketed_paste,
                kitty_keyboard_flags: (payload.kitty_keyboard_flags
                    & u32::from(
                        TerminalModes::KITTY_DISAMBIGUATE
                            | TerminalModes::KITTY_REPORT_EVENTS
                            | TerminalModes::KITTY_REPORT_ALTERNATE_KEYS
                            | TerminalModes::KITTY_REPORT_ALL_KEYS
                            | TerminalModes::KITTY_REPORT_ASSOCIATED_TEXT,
                    )) as u8,
                modify_other_keys: payload.modify_other_keys.min(3) as u8,
                format_other_keys: payload.format_other_keys,
            },
            cells: payload
                .cells
                .into_iter()
                .map(Cell::try_from)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl From<CursorAppearance> for TerminalCursorAppearance {
    fn from(appearance: CursorAppearance) -> Self {
        Self {
            shape: match appearance.shape {
                CursorShape::Block => TerminalCursorShape::Block,
                CursorShape::Underline => TerminalCursorShape::Underline,
                CursorShape::Beam => TerminalCursorShape::Beam,
                CursorShape::HollowBlock => TerminalCursorShape::HollowBlock,
                CursorShape::Hidden => TerminalCursorShape::Hidden,
            } as i32,
            blinking: appearance.blinking,
            color: appearance.color.map(TerminalColor::from),
        }
    }
}

impl TryFrom<TerminalCursorAppearance> for CursorAppearance {
    type Error = SnapshotCodecError;

    fn try_from(appearance: TerminalCursorAppearance) -> Result<Self, Self::Error> {
        let shape = match TerminalCursorShape::try_from(appearance.shape)
            .map_err(|_| SnapshotCodecError::UnknownCursorShape(appearance.shape))?
        {
            TerminalCursorShape::Block => CursorShape::Block,
            TerminalCursorShape::Underline => CursorShape::Underline,
            TerminalCursorShape::Beam => CursorShape::Beam,
            TerminalCursorShape::HollowBlock => CursorShape::HollowBlock,
            TerminalCursorShape::Hidden => CursorShape::Hidden,
        };
        Ok(Self {
            shape,
            blinking: appearance.blinking,
            color: appearance.color.map(Color::try_from).transpose()?,
        })
    }
}

impl From<&Cell> for TerminalCell {
    fn from(cell: &Cell) -> Self {
        Self {
            scalar: u32::from(cell.character),
            zerowidth_scalars: cell.zerowidth().iter().copied().map(u32::from).collect(),
            width: match cell.width {
                CellWidth::Single => TerminalCellWidth::Single,
                CellWidth::Wide => TerminalCellWidth::Wide,
                CellWidth::WideSpacer => TerminalCellWidth::WideSpacer,
                CellWidth::LeadingWideSpacer => TerminalCellWidth::LeadingWideSpacer,
            } as i32,
            style: Some(TerminalStyle::from(cell.style)),
            hyperlink_uri: cell.hyperlink_uri().unwrap_or_default().to_owned(),
        }
    }
}

impl TryFrom<TerminalCell> for Cell {
    type Error = SnapshotCodecError;

    fn try_from(cell: TerminalCell) -> Result<Self, Self::Error> {
        if cell.zerowidth_scalars.len() > MAX_ZERO_WIDTH_CHARS_PER_CELL {
            return Err(SnapshotCodecError::TooManyZeroWidthScalars {
                actual: cell.zerowidth_scalars.len(),
                maximum: MAX_ZERO_WIDTH_CHARS_PER_CELL,
            });
        }
        let character =
            char::from_u32(cell.scalar).ok_or(SnapshotCodecError::InvalidScalar(cell.scalar))?;
        let zerowidth = cell
            .zerowidth_scalars
            .into_iter()
            .map(|scalar| {
                let character =
                    char::from_u32(scalar).ok_or(SnapshotCodecError::InvalidScalar(scalar))?;
                if !is_zero_width_character(character) {
                    return Err(SnapshotCodecError::NonZeroWidthCombiningScalar(scalar));
                }
                Ok(character)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let width = match TerminalCellWidth::try_from(cell.width)
            .map_err(|_| SnapshotCodecError::UnknownCellWidth(cell.width))?
        {
            TerminalCellWidth::Single => CellWidth::Single,
            TerminalCellWidth::Wide => CellWidth::Wide,
            TerminalCellWidth::WideSpacer => CellWidth::WideSpacer,
            TerminalCellWidth::LeadingWideSpacer => CellWidth::LeadingWideSpacer,
        };
        let style = cell
            .style
            .map_or_else(|| Ok(Style::default()), Style::try_from)?;
        if cell.hyperlink_uri.len() > MAX_TERMINAL_HYPERLINK_URI_BYTES {
            return Err(SnapshotCodecError::HyperlinkUriTooLong {
                actual: cell.hyperlink_uri.len(),
                maximum: MAX_TERMINAL_HYPERLINK_URI_BYTES,
            });
        }
        let mut decoded = Self::with_zerowidth(character, zerowidth, width, style);
        if !cell.hyperlink_uri.is_empty() {
            decoded = decoded.with_hyperlink_uri(cell.hyperlink_uri);
        }
        Ok(decoded)
    }
}

impl From<Style> for TerminalStyle {
    fn from(style: Style) -> Self {
        Self {
            foreground: Some(TerminalColor::from(style.foreground)),
            background: Some(TerminalColor::from(style.background)),
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            inverse: style.inverse,
        }
    }
}

impl TryFrom<TerminalStyle> for Style {
    type Error = SnapshotCodecError;

    fn try_from(style: TerminalStyle) -> Result<Self, Self::Error> {
        Ok(Self {
            foreground: style
                .foreground
                .map_or_else(|| Ok(Color::Default), Color::try_from)?,
            background: style
                .background
                .map_or_else(|| Ok(Color::Default), Color::try_from)?,
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            inverse: style.inverse,
        })
    }
}

impl From<Color> for TerminalColor {
    fn from(color: Color) -> Self {
        match color {
            Color::Default => Self {
                kind: ColorKind::Default as i32,
                value: 0,
            },
            Color::Indexed(index) => Self {
                kind: ColorKind::Indexed as i32,
                value: u32::from(index),
            },
            Color::Rgb(red, green, blue) => Self {
                kind: ColorKind::Rgb as i32,
                value: u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue),
            },
        }
    }
}

impl TryFrom<TerminalColor> for Color {
    type Error = SnapshotCodecError;

    fn try_from(color: TerminalColor) -> Result<Self, Self::Error> {
        match ColorKind::try_from(color.kind)
            .map_err(|_| SnapshotCodecError::UnknownColorKind(color.kind))?
        {
            ColorKind::Default => Ok(Self::Default),
            ColorKind::Indexed => u8::try_from(color.value)
                .map(Self::Indexed)
                .map_err(|_| SnapshotCodecError::InvalidIndexedColor(color.value)),
            ColorKind::Rgb if color.value <= 0xFF_FFFF => Ok(Self::Rgb(
                ((color.value >> 16) & 0xFF) as u8,
                ((color.value >> 8) & 0xFF) as u8,
                (color.value & 0xFF) as u8,
            )),
            ColorKind::Rgb => Err(SnapshotCodecError::InvalidRgbColor(color.value)),
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct FrameDelta {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(fixed64, tag = "2")]
    pub base_generation: u64,
    #[prost(fixed64, tag = "3")]
    pub generation: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub payload: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalDeltaPayload {
    #[prost(uint32, tag = "1")]
    pub schema_version: u32,
    #[prost(uint32, tag = "2")]
    pub rows: u32,
    #[prost(uint32, tag = "3")]
    pub cols: u32,
    #[prost(uint32, tag = "4")]
    pub cursor_row: u32,
    #[prost(uint32, tag = "5")]
    pub cursor_col: u32,
    #[prost(bool, tag = "6")]
    pub application_cursor: bool,
    #[prost(bool, tag = "7")]
    pub bracketed_paste: bool,
    #[prost(message, repeated, tag = "8")]
    pub rows_changed: Vec<TerminalRowPatch>,
    #[prost(message, optional, tag = "9")]
    pub cursor_appearance: Option<TerminalCursorAppearance>,
    #[prost(uint32, tag = "10")]
    pub kitty_keyboard_flags: u32,
    #[prost(uint32, tag = "11")]
    pub modify_other_keys: u32,
    #[prost(bool, tag = "12")]
    pub format_other_keys: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalRowPatch {
    #[prost(uint32, tag = "1")]
    pub row: u32,
    #[prost(message, repeated, tag = "2")]
    pub cells: Vec<TerminalCell>,
}

#[derive(Debug, Error)]
pub enum DeltaCodecError {
    #[error("terminal delta generation {generation} must exceed base {base_generation}")]
    InvalidGeneration {
        base_generation: u64,
        generation: u64,
    },
    #[error("terminal delta base generation {actual} does not match snapshot {expected}")]
    BaseGenerationMismatch { expected: u64, actual: u64 },
    #[error("terminal dimensions changed from {base_rows}x{base_cols} to {rows}x{cols}")]
    DimensionsChanged {
        base_rows: u16,
        base_cols: u16,
        rows: u16,
        cols: u16,
    },
    #[error("cannot decode terminal delta payload: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("unsupported terminal delta schema version {0}")]
    UnsupportedSchema(u32),
    #[error("terminal delta dimensions {rows}x{cols} do not match its base snapshot")]
    InvalidDimensions { rows: u32, cols: u32 },
    #[error("terminal delta cursor ({row},{col}) is outside {rows}x{cols}")]
    InvalidCursor {
        row: u32,
        col: u32,
        rows: u32,
        cols: u32,
    },
    #[error("terminal delta row {0} is outside the screen")]
    InvalidRow(u32),
    #[error("terminal delta row has {actual} cells; expected {expected}")]
    InvalidRowWidth { expected: usize, actual: usize },
    #[error(transparent)]
    Cell(#[from] SnapshotCodecError),
}

impl FrameDelta {
    pub fn between_terminal_snapshots(
        session_id: SessionId,
        base: &FrameSnapshot,
        current: &FrameSnapshot,
    ) -> Result<Self, DeltaCodecError> {
        if current.generation <= base.generation {
            return Err(DeltaCodecError::InvalidGeneration {
                base_generation: base.generation,
                generation: current.generation,
            });
        }
        if (base.rows, base.cols) != (current.rows, current.cols) {
            return Err(DeltaCodecError::DimensionsChanged {
                base_rows: base.rows,
                base_cols: base.cols,
                rows: current.rows,
                cols: current.cols,
            });
        }
        let cols = usize::from(current.cols);
        let rows_changed = base
            .cells
            .chunks_exact(cols)
            .zip(current.cells.chunks_exact(cols))
            .enumerate()
            .filter(|(_row, (base_row, current_row))| base_row != current_row)
            .map(|(row, (_base_row, current_row))| TerminalRowPatch {
                row: row as u32,
                cells: current_row.iter().map(TerminalCell::from).collect(),
            })
            .collect();
        let payload = TerminalDeltaPayload {
            schema_version: TERMINAL_FRAME_SCHEMA_VERSION,
            rows: u32::from(current.rows),
            cols: u32::from(current.cols),
            cursor_row: u32::from(current.cursor_row),
            cursor_col: u32::from(current.cursor_col),
            application_cursor: current.terminal_modes.application_cursor,
            bracketed_paste: current.terminal_modes.bracketed_paste,
            rows_changed,
            cursor_appearance: Some(TerminalCursorAppearance::from(current.cursor_appearance)),
            kitty_keyboard_flags: u32::from(current.terminal_modes.kitty_keyboard_flags),
            modify_other_keys: u32::from(current.terminal_modes.modify_other_keys),
            format_other_keys: current.terminal_modes.format_other_keys,
        };
        Ok(Self {
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            base_generation: base.generation,
            generation: current.generation,
            payload: payload.encode_to_vec(),
        })
    }

    pub fn apply_terminal_delta(
        &self,
        base: &FrameSnapshot,
    ) -> Result<FrameSnapshot, DeltaCodecError> {
        if self.generation <= self.base_generation {
            return Err(DeltaCodecError::InvalidGeneration {
                base_generation: self.base_generation,
                generation: self.generation,
            });
        }
        if self.base_generation != base.generation {
            return Err(DeltaCodecError::BaseGenerationMismatch {
                expected: base.generation,
                actual: self.base_generation,
            });
        }
        let payload = TerminalDeltaPayload::decode(self.payload.as_slice())?;
        if payload.schema_version != TERMINAL_FRAME_SCHEMA_VERSION {
            return Err(DeltaCodecError::UnsupportedSchema(payload.schema_version));
        }
        if payload.rows != u32::from(base.rows) || payload.cols != u32::from(base.cols) {
            return Err(DeltaCodecError::InvalidDimensions {
                rows: payload.rows,
                cols: payload.cols,
            });
        }
        if payload.cursor_row >= payload.rows || payload.cursor_col >= payload.cols {
            return Err(DeltaCodecError::InvalidCursor {
                row: payload.cursor_row,
                col: payload.cursor_col,
                rows: payload.rows,
                cols: payload.cols,
            });
        }
        let mut snapshot = base.clone();
        let cols = usize::from(base.cols);
        for row in payload.rows_changed {
            let row_index = usize::try_from(row.row)
                .ok()
                .filter(|row| *row < usize::from(base.rows))
                .ok_or(DeltaCodecError::InvalidRow(row.row))?;
            if row.cells.len() != cols {
                return Err(DeltaCodecError::InvalidRowWidth {
                    expected: cols,
                    actual: row.cells.len(),
                });
            }
            let start = row_index * cols;
            let decoded = row
                .cells
                .into_iter()
                .map(Cell::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            snapshot.cells[start..start + cols].clone_from_slice(&decoded);
        }
        snapshot.generation = self.generation;
        snapshot.cursor_row = payload.cursor_row as u16;
        snapshot.cursor_col = payload.cursor_col as u16;
        snapshot.cursor_appearance = payload.cursor_appearance.map_or_else(
            || Ok(CursorAppearance::default()),
            CursorAppearance::try_from,
        )?;
        snapshot.terminal_modes = TerminalModes {
            application_cursor: payload.application_cursor,
            bracketed_paste: payload.bracketed_paste,
            kitty_keyboard_flags: (payload.kitty_keyboard_flags
                & u32::from(
                    TerminalModes::KITTY_DISAMBIGUATE
                        | TerminalModes::KITTY_REPORT_EVENTS
                        | TerminalModes::KITTY_REPORT_ALTERNATE_KEYS
                        | TerminalModes::KITTY_REPORT_ALL_KEYS
                        | TerminalModes::KITTY_REPORT_ASSOCIATED_TEXT,
                )) as u8,
            modify_other_keys: payload.modify_other_keys.min(3) as u8,
            format_other_keys: payload.format_other_keys,
        };
        Ok(snapshot)
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct SnapshotRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(fixed64, optional, tag = "2")]
    pub current_generation: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionListRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct SessionListResponse {
    #[prost(message, repeated, tag = "1")]
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionSummary {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub title: String,
    #[prost(bool, tag = "3")]
    pub running: bool,
    #[prost(fixed64, tag = "4")]
    pub generation: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionCreateRequest {
    #[prost(uint32, tag = "1")]
    pub rows: u32,
    #[prost(uint32, tag = "2")]
    pub cols: u32,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub profile_id: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionCreateResponse {
    #[prost(message, optional, tag = "1")]
    pub session: Option<SessionSummary>,
    #[prost(string, tag = "2")]
    pub detail: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionCloseRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionCloseResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalInputRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    pub key: Option<TerminalKeyEvent>,
    #[prost(string, optional, tag = "3")]
    pub text: Option<String>,
    #[prost(message, optional, tag = "4")]
    pub paste: Option<TerminalPaste>,
    #[prost(enumeration = "TerminalControlAction", optional, tag = "5")]
    pub control: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalKeyEvent {
    #[prost(enumeration = "TerminalKeyKind", tag = "1")]
    pub kind: i32,
    #[prost(string, tag = "2")]
    pub character: String,
    #[prost(uint32, tag = "3")]
    pub function: u32,
    #[prost(bool, tag = "4")]
    pub ctrl: bool,
    #[prost(bool, tag = "5")]
    pub alt: bool,
    #[prost(bool, tag = "6")]
    pub shift: bool,
    #[prost(bool, tag = "7")]
    pub super_key: bool,
    #[prost(bool, tag = "8")]
    pub pressed: bool,
    #[prost(bool, tag = "9")]
    pub repeated: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalPaste {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(bool, tag = "2")]
    pub bracketed: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalKeyKind {
    Character = 0,
    Enter = 1,
    Tab = 2,
    Backspace = 3,
    Escape = 4,
    ArrowUp = 5,
    ArrowDown = 6,
    ArrowLeft = 7,
    ArrowRight = 8,
    Function = 9,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalControlAction {
    Interrupt = 0,
    EndOfFile = 1,
    Suspend = 2,
    ClearScreen = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalResizeRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    pub rows: u32,
    #[prost(uint32, tag = "3")]
    pub cols: u32,
    #[prost(uint32, tag = "4")]
    pub pixel_width: u32,
    #[prost(uint32, tag = "5")]
    pub pixel_height: u32,
    #[prost(fixed64, tag = "6")]
    pub generation: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct TerminalControlResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(enumeration = "TerminalControlStatus", tag = "2")]
    pub status: i32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Enumeration)]
#[repr(i32)]
pub enum TerminalControlStatus {
    Accepted = 0,
    InvalidRequest = 1,
    UnknownSession = 2,
    Backpressure = 3,
    SessionClosed = 4,
    Failed = 5,
}

impl TerminalControlResponse {
    #[must_use]
    pub fn new(session_id: Vec<u8>, status: TerminalControlStatus) -> Self {
        Self {
            session_id,
            status: status as i32,
        }
    }

    pub fn decoded_status(&self) -> Result<TerminalControlStatus, TerminalControlCodecError> {
        TerminalControlStatus::try_from(self.status)
            .map_err(|_| TerminalControlCodecError::UnknownStatus(self.status))
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TerminalControlCodecError {
    #[error("terminal control session ID must contain exactly 16 bytes, got {0}")]
    InvalidSessionIdLength(usize),
    #[error("terminal input request must contain exactly one action")]
    InvalidActionCount,
    #[error("terminal key kind {0} is unknown")]
    UnknownKeyKind(i32),
    #[error("terminal control action {0} is unknown")]
    UnknownControlAction(i32),
    #[error("terminal key payload does not match its kind")]
    InvalidKeyPayload,
    #[error("terminal size {rows}x{cols} is invalid")]
    InvalidSize { rows: u32, cols: u32 },
    #[error("terminal input action is not supported by this protocol version")]
    UnsupportedInputAction,
    #[error("terminal input contains {actual} bytes; maximum is {maximum}")]
    TooManyInputBytes { actual: usize, maximum: usize },
    #[error("terminal pixel size {width}x{height} is invalid")]
    InvalidPixelSize { width: u32, height: u32 },
    #[error("terminal control response status {0} is unknown")]
    UnknownStatus(i32),
}

impl TerminalInputRequest {
    pub fn from_action(
        session_id: SessionId,
        action: &InputAction,
    ) -> Result<Self, TerminalControlCodecError> {
        let mut request = Self {
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            key: None,
            text: None,
            paste: None,
            control: None,
        };
        match action {
            InputAction::Text(text) => request.text = Some(text.clone()),
            InputAction::Key(event) => request.key = Some(TerminalKeyEvent::try_from(event)?),
            InputAction::Paste { text, bracketed } => {
                request.paste = Some(TerminalPaste {
                    text: text.clone(),
                    bracketed: *bracketed,
                });
            }
            InputAction::Control(control) => {
                request.control = Some(TerminalControlAction::from(*control) as i32);
            }
            _ => return Err(TerminalControlCodecError::UnsupportedInputAction),
        }
        request.validate_input_size()?;
        Ok(request)
    }

    pub fn decode_action(&self) -> Result<InputAction, TerminalControlCodecError> {
        validate_control_session_id(&self.session_id)?;
        let action_count = usize::from(self.key.is_some())
            + usize::from(self.text.is_some())
            + usize::from(self.paste.is_some())
            + usize::from(self.control.is_some());
        if action_count != 1 {
            return Err(TerminalControlCodecError::InvalidActionCount);
        }
        self.validate_input_size()?;
        if let Some(key) = &self.key {
            return Ok(InputAction::Key(key.try_into()?));
        }
        if let Some(text) = &self.text {
            return Ok(InputAction::Text(text.clone()));
        }
        if let Some(paste) = &self.paste {
            return Ok(InputAction::Paste {
                text: paste.text.clone(),
                bracketed: paste.bracketed,
            });
        }
        let control = self
            .control
            .ok_or(TerminalControlCodecError::InvalidActionCount)?;
        Ok(InputAction::Control(
            TerminalControlAction::try_from(control)
                .map_err(|_| TerminalControlCodecError::UnknownControlAction(control))?
                .into(),
        ))
    }

    fn validate_input_size(&self) -> Result<(), TerminalControlCodecError> {
        let actual = self
            .key
            .as_ref()
            .map_or(0, |key| key.character.len())
            .saturating_add(self.text.as_ref().map_or(0, String::len))
            .saturating_add(self.paste.as_ref().map_or(0, |paste| paste.text.len()));
        if actual > MAX_TERMINAL_INPUT_BYTES {
            Err(TerminalControlCodecError::TooManyInputBytes {
                actual,
                maximum: MAX_TERMINAL_INPUT_BYTES,
            })
        } else {
            Ok(())
        }
    }
}

impl TryFrom<&KeyEvent> for TerminalKeyEvent {
    type Error = TerminalControlCodecError;

    fn try_from(event: &KeyEvent) -> Result<Self, Self::Error> {
        let (kind, character, function) = match &event.code {
            KeyCode::Character(text) => (TerminalKeyKind::Character, text.clone(), 0),
            KeyCode::Enter => (TerminalKeyKind::Enter, String::new(), 0),
            KeyCode::Tab => (TerminalKeyKind::Tab, String::new(), 0),
            KeyCode::Backspace => (TerminalKeyKind::Backspace, String::new(), 0),
            KeyCode::Escape => (TerminalKeyKind::Escape, String::new(), 0),
            KeyCode::ArrowUp => (TerminalKeyKind::ArrowUp, String::new(), 0),
            KeyCode::ArrowDown => (TerminalKeyKind::ArrowDown, String::new(), 0),
            KeyCode::ArrowLeft => (TerminalKeyKind::ArrowLeft, String::new(), 0),
            KeyCode::ArrowRight => (TerminalKeyKind::ArrowRight, String::new(), 0),
            KeyCode::Function(number) => {
                (TerminalKeyKind::Function, String::new(), u32::from(*number))
            }
            _ => return Err(TerminalControlCodecError::UnsupportedInputAction),
        };
        Ok(Self {
            kind: kind as i32,
            character,
            function,
            ctrl: event.modifiers.ctrl,
            alt: event.modifiers.alt,
            shift: event.modifiers.shift,
            super_key: event.modifiers.super_key,
            pressed: event.pressed,
            repeated: event.repeated,
        })
    }
}

impl TryFrom<&TerminalKeyEvent> for KeyEvent {
    type Error = TerminalControlCodecError;

    fn try_from(event: &TerminalKeyEvent) -> Result<Self, Self::Error> {
        let kind = TerminalKeyKind::try_from(event.kind)
            .map_err(|_| TerminalControlCodecError::UnknownKeyKind(event.kind))?;
        let code = match kind {
            TerminalKeyKind::Character if !event.character.is_empty() && event.function == 0 => {
                KeyCode::Character(event.character.clone())
            }
            TerminalKeyKind::Function if event.character.is_empty() => {
                let number = u8::try_from(event.function)
                    .ok()
                    .filter(|number| (1..=24).contains(number))
                    .ok_or(TerminalControlCodecError::InvalidKeyPayload)?;
                KeyCode::Function(number)
            }
            TerminalKeyKind::Enter if event.character.is_empty() && event.function == 0 => {
                KeyCode::Enter
            }
            TerminalKeyKind::Tab if event.character.is_empty() && event.function == 0 => {
                KeyCode::Tab
            }
            TerminalKeyKind::Backspace if event.character.is_empty() && event.function == 0 => {
                KeyCode::Backspace
            }
            TerminalKeyKind::Escape if event.character.is_empty() && event.function == 0 => {
                KeyCode::Escape
            }
            TerminalKeyKind::ArrowUp if event.character.is_empty() && event.function == 0 => {
                KeyCode::ArrowUp
            }
            TerminalKeyKind::ArrowDown if event.character.is_empty() && event.function == 0 => {
                KeyCode::ArrowDown
            }
            TerminalKeyKind::ArrowLeft if event.character.is_empty() && event.function == 0 => {
                KeyCode::ArrowLeft
            }
            TerminalKeyKind::ArrowRight if event.character.is_empty() && event.function == 0 => {
                KeyCode::ArrowRight
            }
            _ => return Err(TerminalControlCodecError::InvalidKeyPayload),
        };
        Ok(Self {
            code,
            modifiers: Modifiers {
                ctrl: event.ctrl,
                alt: event.alt,
                shift: event.shift,
                super_key: event.super_key,
            },
            pressed: event.pressed,
            repeated: event.repeated,
        })
    }
}

impl TerminalResizeRequest {
    #[must_use]
    pub fn from_size(session_id: SessionId, size: TerminalSize) -> Self {
        Self {
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            rows: u32::from(size.rows),
            cols: u32::from(size.cols),
            pixel_width: u32::from(size.pixel_width),
            pixel_height: u32::from(size.pixel_height),
            generation: size.generation,
        }
    }

    pub fn decode_size(&self) -> Result<TerminalSize, TerminalControlCodecError> {
        validate_control_session_id(&self.session_id)?;
        let rows = u16::try_from(self.rows).ok().filter(|value| *value > 0);
        let cols = u16::try_from(self.cols).ok().filter(|value| *value > 0);
        let (Some(rows), Some(cols)) = (rows, cols) else {
            return Err(TerminalControlCodecError::InvalidSize {
                rows: self.rows,
                cols: self.cols,
            });
        };
        if usize::from(rows).saturating_mul(usize::from(cols)) > 1_000_000 {
            return Err(TerminalControlCodecError::InvalidSize {
                rows: self.rows,
                cols: self.cols,
            });
        }
        let pixel_width = u16::try_from(self.pixel_width).map_err(|_| {
            TerminalControlCodecError::InvalidPixelSize {
                width: self.pixel_width,
                height: self.pixel_height,
            }
        })?;
        let pixel_height = u16::try_from(self.pixel_height).map_err(|_| {
            TerminalControlCodecError::InvalidPixelSize {
                width: self.pixel_width,
                height: self.pixel_height,
            }
        })?;
        Ok(TerminalSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
            generation: self.generation,
        })
    }
}

fn validate_control_session_id(bytes: &[u8]) -> Result<(), TerminalControlCodecError> {
    if bytes.len() == 16 {
        Ok(())
    } else {
        Err(TerminalControlCodecError::InvalidSessionIdLength(
            bytes.len(),
        ))
    }
}

impl From<ControlAction> for TerminalControlAction {
    fn from(action: ControlAction) -> Self {
        match action {
            ControlAction::Interrupt => Self::Interrupt,
            ControlAction::EndOfFile => Self::EndOfFile,
            ControlAction::Suspend => Self::Suspend,
            ControlAction::ClearScreen => Self::ClearScreen,
        }
    }
}

impl From<TerminalControlAction> for ControlAction {
    fn from(action: TerminalControlAction) -> Self {
        match action {
            TerminalControlAction::Interrupt => Self::Interrupt,
            TerminalControlAction::EndOfFile => Self::EndOfFile,
            TerminalControlAction::Suspend => Self::Suspend,
            TerminalControlAction::ClearScreen => Self::ClearScreen,
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct LogPageRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    /// Missing means "follow the current tail".
    #[prost(fixed64, optional, tag = "2")]
    pub anchor_line_id: Option<u64>,
    #[prost(uint32, tag = "3")]
    pub cell_offset: u32,
    #[prost(uint32, tag = "4")]
    pub rows_before: u32,
    #[prost(uint32, tag = "5")]
    pub rows_after: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct LogPage {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(fixed64, tag = "2")]
    pub revision: u64,
    #[prost(fixed64, tag = "3")]
    pub anchor_line_id: u64,
    #[prost(message, repeated, tag = "4")]
    pub rows: Vec<LogRow>,
    #[prost(fixed64, tag = "5")]
    pub total_line_count: u64,
    #[prost(bool, tag = "6")]
    pub has_before: bool,
    #[prost(bool, tag = "7")]
    pub has_after: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct LogRow {
    #[prost(fixed64, tag = "1")]
    pub line_id: u64,
    #[prost(string, tag = "2")]
    pub text: String,
    #[prost(message, repeated, tag = "3")]
    pub style_spans: Vec<LogStyleSpan>,
    #[prost(bool, tag = "4")]
    pub truncated: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct LogStyleSpan {
    #[prost(uint32, tag = "1")]
    pub start: u32,
    #[prost(uint32, tag = "2")]
    pub end: u32,
    #[prost(message, optional, tag = "3")]
    pub style: Option<TerminalStyle>,
}

#[derive(Debug, Error)]
pub enum LogPageCodecError {
    #[error("log page session ID must contain exactly 16 bytes, got {0}")]
    InvalidSessionIdLength(usize),
    #[error("log page contains {0} rows; maximum is {MAX_LOG_PAGE_ROWS}")]
    TooManyRows(usize),
    #[error("log page rows are not strictly ordered by line id")]
    UnorderedRows,
    #[error("log page total line count {total} is smaller than its {page_rows} rows")]
    InvalidTotal { total: u64, page_rows: usize },
    #[error("log page contains {0} text bytes; maximum is {MAX_LOG_PAGE_TEXT_BYTES}")]
    TooManyTextBytes(usize),
    #[error("log page contains {0} style spans; maximum is {MAX_LOG_PAGE_STYLE_SPANS}")]
    TooManyStyleSpans(usize),
    #[error("log row {line_id} contains a line break or unsupported control character")]
    InvalidRowText { line_id: u64 },
    #[error("log row {line_id} contains an invalid, overlapping, or non-grapheme style range")]
    InvalidStyleSpan { line_id: u64 },
    #[error("log row {line_id} contains a style span without a style")]
    MissingStyle { line_id: u64 },
    #[error("log page does not contain its declared anchor line {0}")]
    AnchorMissing(u64),
    #[error(transparent)]
    TerminalStyle(#[from] SnapshotCodecError),
}

impl LogPageRequest {
    pub fn validate(&self) -> Result<(), LogPageCodecError> {
        if self.session_id.len() != 16 {
            return Err(LogPageCodecError::InvalidSessionIdLength(
                self.session_id.len(),
            ));
        }
        if self.rows_before as usize > MAX_LOG_PAGE_ROWS
            || self.rows_after as usize > MAX_LOG_PAGE_ROWS
        {
            return Err(LogPageCodecError::TooManyRows(
                self.rows_before.max(self.rows_after) as usize,
            ));
        }
        Ok(())
    }
}

impl LogPage {
    pub fn validate(&self) -> Result<(), LogPageCodecError> {
        if self.session_id.len() != 16 {
            return Err(LogPageCodecError::InvalidSessionIdLength(
                self.session_id.len(),
            ));
        }
        if self.rows.len() > MAX_LOG_PAGE_ROWS {
            return Err(LogPageCodecError::TooManyRows(self.rows.len()));
        }
        if self
            .rows
            .windows(2)
            .any(|rows| rows[0].line_id >= rows[1].line_id)
        {
            return Err(LogPageCodecError::UnorderedRows);
        }
        if self.total_line_count < self.rows.len() as u64 {
            return Err(LogPageCodecError::InvalidTotal {
                total: self.total_line_count,
                page_rows: self.rows.len(),
            });
        }
        let text_bytes = self
            .rows
            .iter()
            .try_fold(0_usize, |total, row| total.checked_add(row.text.len()))
            .unwrap_or(usize::MAX);
        if text_bytes > MAX_LOG_PAGE_TEXT_BYTES {
            return Err(LogPageCodecError::TooManyTextBytes(text_bytes));
        }
        let style_spans = self
            .rows
            .iter()
            .try_fold(0_usize, |total, row| {
                total.checked_add(row.style_spans.len())
            })
            .unwrap_or(usize::MAX);
        if style_spans > MAX_LOG_PAGE_STYLE_SPANS {
            return Err(LogPageCodecError::TooManyStyleSpans(style_spans));
        }
        for row in &self.rows {
            if row
                .text
                .chars()
                .any(|character| character.is_control() && character != '\t')
            {
                return Err(LogPageCodecError::InvalidRowText {
                    line_id: row.line_id,
                });
            }
            let boundaries: HashSet<_> = row
                .text
                .grapheme_indices(true)
                .map(|(index, _)| index)
                .chain(std::iter::once(row.text.len()))
                .collect();
            let mut previous_end = 0_usize;
            for span in &row.style_spans {
                let start = span.start as usize;
                let end = span.end as usize;
                if start >= end
                    || start < previous_end
                    || !boundaries.contains(&start)
                    || !boundaries.contains(&end)
                {
                    return Err(LogPageCodecError::InvalidStyleSpan {
                        line_id: row.line_id,
                    });
                }
                let style = span.style.clone().ok_or(LogPageCodecError::MissingStyle {
                    line_id: row.line_id,
                })?;
                let _validated = Style::try_from(style)?;
                previous_end = end;
            }
        }
        if self.rows.is_empty() {
            if self.anchor_line_id != 0 {
                return Err(LogPageCodecError::AnchorMissing(self.anchor_line_id));
            }
        } else if self
            .rows
            .binary_search_by_key(&self.anchor_line_id, |row| row.line_id)
            .is_err()
        {
            return Err(LogPageCodecError::AnchorMissing(self.anchor_line_id));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum HistorySearchDirection {
    Forward = 0,
    Backward = 1,
}

#[derive(Clone, PartialEq, Message)]
pub struct HistorySearchRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub query: String,
    #[prost(bool, tag = "3")]
    pub case_sensitive: bool,
    #[prost(bool, tag = "4")]
    pub whole_word: bool,
    #[prost(bool, tag = "5")]
    pub regex: bool,
    #[prost(enumeration = "HistorySearchDirection", tag = "6")]
    pub direction: i32,
    #[prost(fixed64, optional, tag = "7")]
    pub cursor_line_id: Option<u64>,
    #[prost(uint32, optional, tag = "8")]
    pub cursor_byte_offset: Option<u32>,
    #[prost(uint32, tag = "9")]
    pub max_scan_lines: u32,
    #[prost(uint32, tag = "10")]
    pub max_matches: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct HistorySearchMatch {
    #[prost(fixed64, tag = "1")]
    pub line_id: u64,
    #[prost(uint32, tag = "2")]
    pub byte_start: u32,
    #[prost(uint32, tag = "3")]
    pub byte_end: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct HistorySearchResult {
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    #[prost(fixed64, tag = "2")]
    pub revision: u64,
    #[prost(message, repeated, tag = "3")]
    pub matches: Vec<HistorySearchMatch>,
    #[prost(uint32, tag = "4")]
    pub scanned_lines: u32,
    #[prost(fixed64, optional, tag = "5")]
    pub next_line_id: Option<u64>,
    #[prost(uint32, optional, tag = "6")]
    pub next_byte_offset: Option<u32>,
    #[prost(bool, tag = "7")]
    pub incomplete: bool,
}

#[derive(Debug, Error)]
pub enum HistorySearchCodecError {
    #[error("history search session ID must contain exactly 16 bytes, got {0}")]
    InvalidSessionIdLength(usize),
    #[error(
        "history search query must contain between 1 and {MAX_HISTORY_SEARCH_QUERY_BYTES} bytes"
    )]
    InvalidQueryLength,
    #[error("history search direction is unknown")]
    UnknownDirection,
    #[error(
        "history search cursor line and byte offset must either both be present or both be absent"
    )]
    PartialCursor,
    #[error("history search line cursor must not be zero")]
    ZeroLineCursor,
    #[error("history search scan limit must be between 1 and {MAX_HISTORY_SEARCH_SCAN_LINES}")]
    InvalidScanLimit,
    #[error("history search match limit must be between 1 and {MAX_HISTORY_SEARCH_MATCHES}")]
    InvalidMatchLimit,
    #[error("history search result contains too many matches")]
    TooManyMatches,
    #[error("history search match has an invalid line or byte range")]
    InvalidMatch,
    #[error("history search result contains an invalid continuation cursor")]
    InvalidContinuation,
    #[error("history search result scanned more than {MAX_HISTORY_SEARCH_SCAN_LINES} lines")]
    InvalidScannedLines,
}

impl HistorySearchRequest {
    pub fn validate(&self) -> Result<(), HistorySearchCodecError> {
        validate_history_session_id(&self.session_id)?;
        if self.query.is_empty() || self.query.len() > MAX_HISTORY_SEARCH_QUERY_BYTES {
            return Err(HistorySearchCodecError::InvalidQueryLength);
        }
        HistorySearchDirection::try_from(self.direction)
            .map_err(|_| HistorySearchCodecError::UnknownDirection)?;
        match (self.cursor_line_id, self.cursor_byte_offset) {
            (None, None) => {}
            (Some(0), Some(_)) => return Err(HistorySearchCodecError::ZeroLineCursor),
            (Some(_), Some(_)) => {}
            _ => return Err(HistorySearchCodecError::PartialCursor),
        }
        if self.max_scan_lines == 0 || self.max_scan_lines as usize > MAX_HISTORY_SEARCH_SCAN_LINES
        {
            return Err(HistorySearchCodecError::InvalidScanLimit);
        }
        if self.max_matches == 0 || self.max_matches as usize > MAX_HISTORY_SEARCH_MATCHES {
            return Err(HistorySearchCodecError::InvalidMatchLimit);
        }
        Ok(())
    }
}

impl HistorySearchResult {
    pub fn validate(&self) -> Result<(), HistorySearchCodecError> {
        validate_history_session_id(&self.session_id)?;
        if self.matches.len() > MAX_HISTORY_SEARCH_MATCHES {
            return Err(HistorySearchCodecError::TooManyMatches);
        }
        if self.scanned_lines as usize > MAX_HISTORY_SEARCH_SCAN_LINES {
            return Err(HistorySearchCodecError::InvalidScannedLines);
        }
        if self
            .matches
            .iter()
            .any(|matched| matched.line_id == 0 || matched.byte_start >= matched.byte_end)
        {
            return Err(HistorySearchCodecError::InvalidMatch);
        }
        match (self.next_line_id, self.next_byte_offset) {
            (None, None) | (Some(1..), Some(_)) => Ok(()),
            _ => Err(HistorySearchCodecError::InvalidContinuation),
        }
    }
}

fn validate_history_session_id(bytes: &[u8]) -> Result<(), HistorySearchCodecError> {
    if bytes.len() == 16 {
        Ok(())
    } else {
        Err(HistorySearchCodecError::InvalidSessionIdLength(bytes.len()))
    }
}

impl Handshake {
    pub const MAGIC: u32 = 0x4353_484C;

    #[must_use]
    pub fn new(daemon_instance_id: Vec<u8>, instance_token: Vec<u8>) -> Self {
        Self {
            magic: Self::MAGIC,
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            feature_bits: 0,
            daemon_instance_id,
            instance_token,
        }
    }

    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.magic == Self::MAGIC
            && self.protocol_major == PROTOCOL_MAJOR
            && self.instance_token.len() == 32
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Envelope {
    #[prost(fixed64, tag = "1")]
    pub request_id: u64,
    #[prost(fixed64, tag = "2")]
    pub deadline_unix_ms: u64,
    #[prost(
        oneof = "envelope::Payload",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32"
    )]
    pub payload: Option<envelope::Payload>,
}

pub mod envelope {
    use super::{
        FrameDelta, FullFrame, Handshake, HandshakeAck, HistorySearchRequest, HistorySearchResult,
        LogPage, LogPageRequest, SessionCloseRequest, SessionCloseResponse, SessionCreateRequest,
        SessionCreateResponse, SessionListRequest, SessionListResponse, SnapshotRequest,
        TerminalControlResponse, TerminalInputRequest, TerminalResizeRequest,
    };
    use crate::{ProfileRequest, ProfileResponse};
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Payload {
        #[prost(message, tag = "10")]
        Handshake(Handshake),
        #[prost(bytes, tag = "11")]
        Request(Vec<u8>),
        #[prost(bytes, tag = "12")]
        Response(Vec<u8>),
        #[prost(fixed64, tag = "13")]
        CancelRequestId(u64),
        #[prost(message, tag = "14")]
        HandshakeAck(HandshakeAck),
        #[prost(message, tag = "15")]
        FullFrame(FullFrame),
        #[prost(message, tag = "16")]
        FrameDelta(FrameDelta),
        #[prost(message, tag = "17")]
        SnapshotRequest(SnapshotRequest),
        #[prost(message, tag = "18")]
        SessionListRequest(SessionListRequest),
        #[prost(message, tag = "19")]
        SessionListResponse(SessionListResponse),
        #[prost(message, tag = "20")]
        SessionCreateRequest(SessionCreateRequest),
        #[prost(message, tag = "21")]
        SessionCreateResponse(SessionCreateResponse),
        #[prost(message, tag = "22")]
        SessionCloseRequest(SessionCloseRequest),
        #[prost(message, tag = "23")]
        SessionCloseResponse(SessionCloseResponse),
        #[prost(message, tag = "24")]
        LogPageRequest(LogPageRequest),
        #[prost(message, tag = "25")]
        LogPage(LogPage),
        #[prost(message, tag = "26")]
        TerminalInputRequest(TerminalInputRequest),
        #[prost(message, tag = "27")]
        TerminalResizeRequest(TerminalResizeRequest),
        #[prost(message, tag = "28")]
        TerminalControlResponse(TerminalControlResponse),
        #[prost(message, tag = "29")]
        HistorySearchRequest(HistorySearchRequest),
        #[prost(message, tag = "30")]
        HistorySearchResult(HistorySearchResult),
        #[prost(message, tag = "31")]
        ProfileRequest(ProfileRequest),
        #[prost(message, tag = "32")]
        ProfileResponse(ProfileResponse),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        DeltaCodecError, Envelope, FrameDelta, FullFrame, HistorySearchCodecError,
        HistorySearchDirection, HistorySearchMatch, HistorySearchRequest, HistorySearchResult,
        LogPage, LogPageCodecError, LogPageRequest, LogRow, LogStyleSpan, MAX_LOG_PAGE_ROWS,
        MAX_TERMINAL_INPUT_BYTES, SnapshotCodecError, TERMINAL_FRAME_SCHEMA_VERSION, TerminalCell,
        TerminalCellWidth, TerminalCursorAppearance, TerminalFramePayload, TerminalInputRequest,
        TerminalResizeRequest, TerminalStyle, envelope,
    };
    use cshell_domain::{InputAction, KeyCode, KeyEvent, Modifiers, SessionId, TerminalSize};
    use cshell_terminal::{
        Cell, CellWidth, Color, CursorAppearance, CursorShape, FrameSnapshot, Style, TerminalModes,
    };
    use prost::Message;

    #[test]
    fn terminal_control_round_trip_preserves_typed_input_and_bounded_resize() {
        let session_id = SessionId::new();
        let action = InputAction::Key(KeyEvent {
            code: KeyCode::ArrowUp,
            modifiers: Modifiers {
                ctrl: true,
                alt: false,
                shift: true,
                super_key: false,
            },
            pressed: true,
            repeated: true,
        });
        let request = TerminalInputRequest::from_action(session_id, &action).unwrap();
        assert_eq!(request.decode_action().unwrap(), action);

        let size = TerminalSize {
            rows: 48,
            cols: 160,
            pixel_width: 1_920,
            pixel_height: 1_080,
            generation: 9,
        };
        let request = TerminalResizeRequest::from_size(session_id, size);
        assert_eq!(request.decode_size().unwrap(), size);

        let mut invalid = request;
        invalid.rows = 0;
        assert!(invalid.decode_size().is_err());

        let oversized = InputAction::Text("x".repeat(MAX_TERMINAL_INPUT_BYTES + 1));
        assert!(TerminalInputRequest::from_action(session_id, &oversized).is_err());
    }

    #[test]
    fn terminal_full_frame_round_trip_preserves_cells_styles_cursor_and_modes() {
        let session_id = SessionId::new();
        let snapshot = FrameSnapshot {
            generation: 42,
            rows: 2,
            cols: 2,
            cursor_row: 1,
            cursor_col: 1,
            cursor_appearance: CursorAppearance {
                shape: CursorShape::Beam,
                blinking: true,
                color: Some(Color::Rgb(10, 20, 30)),
            },
            terminal_modes: TerminalModes {
                application_cursor: true,
                bracketed_paste: true,
                kitty_keyboard_flags: TerminalModes::KITTY_DISAMBIGUATE
                    | TerminalModes::KITTY_REPORT_EVENTS,
                modify_other_keys: 2,
                format_other_keys: true,
            },
            cells: vec![
                Cell::with_zerowidth(
                    '\u{4f60}',
                    ['\u{301}'],
                    CellWidth::Wide,
                    Style {
                        foreground: Color::Rgb(0x12, 0x34, 0x56),
                        background: Color::Indexed(237),
                        bold: true,
                        italic: true,
                        underline: true,
                        inverse: true,
                    },
                )
                .with_hyperlink_uri("https://example.test/docs"),
                Cell::new(' ', CellWidth::WideSpacer, Style::default()),
                Cell::new('\u{3bb}', CellWidth::Single, Style::default()),
                Cell::new('\u{754c}', CellWidth::LeadingWideSpacer, Style::default()),
            ],
        };

        let frame = FullFrame::from_terminal_snapshot(session_id, &snapshot);
        assert_eq!(frame.session_id, session_id.as_uuid().as_bytes());
        assert_eq!(frame.decode_terminal_snapshot().unwrap(), snapshot);
    }

    #[test]
    fn terminal_full_frame_defaults_cursor_appearance_when_the_field_is_absent() {
        let payload = TerminalFramePayload {
            schema_version: TERMINAL_FRAME_SCHEMA_VERSION,
            rows: 1,
            cols: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: None,
            application_cursor: false,
            bracketed_paste: false,
            kitty_keyboard_flags: 0,
            modify_other_keys: 0,
            format_other_keys: false,
            cells: vec![TerminalCell {
                scalar: u32::from('A'),
                style: None,
                zerowidth_scalars: Vec::new(),
                width: TerminalCellWidth::Single as i32,
                hyperlink_uri: String::new(),
            }],
        };
        let frame = FullFrame {
            session_id: vec![0; 16],
            generation: 1,
            payload: payload.encode_to_vec(),
        };

        assert_eq!(
            frame.decode_terminal_snapshot().unwrap().cursor_appearance,
            CursorAppearance::default()
        );
    }

    #[test]
    fn malformed_terminal_full_frame_is_rejected_before_use() {
        let payload = TerminalFramePayload {
            schema_version: TERMINAL_FRAME_SCHEMA_VERSION,
            rows: 24,
            cols: 80,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: None,
            application_cursor: false,
            bracketed_paste: false,
            kitty_keyboard_flags: 0,
            modify_other_keys: 0,
            format_other_keys: false,
            cells: Vec::new(),
        };
        let frame = FullFrame {
            session_id: vec![0; 16],
            generation: 1,
            payload: payload.encode_to_vec(),
        };

        assert!(matches!(
            frame.decode_terminal_snapshot(),
            Err(SnapshotCodecError::InvalidCellCount {
                expected: 1_920,
                actual: 0
            })
        ));
    }

    #[test]
    fn terminal_cell_rejects_unbounded_combining_data_and_unknown_width() {
        let frame_for = |cell| FullFrame {
            session_id: vec![0; 16],
            generation: 1,
            payload: TerminalFramePayload {
                schema_version: TERMINAL_FRAME_SCHEMA_VERSION,
                rows: 1,
                cols: 1,
                cursor_row: 0,
                cursor_col: 0,
                cursor_appearance: None,
                application_cursor: false,
                bracketed_paste: false,
                kitty_keyboard_flags: 0,
                modify_other_keys: 0,
                format_other_keys: false,
                cells: vec![cell],
            }
            .encode_to_vec(),
        };
        let excessive = frame_for(TerminalCell {
            scalar: u32::from('e'),
            style: None,
            zerowidth_scalars: vec![0x301; 65],
            width: TerminalCellWidth::Single as i32,
            hyperlink_uri: String::new(),
        });
        assert!(matches!(
            excessive.decode_terminal_snapshot(),
            Err(SnapshotCodecError::TooManyZeroWidthScalars {
                actual: 65,
                maximum: 64
            })
        ));

        let unknown_width = frame_for(TerminalCell {
            scalar: u32::from('A'),
            style: None,
            zerowidth_scalars: Vec::new(),
            width: 99,
            hyperlink_uri: String::new(),
        });
        assert!(matches!(
            unknown_width.decode_terminal_snapshot(),
            Err(SnapshotCodecError::UnknownCellWidth(99))
        ));

        let nonzero_combining = frame_for(TerminalCell {
            scalar: u32::from('A'),
            style: None,
            zerowidth_scalars: vec![u32::from('X')],
            width: TerminalCellWidth::Single as i32,
            hyperlink_uri: String::new(),
        });
        assert!(matches!(
            nonzero_combining.decode_terminal_snapshot(),
            Err(SnapshotCodecError::NonZeroWidthCombiningScalar(value))
                if value == u32::from('X')
        ));

        let oversized_hyperlink = frame_for(TerminalCell {
            scalar: u32::from('A'),
            style: None,
            zerowidth_scalars: Vec::new(),
            width: TerminalCellWidth::Single as i32,
            hyperlink_uri: "x".repeat(super::MAX_TERMINAL_HYPERLINK_URI_BYTES + 1),
        });
        assert!(matches!(
            oversized_hyperlink.decode_terminal_snapshot(),
            Err(SnapshotCodecError::HyperlinkUriTooLong {
                actual: 4097,
                maximum: 4096
            })
        ));

        let unknown_cursor_shape = FullFrame {
            session_id: vec![0; 16],
            generation: 1,
            payload: TerminalFramePayload {
                schema_version: TERMINAL_FRAME_SCHEMA_VERSION,
                rows: 1,
                cols: 1,
                cursor_row: 0,
                cursor_col: 0,
                cursor_appearance: Some(TerminalCursorAppearance {
                    shape: 99,
                    blinking: false,
                    color: None,
                }),
                application_cursor: false,
                bracketed_paste: false,
                kitty_keyboard_flags: 0,
                modify_other_keys: 0,
                format_other_keys: false,
                cells: vec![TerminalCell {
                    scalar: u32::from('A'),
                    style: None,
                    zerowidth_scalars: Vec::new(),
                    width: TerminalCellWidth::Single as i32,
                    hyperlink_uri: String::new(),
                }],
            }
            .encode_to_vec(),
        };
        assert!(matches!(
            unknown_cursor_shape.decode_terminal_snapshot(),
            Err(SnapshotCodecError::UnknownCursorShape(99))
        ));
    }

    #[test]
    fn terminal_delta_contains_only_changed_rows_and_applies_to_its_base() {
        let session_id = SessionId::new();
        let base = FrameSnapshot {
            generation: 7,
            rows: 3,
            cols: 4,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: CursorAppearance::default(),
            terminal_modes: TerminalModes::default(),
            cells: vec![Cell::default(); 12],
        };
        let mut current = base.clone();
        current.generation = 12;
        current.cursor_row = 2;
        current.cursor_col = 3;
        current.cursor_appearance = CursorAppearance {
            shape: CursorShape::HollowBlock,
            blinking: false,
            color: Some(Color::Indexed(14)),
        };
        current.terminal_modes.bracketed_paste = true;
        current.cells[1].character = 'A';
        current.cells[9].character = '界';
        current.cells[9] = current.cells[9]
            .clone()
            .with_hyperlink_uri("https://example.test/delta");

        let delta = FrameDelta::between_terminal_snapshots(session_id, &base, &current).unwrap();
        assert_eq!(delta.base_generation, 7);
        assert_eq!(delta.generation, 12);
        assert_eq!(delta.apply_terminal_delta(&base).unwrap(), current);

        let mut wrong_base = base;
        wrong_base.generation = 8;
        assert!(matches!(
            delta.apply_terminal_delta(&wrong_base),
            Err(DeltaCodecError::BaseGenerationMismatch {
                expected: 8,
                actual: 7
            })
        ));
    }

    #[test]
    fn bounded_log_page_round_trip_preserves_unicode_and_styles() {
        let session_id = SessionId::new();
        let page = LogPage {
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            revision: 77,
            anchor_line_id: 9,
            rows: vec![LogRow {
                line_id: 9,
                text: "A中e\u{301}".to_owned(),
                style_spans: vec![LogStyleSpan {
                    start: 1,
                    end: 4,
                    style: Some(TerminalStyle::from(Style {
                        foreground: Color::Rgb(1, 2, 3),
                        ..Style::default()
                    })),
                }],
                truncated: false,
            }],
            total_line_count: 12,
            has_before: true,
            has_after: true,
        };
        page.validate().unwrap();
        let encoded = Envelope {
            request_id: 42,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::LogPage(page.clone())),
        }
        .encode_to_vec();
        let decoded = Envelope::decode(encoded.as_slice()).unwrap();
        let Some(envelope::Payload::LogPage(decoded)) = decoded.payload else {
            panic!("expected a log page envelope");
        };
        decoded.validate().unwrap();
        assert_eq!(decoded, page);
    }

    #[test]
    fn bounded_history_search_round_trip_and_validation_preserve_cursor() {
        let session_id = SessionId::new().as_uuid().as_bytes().to_vec();
        let request = HistorySearchRequest {
            session_id: session_id.clone(),
            query: "错误.*42".to_owned(),
            case_sensitive: false,
            whole_word: true,
            regex: true,
            direction: HistorySearchDirection::Backward as i32,
            cursor_line_id: Some(900),
            cursor_byte_offset: Some(17),
            max_scan_lines: 512,
            max_matches: 32,
        };
        request.validate().unwrap();
        let envelope = Envelope {
            request_id: 41,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::HistorySearchRequest(request.clone())),
        };
        let decoded = Envelope::decode(envelope.encode_to_vec().as_slice()).unwrap();
        assert!(matches!(
            decoded.payload,
            Some(envelope::Payload::HistorySearchRequest(value)) if value == request
        ));

        let result = HistorySearchResult {
            session_id,
            revision: 8,
            matches: vec![HistorySearchMatch {
                line_id: 899,
                byte_start: 7,
                byte_end: 13,
            }],
            scanned_lines: 12,
            next_line_id: Some(899),
            next_byte_offset: Some(7),
            incomplete: false,
        };
        result.validate().unwrap();

        let mut malformed = request;
        malformed.cursor_byte_offset = None;
        assert!(matches!(
            malformed.validate(),
            Err(HistorySearchCodecError::PartialCursor)
        ));
    }

    #[test]
    fn log_page_rejects_unbounded_requests_and_split_grapheme_styles() {
        let request = LogPageRequest {
            session_id: vec![1; 16],
            anchor_line_id: None,
            cell_offset: 0,
            rows_before: MAX_LOG_PAGE_ROWS as u32 + 1,
            rows_after: 0,
        };
        assert!(matches!(
            request.validate(),
            Err(LogPageCodecError::TooManyRows(_))
        ));

        let page = LogPage {
            session_id: vec![1; 16],
            revision: 1,
            anchor_line_id: 1,
            rows: vec![LogRow {
                line_id: 1,
                text: "A中e\u{301}".to_owned(),
                style_spans: vec![LogStyleSpan {
                    start: 2,
                    end: 4,
                    style: Some(TerminalStyle::from(Style::default())),
                }],
                truncated: false,
            }],
            total_line_count: 1,
            has_before: false,
            has_after: false,
        };
        assert!(matches!(
            page.validate(),
            Err(LogPageCodecError::InvalidStyleSpan { line_id: 1 })
        ));
    }
}
