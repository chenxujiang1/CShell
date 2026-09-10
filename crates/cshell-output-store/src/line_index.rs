use crate::ansi::{ANSI_STATE_BYTES, AnsiState};
use crate::{
    Durability, HEADER_SIZE, JournalError, JournalStyleSpan, MAGIC, MAX_RECORD_SIZE, StyledLogText,
    VERSION,
};
use crc32fast::Hasher;
use memchr::memchr_iter;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Barrier;
use std::sync::{Arc, RwLock};

const INDEX_SCAN_BUFFER_BYTES: usize = 64 * 1024;
const CACHE_MAGIC: [u8; 4] = *b"CSIX";
const CACHE_VERSION: u16 = 4;
const CACHE_HEADER_BYTES: usize = 92 + ANSI_STATE_BYTES;
const CACHE_SEGMENT_BYTES: usize = 24;
const CACHE_RECORD_BYTES: usize = 32 + ANSI_STATE_BYTES;
const CACHE_LINE_BLOCK_BYTES: usize = 32;
const CACHE_BLOCK_NARROW: u8 = 0;
const CACHE_BLOCK_WIDE: u8 = 1;
const CACHE_BLOCK_COLD: u8 = 2;
const LINE_BLOCK_SIZE: usize = 4096;
const LINE_BLOCK_MAGIC: [u8; 4] = *b"CSLB";
const LINE_BLOCK_VERSION: u16 = 1;
const LINE_BLOCK_HEADER_BYTES: usize = 40;
const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_LOG_PAGE_LINES: usize = 4096;
pub const MAX_LOG_PAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_LOG_PAGE_STYLE_SPANS: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedLogLine {
    pub line_id: u64,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalLogPage {
    pub revision: u64,
    pub anchor_line_id: u64,
    pub rows: Vec<IndexedLogLine>,
    pub total_line_count: u64,
    pub has_before: bool,
    pub has_after: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledIndexedLogLine {
    pub line_id: u64,
    pub text: String,
    pub style_spans: Vec<JournalStyleSpan>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalStyledLogPage {
    pub revision: u64,
    pub anchor_line_id: u64,
    pub rows: Vec<StyledIndexedLogLine>,
    pub total_line_count: u64,
    pub has_before: bool,
    pub has_after: bool,
}

#[derive(Clone, Copy, Debug)]
struct RecordLocation {
    segment_id: u32,
    logical_start: u64,
    payload_file_offset: u64,
    payload_len: u64,
    ansi_state: AnsiState,
}

impl RecordLocation {
    fn logical_end(self) -> u64 {
        self.logical_start.saturating_add(self.payload_len)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineLocation {
    start: u64,
    end: u64,
}

#[derive(Debug)]
enum LineBlockEnds {
    Narrow(Vec<u32>),
    Wide(Vec<u64>),
    Cold(ColdLineBlock),
}

#[derive(Clone, Debug)]
struct ColdLineBlock {
    block_id: u64,
    logical_base: u64,
    payload_offset: u64,
    count: usize,
    width: u8,
    checksum: u32,
    #[cfg(test)]
    read_gate: Option<ColdReadGate>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
struct ColdReadGate {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl ColdLineBlock {
    fn load(&self, path: &Path) -> Result<Vec<u64>, JournalError> {
        #[cfg(test)]
        if let Some(gate) = &self.read_gate {
            gate.entered.wait();
            gate.release.wait();
        }
        let payload_len = self
            .count
            .checked_mul(usize::from(self.width))
            .ok_or_else(|| JournalError::CorruptLineBlock {
                block_id: self.block_id,
                detail: "payload length overflow".to_owned(),
            })?;
        let mut file = File::open(path)?;
        let header_offset = self
            .payload_offset
            .checked_sub(LINE_BLOCK_HEADER_BYTES as u64)
            .ok_or_else(|| self.corrupt("payload offset precedes header"))?;
        file.seek(SeekFrom::Start(header_offset))?;
        let mut header = [0_u8; LINE_BLOCK_HEADER_BYTES];
        file.read_exact(&mut header)
            .map_err(|error| self.corrupt(format!("incomplete header: {error}")))?;
        let encoded_width = header[6];
        let encoded_block_id = u64::from_le_bytes(header[8..16].try_into().unwrap_or_default());
        let encoded_logical_base =
            u64::from_le_bytes(header[16..24].try_into().unwrap_or_default());
        let encoded_count = u32::from_le_bytes(header[24..28].try_into().unwrap_or_default());
        let encoded_payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap_or_default());
        let encoded_checksum = u32::from_le_bytes(header[32..36].try_into().unwrap_or_default());
        if header[0..4] != LINE_BLOCK_MAGIC
            || u16::from_le_bytes(header[4..6].try_into().unwrap_or_default()) != LINE_BLOCK_VERSION
            || encoded_width != self.width
            || encoded_block_id != self.block_id
            || encoded_logical_base != self.logical_base
            || encoded_count as usize != self.count
            || encoded_payload_len as usize != payload_len
            || encoded_checksum != self.checksum
        {
            return Err(self.corrupt("header metadata mismatch"));
        }
        let mut payload = vec![0_u8; payload_len];
        file.read_exact(&mut payload)
            .map_err(|error| self.corrupt(format!("incomplete payload: {error}")))?;
        if crc32fast::hash(&payload) != self.checksum {
            return Err(self.corrupt("checksum mismatch"));
        }
        let mut ends = Vec::with_capacity(self.count);
        for encoded in payload.chunks_exact(usize::from(self.width)) {
            let value = match self.width {
                4 => u64::from(u32::from_le_bytes(encoded.try_into().unwrap_or_default())),
                8 => u64::from_le_bytes(encoded.try_into().unwrap_or_default()),
                _ => {
                    return Err(JournalError::CorruptLineBlock {
                        block_id: self.block_id,
                        detail: format!("unsupported offset width {}", self.width),
                    });
                }
            };
            ends.push(value);
        }
        Ok(ends)
    }

    fn corrupt(&self, detail: impl Into<String>) -> JournalError {
        JournalError::CorruptLineBlock {
            block_id: self.block_id,
            detail: detail.into(),
        }
    }
}

#[derive(Debug)]
struct LineBlock {
    logical_base: u64,
    ends: LineBlockEnds,
}

impl LineBlock {
    fn end(&self, index: usize) -> Option<u64> {
        match &self.ends {
            LineBlockEnds::Narrow(ends) => ends
                .get(index)
                .map(|end| self.logical_base.saturating_add(u64::from(*end))),
            LineBlockEnds::Wide(ends) => ends
                .get(index)
                .map(|end| self.logical_base.saturating_add(*end)),
            LineBlockEnds::Cold(_) => None,
        }
    }

    fn push(&mut self, end: u64) {
        let relative = end.saturating_sub(self.logical_base);
        if let LineBlockEnds::Narrow(ends) = &mut self.ends {
            if let Ok(relative) = u32::try_from(relative) {
                ends.push(relative);
                return;
            }
            let mut wide = Vec::with_capacity(LINE_BLOCK_SIZE);
            wide.extend(ends.iter().map(|end| u64::from(*end)));
            self.ends = LineBlockEnds::Wide(wide);
        }
        if let LineBlockEnds::Wide(ends) = &mut self.ends {
            ends.push(relative);
        }
    }

    fn resident_bytes(&self) -> usize {
        match &self.ends {
            LineBlockEnds::Narrow(ends) => ends.capacity() * std::mem::size_of::<u32>(),
            LineBlockEnds::Wide(ends) => ends.capacity() * std::mem::size_of::<u64>(),
            LineBlockEnds::Cold(_) => 0,
        }
    }
}

#[derive(Debug, Default)]
struct BlockedLineIndex {
    blocks: Vec<LineBlock>,
    len: usize,
    store_path: Option<PathBuf>,
    store: Option<File>,
    next_store_offset: u64,
    #[cfg(test)]
    fail_store_after_bytes: Option<usize>,
}

impl BlockedLineIndex {
    fn new(store_path: PathBuf) -> Self {
        Self {
            store_path: Some(store_path),
            ..Self::default()
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, start: u64, end: u64) -> Result<(), JournalError> {
        if self.len.is_multiple_of(LINE_BLOCK_SIZE) {
            self.blocks.push(LineBlock {
                logical_base: start,
                ends: LineBlockEnds::Narrow(Vec::with_capacity(LINE_BLOCK_SIZE)),
            });
        }
        if let Some(block) = self.blocks.last_mut() {
            block.push(end);
            self.len = self.len.saturating_add(1);
        }
        self.seal_last_block()?;
        Ok(())
    }

    fn end(&self, index: usize, loaded: &LoadedLineBlocks) -> Option<u64> {
        let block_index = index / LINE_BLOCK_SIZE;
        let block = self.blocks.get(block_index)?;
        match &block.ends {
            LineBlockEnds::Cold(_) => loaded
                .get(&block_index)?
                .get(index % LINE_BLOCK_SIZE)
                .map(|end| block.logical_base.saturating_add(*end)),
            _ => block.end(index % LINE_BLOCK_SIZE),
        }
    }

    fn line(&self, index: usize, loaded: &LoadedLineBlocks) -> Option<LineLocation> {
        let end = self.end(index, loaded)?;
        let start = if index == 0 {
            0
        } else {
            self.end(index - 1, loaded)?.saturating_add(1)
        };
        Some(LineLocation { start, end })
    }

    fn cold_for_range(&self, range: Range<usize>) -> Vec<(usize, ColdLineBlock, PathBuf)> {
        if range.is_empty() {
            return Vec::new();
        }
        let Some(path) = self.store_path.as_ref() else {
            return Vec::new();
        };
        let first_line = range.start.saturating_sub(1);
        let first_block = first_line / LINE_BLOCK_SIZE;
        let last_block = range.end.saturating_sub(1) / LINE_BLOCK_SIZE;
        (first_block..=last_block)
            .filter_map(|block_index| {
                self.blocks
                    .get(block_index)
                    .and_then(|block| match &block.ends {
                        LineBlockEnds::Cold(cold) => {
                            Some((block_index, cold.clone(), path.clone()))
                        }
                        _ => None,
                    })
            })
            .collect()
    }

    fn encode_cache_blocks(&self, output: &mut Vec<u8>) -> Result<(), JournalError> {
        for block in &self.blocks {
            let (count, width, kind, payload_offset, checksum, payload) = match &block.ends {
                LineBlockEnds::Narrow(ends) => {
                    let payload = ends
                        .iter()
                        .flat_map(|end| end.to_le_bytes())
                        .collect::<Vec<_>>();
                    (
                        ends.len(),
                        4_u8,
                        CACHE_BLOCK_NARROW,
                        0_u64,
                        crc32fast::hash(&payload),
                        payload,
                    )
                }
                LineBlockEnds::Wide(ends) => {
                    let payload = ends
                        .iter()
                        .flat_map(|end| end.to_le_bytes())
                        .collect::<Vec<_>>();
                    (
                        ends.len(),
                        8_u8,
                        CACHE_BLOCK_WIDE,
                        0_u64,
                        crc32fast::hash(&payload),
                        payload,
                    )
                }
                LineBlockEnds::Cold(cold) => (
                    cold.count,
                    cold.width,
                    CACHE_BLOCK_COLD,
                    cold.payload_offset,
                    cold.checksum,
                    Vec::new(),
                ),
            };
            let payload_len = count.checked_mul(usize::from(width)).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line block payload length overflow",
                )
            })?;
            output.extend_from_slice(&block.logical_base.to_le_bytes());
            output.extend_from_slice(&(count as u32).to_le_bytes());
            output.push(width);
            output.push(kind);
            output.extend_from_slice(&0_u16.to_le_bytes());
            output.extend_from_slice(&payload_offset.to_le_bytes());
            output.extend_from_slice(&checksum.to_le_bytes());
            output.extend_from_slice(&(payload_len as u32).to_le_bytes());
            output.extend_from_slice(&payload);
        }
        Ok(())
    }

    fn has_cold_blocks(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block.ends, LineBlockEnds::Cold(_)))
    }

    fn decode_cache_blocks(
        cursor: &mut CacheCursor<'_>,
        store_path: PathBuf,
        store_len: u64,
        line_count: usize,
        block_count: usize,
        logical_bytes: u64,
    ) -> Option<Self> {
        if block_count != line_count.div_ceil(LINE_BLOCK_SIZE) {
            return None;
        }
        let mut blocks = Vec::with_capacity(block_count);
        let mut decoded_lines = 0_usize;
        let mut previous_base = None;
        for block_index in 0..block_count {
            let logical_base = cursor.u64()?;
            let count = usize::try_from(cursor.u32()?).ok()?;
            let width = *cursor.take(1)?.first()?;
            let kind = *cursor.take(1)?.first()?;
            if cursor.u16()? != 0 {
                return None;
            }
            let payload_offset = cursor.u64()?;
            let checksum = cursor.u32()?;
            let payload_len = usize::try_from(cursor.u32()?).ok()?;
            let expected_count = line_count
                .saturating_sub(decoded_lines)
                .min(LINE_BLOCK_SIZE);
            if count != expected_count
                || count == 0
                || !matches!(width, 4 | 8)
                || payload_len != count.checked_mul(usize::from(width))?
                || previous_base.is_some_and(|base| logical_base <= base)
                || logical_base >= logical_bytes
            {
                return None;
            }
            let ends = match kind {
                CACHE_BLOCK_NARROW if width == 4 && payload_offset == 0 => {
                    let payload = cursor.take(payload_len)?;
                    if crc32fast::hash(payload) != checksum {
                        return None;
                    }
                    let values = payload
                        .chunks_exact(4)
                        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap_or_default()))
                        .collect::<Vec<_>>();
                    if !valid_relative_ends_u32(&values, logical_base, logical_bytes) {
                        return None;
                    }
                    LineBlockEnds::Narrow(values)
                }
                CACHE_BLOCK_WIDE if width == 8 && payload_offset == 0 => {
                    let payload = cursor.take(payload_len)?;
                    if crc32fast::hash(payload) != checksum {
                        return None;
                    }
                    let values = payload
                        .chunks_exact(8)
                        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap_or_default()))
                        .collect::<Vec<_>>();
                    if !valid_relative_ends_u64(&values, logical_base, logical_bytes) {
                        return None;
                    }
                    LineBlockEnds::Wide(values)
                }
                CACHE_BLOCK_COLD
                    if count == LINE_BLOCK_SIZE
                        && payload_offset >= LINE_BLOCK_HEADER_BYTES as u64
                        && payload_offset.saturating_add(payload_len as u64) <= store_len =>
                {
                    LineBlockEnds::Cold(ColdLineBlock {
                        block_id: block_index as u64,
                        logical_base,
                        payload_offset,
                        count,
                        width,
                        checksum,
                        #[cfg(test)]
                        read_gate: None,
                    })
                }
                _ => return None,
            };
            blocks.push(LineBlock { logical_base, ends });
            decoded_lines = decoded_lines.saturating_add(count);
            previous_base = Some(logical_base);
        }
        Some(Self {
            blocks,
            len: decoded_lines,
            store_path: Some(store_path),
            store: None,
            next_store_offset: store_len,
            #[cfg(test)]
            fail_store_after_bytes: None,
        })
    }

    fn last_end(&self) -> Result<Option<u64>, JournalError> {
        let Some(block) = self.blocks.last() else {
            return Ok(None);
        };
        let relative = match &block.ends {
            LineBlockEnds::Narrow(ends) => ends.last().copied().map(u64::from),
            LineBlockEnds::Wide(ends) => ends.last().copied(),
            LineBlockEnds::Cold(cold) => {
                let path = self.store_path.as_deref().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "cold line block has no backing store",
                    )
                })?;
                cold.load(path)?.last().copied()
            }
        };
        Ok(relative.map(|end| block.logical_base.saturating_add(end)))
    }

    fn flush_store(&mut self, durability: Durability) -> Result<(), JournalError> {
        let Some(store) = self.store.as_mut() else {
            return Ok(());
        };
        store.flush()?;
        match durability {
            Durability::Ephemeral => Ok(()),
            Durability::SessionLog => store.sync_data().map_err(Into::into),
            Durability::Audit => store.sync_all().map_err(Into::into),
        }
    }

    fn write_store_record(&mut self, header: &[u8], payload: &[u8]) -> Result<(), std::io::Error> {
        let store = self.store.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "line block store is unavailable",
            )
        })?;
        #[cfg(test)]
        {
            write_all_with_fault(store, header, &mut self.fail_store_after_bytes)?;
            write_all_with_fault(store, payload, &mut self.fail_store_after_bytes)
        }
        #[cfg(not(test))]
        {
            store.write_all(header)?;
            store.write_all(payload)
        }
    }

    fn resident_bytes(&self) -> usize {
        self.blocks.capacity() * std::mem::size_of::<LineBlock>()
            + self
                .blocks
                .iter()
                .map(LineBlock::resident_bytes)
                .sum::<usize>()
    }

    fn seal_last_block(&mut self) -> Result<(), JournalError> {
        if self.store_path.is_none() || !self.len.is_multiple_of(LINE_BLOCK_SIZE) {
            return Ok(());
        }
        let block_id = self.blocks.len().saturating_sub(1) as u64;
        let Some(block) = self.blocks.last() else {
            return Ok(());
        };
        let (width, payload) = match &block.ends {
            LineBlockEnds::Narrow(ends) => (
                4_u8,
                ends.iter()
                    .flat_map(|end| end.to_le_bytes())
                    .collect::<Vec<_>>(),
            ),
            LineBlockEnds::Wide(ends) => (
                8_u8,
                ends.iter()
                    .flat_map(|end| end.to_le_bytes())
                    .collect::<Vec<_>>(),
            ),
            LineBlockEnds::Cold(_) => return Ok(()),
        };
        if self.store.is_none() {
            let path = self.store_path.as_ref().cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "line block path is missing")
            })?;
            self.store = Some(
                OpenOptions::new()
                    .create(true)
                    .truncate(self.next_store_offset == 0)
                    .read(true)
                    .write(true)
                    .open(path)?,
            );
        }
        let checksum = crc32fast::hash(&payload);
        let payload_offset = self
            .next_store_offset
            .saturating_add(LINE_BLOCK_HEADER_BYTES as u64);
        let mut header = [0_u8; LINE_BLOCK_HEADER_BYTES];
        header[0..4].copy_from_slice(&LINE_BLOCK_MAGIC);
        header[4..6].copy_from_slice(&LINE_BLOCK_VERSION.to_le_bytes());
        header[6] = width;
        header[8..16].copy_from_slice(&block_id.to_le_bytes());
        header[16..24].copy_from_slice(&block.logical_base.to_le_bytes());
        header[24..28].copy_from_slice(&(LINE_BLOCK_SIZE as u32).to_le_bytes());
        header[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[32..36].copy_from_slice(&checksum.to_le_bytes());
        self.store
            .as_mut()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "line block store is unavailable",
                )
            })?
            .seek(SeekFrom::Start(self.next_store_offset))?;
        if let Err(error) = self.write_store_record(&header, &payload) {
            if let Some(store) = self.store.as_mut() {
                let _ = store.set_len(self.next_store_offset);
                let _ = store.seek(SeekFrom::Start(self.next_store_offset));
            }
            return Err(error.into());
        }
        self.next_store_offset = payload_offset.saturating_add(payload.len() as u64);
        if let Some(block) = self.blocks.last_mut() {
            block.ends = LineBlockEnds::Cold(ColdLineBlock {
                block_id,
                logical_base: block.logical_base,
                payload_offset,
                count: LINE_BLOCK_SIZE,
                width,
                checksum,
                #[cfg(test)]
                read_gate: None,
            });
        }
        Ok(())
    }
}

struct VerifiedRecord {
    segment_id: u32,
    file_offset: u64,
    sequence: u64,
    payload_len: usize,
    newline_offsets: Vec<usize>,
    ansi_before: AnsiState,
    ansi_after: AnsiState,
}

impl LineLocation {
    fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug)]
struct JournalLineIndexState {
    segment_paths: BTreeMap<u32, PathBuf>,
    revision: u64,
    logical_bytes: u64,
    open_line_start: u64,
    last_sequence: Option<u64>,
    records: Vec<RecordLocation>,
    completed_lines: BlockedLineIndex,
    integrity_error: Option<String>,
    ansi_state: AnsiState,
}

impl JournalLineIndexState {
    fn empty(path: PathBuf) -> Self {
        let line_store_path = line_block_store_path(&path);
        let mut segment_paths = BTreeMap::new();
        segment_paths.insert(0, path);
        Self {
            segment_paths,
            revision: 0,
            logical_bytes: 0,
            open_line_start: 0,
            last_sequence: None,
            records: Vec::new(),
            completed_lines: BlockedLineIndex::new(line_store_path),
            integrity_error: None,
            ansi_state: AnsiState::default(),
        }
    }

    fn line_count(&self) -> usize {
        self.completed_lines.len() + usize::from(self.open_line_start < self.logical_bytes)
    }

    fn line(&self, index: usize, loaded: &LoadedLineBlocks) -> Option<LineLocation> {
        self.completed_lines.line(index, loaded).or_else(|| {
            (index == self.completed_lines.len() && self.open_line_start < self.logical_bytes)
                .then_some(LineLocation {
                    start: self.open_line_start,
                    end: self.logical_bytes,
                })
        })
    }

    fn append_verified_record(&mut self, record: VerifiedRecord) -> Result<(), JournalError> {
        let logical_start = self.logical_bytes;
        let mut index_error = None;
        for newline_offset in record.newline_offsets {
            let newline = logical_start.saturating_add(newline_offset as u64);
            if let Err(error) = self.completed_lines.push(self.open_line_start, newline)
                && index_error.is_none()
            {
                index_error = Some(error);
            }
            self.open_line_start = newline.saturating_add(1);
        }
        self.records.push(RecordLocation {
            segment_id: record.segment_id,
            logical_start,
            payload_file_offset: record.file_offset.saturating_add(HEADER_SIZE as u64),
            payload_len: record.payload_len as u64,
            ansi_state: record.ansi_before,
        });
        self.logical_bytes = self.logical_bytes.saturating_add(record.payload_len as u64);
        self.revision = self
            .revision
            .saturating_add(HEADER_SIZE as u64)
            .saturating_add(record.payload_len as u64);
        self.last_sequence = Some(record.sequence);
        self.integrity_error = None;
        self.ansi_state = record.ansi_after;
        match index_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

type LoadedLineBlocks = BTreeMap<usize, Vec<u64>>;

#[cfg(test)]
fn write_all_with_fault(
    store: &mut File,
    bytes: &[u8],
    remaining: &mut Option<usize>,
) -> Result<(), std::io::Error> {
    let Some(remaining_bytes) = remaining.as_mut() else {
        return store.write_all(bytes);
    };
    let writable = bytes.len().min(*remaining_bytes);
    if writable > 0 {
        store.write_all(&bytes[..writable])?;
        *remaining_bytes -= writable;
    }
    if writable != bytes.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "injected cold line-index storage exhaustion",
        ));
    }
    Ok(())
}

fn valid_relative_ends_u32(values: &[u32], logical_base: u64, logical_bytes: u64) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
        && values
            .last()
            .is_some_and(|end| logical_base.saturating_add(u64::from(*end)) < logical_bytes)
}

fn valid_relative_ends_u64(values: &[u64], logical_base: u64, logical_bytes: u64) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
        && values
            .last()
            .is_some_and(|end| logical_base.saturating_add(*end) < logical_bytes)
}

fn line_block_store_path(journal_path: &Path) -> PathBuf {
    let mut name = journal_path
        .file_name()
        .map_or_else(|| OsString::from("journal"), OsString::from);
    name.push(".csli");
    journal_path.with_file_name(name)
}

#[derive(Clone, Debug)]
pub struct JournalLineIndex {
    inner: Arc<RwLock<JournalLineIndexState>>,
}

impl JournalLineIndex {
    pub(crate) fn load(path: &Path, repair_tail: bool) -> Result<(Self, u64, u64), JournalError> {
        let mut state = JournalLineIndexState::empty(path.to_owned());
        if !path.exists() {
            return Ok((
                Self {
                    inner: Arc::new(RwLock::new(state)),
                },
                0,
                0,
            ));
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(repair_tail)
            .open(path)?;
        let mut file_offset = 0_u64;
        loop {
            let mut header = [0_u8; HEADER_SIZE];
            let first_read = file.read(&mut header)?;
            if first_read == 0 {
                break;
            }
            if first_read < HEADER_SIZE
                && let Err(error) = file.read_exact(&mut header[first_read..])
            {
                state.integrity_error =
                    Some(format!("partial header at byte {file_offset}: {error}"));
                break;
            }
            if header[0..4] != MAGIC {
                state.integrity_error = Some(format!("invalid magic at byte {file_offset}"));
                break;
            }
            let version = u16::from_le_bytes([header[4], header[5]]);
            if version != VERSION {
                state.integrity_error = Some(format!(
                    "unsupported version {version} at byte {file_offset}"
                ));
                break;
            }
            let sequence = u64::from_le_bytes(header[8..16].try_into().unwrap_or_default());
            let payload_len =
                u32::from_le_bytes(header[16..20].try_into().unwrap_or_default()) as usize;
            let expected_checksum =
                u32::from_le_bytes(header[20..24].try_into().unwrap_or_default());
            if payload_len > MAX_RECORD_SIZE {
                state.integrity_error = Some(format!("oversized record at byte {file_offset}"));
                break;
            }

            let mut remaining = payload_len;
            let mut consumed = 0_usize;
            let mut hasher = Hasher::new();
            let mut newline_offsets = Vec::new();
            let mut buffer = [0_u8; INDEX_SCAN_BUFFER_BYTES];
            let mut payload_complete = true;
            let ansi_before = state.ansi_state;
            let mut ansi_after = ansi_before;
            while remaining > 0 {
                let wanted = remaining.min(buffer.len());
                if let Err(error) = file.read_exact(&mut buffer[..wanted]) {
                    state.integrity_error =
                        Some(format!("partial payload at byte {file_offset}: {error}"));
                    payload_complete = false;
                    break;
                }
                hasher.update(&buffer[..wanted]);
                ansi_after.advance(&buffer[..wanted]);
                newline_offsets.extend(
                    memchr_iter(b'\n', &buffer[..wanted])
                        .map(|index| consumed.saturating_add(index)),
                );
                consumed = consumed.saturating_add(wanted);
                remaining -= wanted;
            }
            if !payload_complete {
                break;
            }
            if hasher.finalize() != expected_checksum {
                state.integrity_error = Some(format!("checksum mismatch at byte {file_offset}"));
                break;
            }
            state.append_verified_record(VerifiedRecord {
                segment_id: 0,
                file_offset,
                sequence,
                payload_len,
                newline_offsets,
                ansi_before,
                ansi_after,
            })?;
            file_offset = file_offset
                .saturating_add(HEADER_SIZE as u64)
                .saturating_add(payload_len as u64);
        }

        if repair_tail && state.integrity_error.is_some() {
            file.set_len(state.revision)?;
        }
        let next_sequence = state
            .last_sequence
            .map_or(0, |sequence| sequence.saturating_add(1));
        let next_file_offset = state.revision;
        Ok((
            Self {
                inner: Arc::new(RwLock::new(state)),
            },
            next_sequence,
            next_file_offset,
        ))
    }

    pub(crate) fn append_record(
        &self,
        file_offset: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<(), JournalError> {
        let newline_offsets = memchr_iter(b'\n', payload).collect();
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ansi_before = state.ansi_state;
        let mut ansi_after = ansi_before;
        ansi_after.advance(payload);
        state.append_verified_record(VerifiedRecord {
            segment_id: 0,
            file_offset,
            sequence,
            payload_len: payload.len(),
            newline_offsets,
            ansi_before,
            ansi_after,
        })
    }

    pub(crate) fn empty_segmented(first_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(RwLock::new(JournalLineIndexState::empty(first_path))),
        }
    }

    pub(crate) fn register_segment(&self, segment_id: u32, path: PathBuf) {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .segment_paths
            .insert(segment_id, path);
    }

    pub(crate) fn append_segment_record(
        &self,
        segment_id: u32,
        file_offset: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<(), JournalError> {
        let newline_offsets = memchr_iter(b'\n', payload).collect();
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ansi_before = state.ansi_state;
        let mut ansi_after = ansi_before;
        ansi_after.advance(payload);
        state.append_verified_record(VerifiedRecord {
            segment_id,
            file_offset,
            sequence,
            payload_len: payload.len(),
            newline_offsets,
            ansi_before,
            ansi_after,
        })
    }

    /// Loads a checksummed index checkpoint after validating every encoded
    /// range against the current immutable segment sizes.
    pub(crate) fn load_segmented_cache(
        cache_path: &Path,
        segment_paths: &[(u32, PathBuf)],
    ) -> Result<Option<Self>, JournalError> {
        let mut file = match File::open(cache_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let file_len = file.metadata()?.len();
        if file_len < (CACHE_HEADER_BYTES + 4) as u64 || file_len > MAX_CACHE_BYTES {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(file_len as usize);
        file.read_to_end(&mut bytes)?;
        let Some(encoded_checksum) = bytes
            .get(bytes.len().saturating_sub(4)..)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes)
        else {
            return Ok(None);
        };
        if crc32fast::hash(&bytes[..bytes.len() - 4]) != encoded_checksum {
            return Ok(None);
        }
        let mut cursor = CacheCursor::new(&bytes[..bytes.len() - 4]);
        if cursor.take(4) != Some(CACHE_MAGIC.as_slice())
            || cursor.u16() != Some(CACHE_VERSION)
            || cursor.u16() != Some(0)
        {
            return Ok(None);
        }
        let Some(revision) = cursor.u64() else {
            return Ok(None);
        };
        let Some(logical_bytes) = cursor.u64() else {
            return Ok(None);
        };
        let Some(open_line_start) = cursor.u64() else {
            return Ok(None);
        };
        let Some(encoded_last_sequence) = cursor.u64() else {
            return Ok(None);
        };
        let Some(segment_count) = cursor.u32().and_then(|value| usize::try_from(value).ok()) else {
            return Ok(None);
        };
        let Some(record_count) = cursor.u64().and_then(|value| usize::try_from(value).ok()) else {
            return Ok(None);
        };
        let Some(line_count) = cursor.u64().and_then(|value| usize::try_from(value).ok()) else {
            return Ok(None);
        };
        let Some(block_count) = cursor.u64().and_then(|value| usize::try_from(value).ok()) else {
            return Ok(None);
        };
        let Some(encoded_store_len) = cursor.u64() else {
            return Ok(None);
        };
        let Some(store_modified_seconds) = cursor.u64() else {
            return Ok(None);
        };
        let Some(store_modified_nanos) = cursor.u32() else {
            return Ok(None);
        };
        if cursor.u32() != Some(0) {
            return Ok(None);
        }
        let Some(ansi_state) = cursor.take(ANSI_STATE_BYTES).and_then(AnsiState::decode) else {
            return Ok(None);
        };
        let Some(minimum_len) = segment_count
            .checked_mul(CACHE_SEGMENT_BYTES)
            .and_then(|segments| CACHE_HEADER_BYTES.checked_add(segments))
            .and_then(|length| {
                record_count
                    .checked_mul(CACHE_RECORD_BYTES)
                    .and_then(|records| length.checked_add(records))
            })
            .and_then(|length| {
                block_count
                    .checked_mul(CACHE_LINE_BLOCK_BYTES)
                    .and_then(|blocks| length.checked_add(blocks))
            })
        else {
            return Ok(None);
        };
        if minimum_len > bytes.len() - 4 {
            return Ok(None);
        }
        if segment_count > segment_paths.len() {
            return Err(JournalError::MissingSegment {
                segment_id: u32::try_from(segment_paths.len())
                    .map_err(|_| JournalError::TooManySegments)?,
            });
        }
        if segment_count != segment_paths.len() {
            return Ok(None);
        }

        let mut paths = BTreeMap::new();
        let mut segment_lengths = BTreeMap::new();
        for (expected_id, expected_path) in segment_paths {
            let Some(segment_id) = cursor.u32() else {
                return Ok(None);
            };
            let Some(segment_len) = cursor.u64() else {
                return Ok(None);
            };
            let Some(modified_seconds) = cursor.u64() else {
                return Ok(None);
            };
            let Some(modified_nanos) = cursor.u32() else {
                return Ok(None);
            };
            let metadata = fs::metadata(expected_path)?;
            let modified = modified_time_parts(&metadata);
            if segment_id != *expected_id
                || metadata.len() != segment_len
                || modified != (modified_seconds, modified_nanos)
            {
                return Ok(None);
            }
            paths.insert(segment_id, expected_path.clone());
            segment_lengths.insert(segment_id, segment_len);
        }

        let mut records = Vec::with_capacity(record_count);
        let mut expected_logical_start = 0_u64;
        for _ in 0..record_count {
            let Some(segment_id) = cursor.u32() else {
                return Ok(None);
            };
            if cursor.u32() != Some(0) {
                return Ok(None);
            }
            let Some(logical_start) = cursor.u64() else {
                return Ok(None);
            };
            let Some(payload_file_offset) = cursor.u64() else {
                return Ok(None);
            };
            let Some(payload_len) = cursor.u64() else {
                return Ok(None);
            };
            let Some(record_ansi_state) = cursor.take(ANSI_STATE_BYTES).and_then(AnsiState::decode)
            else {
                return Ok(None);
            };
            let Some(segment_len) = segment_lengths.get(&segment_id).copied() else {
                return Ok(None);
            };
            if logical_start != expected_logical_start
                || payload_file_offset < HEADER_SIZE as u64
                || payload_file_offset.saturating_add(payload_len) > segment_len
                || payload_len > MAX_RECORD_SIZE as u64
            {
                return Ok(None);
            }
            records.push(RecordLocation {
                segment_id,
                logical_start,
                payload_file_offset,
                payload_len,
                ansi_state: record_ansi_state,
            });
            expected_logical_start = expected_logical_start.saturating_add(payload_len);
        }
        if expected_logical_start != logical_bytes
            || segment_lengths.values().copied().sum::<u64>() != revision
        {
            return Ok(None);
        }

        let line_store_path = segment_paths
            .first()
            .map(|(_, path)| line_block_store_path(path))
            .unwrap_or_else(|| line_block_store_path(cache_path));
        let store_len = if encoded_store_len == u64::MAX {
            0
        } else {
            let metadata = match fs::metadata(&line_store_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if metadata.len() != encoded_store_len
                || modified_time_parts(&metadata) != (store_modified_seconds, store_modified_nanos)
            {
                return Ok(None);
            }
            encoded_store_len
        };
        let Some(completed_lines) = BlockedLineIndex::decode_cache_blocks(
            &mut cursor,
            line_store_path,
            store_len,
            line_count,
            block_count,
            logical_bytes,
        ) else {
            return Ok(None);
        };
        if cursor.remaining() != 0
            || (completed_lines.has_cold_blocks() && encoded_store_len == u64::MAX)
        {
            return Ok(None);
        }
        let expected_open_line_start = match completed_lines.last_end() {
            Ok(Some(end)) => end.saturating_add(1),
            Ok(None) => 0,
            Err(_) => return Ok(None),
        };
        if open_line_start != expected_open_line_start {
            return Ok(None);
        }
        let last_sequence = (encoded_last_sequence != u64::MAX).then_some(encoded_last_sequence);
        let expected_last_sequence = u64::try_from(records.len())
            .ok()
            .and_then(|count| count.checked_sub(1));
        if last_sequence != expected_last_sequence || open_line_start > logical_bytes {
            return Ok(None);
        }
        Ok(Some(Self {
            inner: Arc::new(RwLock::new(JournalLineIndexState {
                segment_paths: paths,
                revision,
                logical_bytes,
                open_line_start,
                last_sequence,
                records,
                completed_lines,
                integrity_error: None,
                ansi_state,
            })),
        }))
    }

    /// Writes a checksummed checkpoint. A torn checkpoint is ignored on the
    /// next start and the journal records remain the source of truth.
    pub(crate) fn write_segmented_cache(&self, cache_path: &Path) -> Result<(), JournalError> {
        let state = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CACHE_MAGIC);
        bytes.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&state.revision.to_le_bytes());
        bytes.extend_from_slice(&state.logical_bytes.to_le_bytes());
        bytes.extend_from_slice(&state.open_line_start.to_le_bytes());
        bytes.extend_from_slice(&state.last_sequence.unwrap_or(u64::MAX).to_le_bytes());
        bytes.extend_from_slice(&(state.segment_paths.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(state.records.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(state.completed_lines.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(state.completed_lines.blocks.len() as u64).to_le_bytes());
        let store_metadata = if state.completed_lines.has_cold_blocks() {
            let path = state.completed_lines.store_path.as_deref().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "cold line blocks have no backing store",
                )
            })?;
            Some(fs::metadata(path)?)
        } else {
            None
        };
        let (store_len, store_modified_seconds, store_modified_nanos) = store_metadata
            .as_ref()
            .map(|metadata| {
                let (seconds, nanos) = modified_time_parts(metadata);
                (metadata.len(), seconds, nanos)
            })
            .unwrap_or((u64::MAX, 0, 0));
        bytes.extend_from_slice(&store_len.to_le_bytes());
        bytes.extend_from_slice(&store_modified_seconds.to_le_bytes());
        bytes.extend_from_slice(&store_modified_nanos.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        state.ansi_state.encode(&mut bytes);
        for (segment_id, path) in &state.segment_paths {
            let metadata = fs::metadata(path)?;
            let (modified_seconds, modified_nanos) = modified_time_parts(&metadata);
            bytes.extend_from_slice(&segment_id.to_le_bytes());
            bytes.extend_from_slice(&metadata.len().to_le_bytes());
            bytes.extend_from_slice(&modified_seconds.to_le_bytes());
            bytes.extend_from_slice(&modified_nanos.to_le_bytes());
        }
        for record in &state.records {
            bytes.extend_from_slice(&record.segment_id.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&record.logical_start.to_le_bytes());
            bytes.extend_from_slice(&record.payload_file_offset.to_le_bytes());
            bytes.extend_from_slice(&record.payload_len.to_le_bytes());
            record.ansi_state.encode(&mut bytes);
        }
        state.completed_lines.encode_cache_blocks(&mut bytes)?;
        let checksum = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        drop(state);

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(cache_path)?;
        file.write_all(&bytes)?;
        file.flush()?;
        Ok(())
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_sequence
            .map_or(0, |sequence| sequence.saturating_add(1))
    }

    pub(crate) fn flush_line_blocks(&self, durability: Durability) -> Result<(), JournalError> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completed_lines
            .flush_store(durability)
    }

    #[cfg(test)]
    pub(crate) fn fail_line_store_after(&self, bytes: usize) {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completed_lines
            .fail_store_after_bytes = Some(bytes);
    }

    #[cfg(test)]
    fn gate_cold_block_read(
        &self,
        block_index: usize,
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    ) -> bool {
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(LineBlock {
            ends: LineBlockEnds::Cold(cold),
            ..
        }) = state.completed_lines.blocks.get_mut(block_index)
        else {
            return false;
        };
        cold.read_gate = Some(ColdReadGate { entered, release });
        true
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision
    }

    #[must_use]
    pub fn logical_bytes(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .logical_bytes
    }

    #[must_use]
    pub fn record_count(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .len() as u64
    }

    #[must_use]
    pub fn total_line_count(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .line_count() as u64
    }

    /// Estimated heap bytes retained by the completed-line block directory.
    #[must_use]
    pub fn resident_line_index_bytes(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completed_lines
            .resident_bytes()
    }

    #[must_use]
    pub fn integrity_error(&self) -> Option<String> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .integrity_error
            .clone()
    }

    /// Plans under the index lock, then releases it before reading journal bytes.
    pub fn read_page(
        &self,
        anchor_line_id: Option<u64>,
        rows_before: usize,
        rows_after: usize,
    ) -> Result<JournalLogPage, JournalError> {
        let plan = self.page_plan(anchor_line_id, rows_before, rows_after)?;
        plan.read()
    }

    /// Reads and ANSI-decodes a page from the nearest persisted record-state
    /// checkpoint, so styling remains correct when a page starts mid-stream.
    pub fn read_styled_page(
        &self,
        anchor_line_id: Option<u64>,
        rows_before: usize,
        rows_after: usize,
    ) -> Result<JournalStyledLogPage, JournalError> {
        let plan = self.page_plan(anchor_line_id, rows_before, rows_after)?;
        plan.read_styled()
    }

    fn page_plan(
        &self,
        anchor_line_id: Option<u64>,
        rows_before: usize,
        rows_after: usize,
    ) -> Result<PageReadPlan, JournalError> {
        let resolved_anchor = {
            let state = self
                .inner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some((resolved_anchor, _)) =
                requested_line_range(state.line_count(), anchor_line_id, rows_before, rows_after)?
            else {
                return PageReadPlan::from_state(
                    &state,
                    anchor_line_id,
                    rows_before,
                    rows_after,
                    &BTreeMap::new(),
                );
            };
            resolved_anchor
        };
        let mut loaded = LoadedLineBlocks::new();
        loop {
            let missing = {
                let state = self
                    .inner
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (_, range) = requested_line_range(
                    state.line_count(),
                    Some(resolved_anchor),
                    rows_before,
                    rows_after,
                )?
                .ok_or(JournalError::UnknownLine(resolved_anchor))?;
                let missing: Vec<_> = state
                    .completed_lines
                    .cold_for_range(range)
                    .into_iter()
                    .filter(|(block_index, _, _)| !loaded.contains_key(block_index))
                    .collect();
                if missing.is_empty() {
                    return PageReadPlan::from_state(
                        &state,
                        Some(resolved_anchor),
                        rows_before,
                        rows_after,
                        &loaded,
                    );
                }
                missing
            };
            for (block_index, cold, path) in missing {
                loaded.insert(block_index, cold.load(&path)?);
            }
        }
    }
}

fn requested_line_range(
    line_count: usize,
    anchor_line_id: Option<u64>,
    rows_before: usize,
    rows_after: usize,
) -> Result<Option<(u64, Range<usize>)>, JournalError> {
    if line_count == 0 {
        return Ok(None);
    }
    let anchor_index = match anchor_line_id {
        Some(0) => return Err(JournalError::UnknownLine(0)),
        Some(line_id) => usize::try_from(line_id - 1)
            .ok()
            .filter(|index| *index < line_count)
            .ok_or(JournalError::UnknownLine(line_id))?,
        None => line_count - 1,
    };
    let rows_before = rows_before.min(MAX_LOG_PAGE_LINES.saturating_sub(1));
    let rows_after = rows_after.min(MAX_LOG_PAGE_LINES.saturating_sub(1));
    let mut start = anchor_index.saturating_sub(rows_before);
    let mut end = anchor_index
        .saturating_add(rows_after)
        .saturating_add(1)
        .min(line_count);
    while end.saturating_sub(start) > MAX_LOG_PAGE_LINES {
        if anchor_index - start >= end - anchor_index - 1 {
            start += 1;
        } else {
            end -= 1;
        }
    }
    Ok(Some((anchor_index as u64 + 1, start..end)))
}

fn modified_time_parts(metadata: &fs::Metadata) -> (u64, u32) {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or((0, 0), |duration| {
            (duration.as_secs(), duration.subsec_nanos())
        })
}

struct CacheCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> CacheCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.position.checked_add(count)?;
        let result = self.bytes.get(self.position..end)?;
        self.position = end;
        Some(result)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }
}

#[derive(Debug)]
struct PageReadPlan {
    segment_paths: BTreeMap<u32, PathBuf>,
    revision: u64,
    anchor_line_id: u64,
    total_line_count: u64,
    has_before: bool,
    has_after: bool,
    lines: Vec<(u64, LineLocation)>,
    records: Vec<RecordLocation>,
}

impl PageReadPlan {
    fn from_state(
        state: &JournalLineIndexState,
        anchor_line_id: Option<u64>,
        rows_before: usize,
        rows_after: usize,
        loaded: &LoadedLineBlocks,
    ) -> Result<Self, JournalError> {
        let line_count = state.line_count();
        if line_count == 0 {
            return Ok(Self {
                segment_paths: state.segment_paths.clone(),
                revision: state.revision,
                anchor_line_id: 0,
                total_line_count: 0,
                has_before: false,
                has_after: false,
                lines: Vec::new(),
                records: Vec::new(),
            });
        }
        let Some((resolved_anchor, range)) =
            requested_line_range(line_count, anchor_line_id, rows_before, rows_after)?
        else {
            unreachable!("non-empty line index must produce a page range");
        };
        let anchor_index = resolved_anchor as usize - 1;
        let mut start = range.start;
        let mut end = range.end;
        let mut selected_bytes = (start..end)
            .filter_map(|index| state.line(index, loaded))
            .fold(0_u64, |total, line| total.saturating_add(line.len()));
        while selected_bytes > MAX_LOG_PAGE_BYTES as u64 && end - start > 1 {
            if anchor_index - start >= end - anchor_index - 1 {
                if let Some(line) = state.line(start, loaded) {
                    selected_bytes = selected_bytes.saturating_sub(line.len());
                }
                start += 1;
            } else {
                end -= 1;
                if let Some(line) = state.line(end, loaded) {
                    selected_bytes = selected_bytes.saturating_sub(line.len());
                }
            }
        }
        let lines: Vec<_> = (start..end)
            .filter_map(|index| {
                state
                    .line(index, loaded)
                    .map(|line| (index as u64 + 1, line))
            })
            .collect();
        let logical_range = lines
            .first()
            .zip(lines.last())
            .map(|((_, first), (_, last))| first.start..last.end)
            .unwrap_or(0..0);
        let record_start = state
            .records
            .partition_point(|record| record.logical_end() <= logical_range.start);
        let records = state.records[record_start..]
            .iter()
            .take_while(|record| record.logical_start < logical_range.end)
            .copied()
            .collect();
        Ok(Self {
            segment_paths: state.segment_paths.clone(),
            revision: state.revision,
            anchor_line_id: anchor_index as u64 + 1,
            total_line_count: line_count as u64,
            has_before: start > 0,
            has_after: end < line_count,
            lines,
            records,
        })
    }

    fn read(self) -> Result<JournalLogPage, JournalError> {
        let mut files = BTreeMap::new();
        let mut remaining_budget = MAX_LOG_PAGE_BYTES;
        let mut rows = Vec::with_capacity(self.lines.len());
        for (line_id, location) in self.lines {
            let available = usize::try_from(location.len()).unwrap_or(usize::MAX);
            let wanted = available.min(remaining_budget);
            let mut bytes = read_logical_range(
                &mut files,
                &self.segment_paths,
                &self.records,
                location.start..location.start.saturating_add(wanted as u64),
            )?;
            let truncated = wanted < available;
            if !truncated && bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            remaining_budget = remaining_budget.saturating_sub(bytes.len());
            rows.push(IndexedLogLine {
                line_id,
                bytes,
                truncated,
            });
        }
        Ok(JournalLogPage {
            revision: self.revision,
            anchor_line_id: self.anchor_line_id,
            rows,
            total_line_count: self.total_line_count,
            has_before: self.has_before,
            has_after: self.has_after,
        })
    }

    fn read_styled(self) -> Result<JournalStyledLogPage, JournalError> {
        if self.lines.is_empty() {
            return Ok(JournalStyledLogPage {
                revision: self.revision,
                anchor_line_id: self.anchor_line_id,
                rows: Vec::new(),
                total_line_count: self.total_line_count,
                has_before: self.has_before,
                has_after: self.has_after,
            });
        }
        let checkpoint_start = self
            .records
            .first()
            .map_or(self.lines[0].1.start, |record| record.logical_start);
        let mut raw_budget = MAX_LOG_PAGE_BYTES;
        let selected_lines: Vec<_> = self
            .lines
            .iter()
            .map(|(line_id, line)| {
                let available = usize::try_from(line.len()).unwrap_or(usize::MAX);
                let wanted = available.min(raw_budget);
                raw_budget = raw_budget.saturating_sub(wanted);
                (
                    *line_id,
                    *line,
                    line.start.saturating_add(wanted as u64),
                    wanted < available,
                )
            })
            .collect();
        let raw_end = selected_lines
            .last()
            .map_or(checkpoint_start, |(_, _, end, _)| *end);
        let mut files = BTreeMap::new();
        let raw = read_logical_range(
            &mut files,
            &self.segment_paths,
            &self.records,
            checkpoint_start..raw_end,
        )?;
        let mut ansi = self
            .records
            .first()
            .map_or_else(AnsiState::default, |record| record.ansi_state);
        let mut logical_cursor = checkpoint_start;
        let mut remaining_text_bytes = MAX_LOG_PAGE_BYTES;
        let mut remaining_style_spans = MAX_LOG_PAGE_STYLE_SPANS;
        let mut rows = Vec::with_capacity(selected_lines.len());
        for (line_id, line, line_end, raw_truncated) in selected_lines {
            ansi.advance(logical_slice(
                &raw,
                checkpoint_start,
                logical_cursor..line.start,
            )?);
            let StyledLogText {
                text,
                mut style_spans,
                truncated: text_truncated,
            } = ansi.decode_text(
                logical_slice(&raw, checkpoint_start, line.start..line_end)?,
                remaining_text_bytes,
            );
            remaining_text_bytes = remaining_text_bytes.saturating_sub(text.len());
            if style_spans.len() > remaining_style_spans {
                style_spans.truncate(remaining_style_spans);
            }
            remaining_style_spans = remaining_style_spans.saturating_sub(style_spans.len());
            rows.push(StyledIndexedLogLine {
                line_id,
                text,
                style_spans,
                truncated: raw_truncated || text_truncated,
            });
            logical_cursor = line_end;
        }
        Ok(JournalStyledLogPage {
            revision: self.revision,
            anchor_line_id: self.anchor_line_id,
            rows,
            total_line_count: self.total_line_count,
            has_before: self.has_before,
            has_after: self.has_after,
        })
    }
}

fn logical_slice(
    bytes: &[u8],
    logical_base: u64,
    range: Range<u64>,
) -> Result<&[u8], JournalError> {
    let start = usize::try_from(range.start.saturating_sub(logical_base)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "journal range start overflow",
        )
    })?;
    let end = usize::try_from(range.end.saturating_sub(logical_base)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "journal range end overflow",
        )
    })?;
    bytes.get(start..end).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "journal range is outside the loaded page",
        )
        .into()
    })
}

fn read_logical_range(
    files: &mut BTreeMap<u32, File>,
    segment_paths: &BTreeMap<u32, PathBuf>,
    records: &[RecordLocation],
    range: Range<u64>,
) -> Result<Vec<u8>, JournalError> {
    let capacity = usize::try_from(range.end.saturating_sub(range.start))
        .unwrap_or(MAX_LOG_PAGE_BYTES)
        .min(MAX_LOG_PAGE_BYTES);
    let mut bytes = Vec::with_capacity(capacity);
    for record in records {
        let start = range.start.max(record.logical_start);
        let end = range.end.min(record.logical_end());
        if start >= end {
            continue;
        }
        let file_offset = record
            .payload_file_offset
            .saturating_add(start.saturating_sub(record.logical_start));
        let length = usize::try_from(end - start).unwrap_or(0);
        if let std::collections::btree_map::Entry::Vacant(entry) = files.entry(record.segment_id) {
            let path = segment_paths.get(&record.segment_id).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("journal segment {} is not registered", record.segment_id),
                )
            })?;
            entry.insert(File::open(path)?);
        }
        let file = files.get_mut(&record.segment_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("journal segment {} could not be opened", record.segment_id),
            )
        })?;
        file.seek(SeekFrom::Start(file_offset))?;
        let previous_len = bytes.len();
        bytes.resize(previous_len.saturating_add(length), 0);
        file.read_exact(&mut bytes[previous_len..])?;
    }
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        BlockedLineIndex, LINE_BLOCK_HEADER_BYTES, MAX_LOG_PAGE_BYTES, MAX_LOG_PAGE_LINES,
        line_block_store_path,
    };
    use crate::{
        DEFAULT_SEGMENT_BYTES, Durability, JournalError, JournalWriter, SegmentedJournalWriter,
        scan,
    };
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    #[test]
    fn incremental_index_reads_lines_across_record_boundaries_without_holding_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("indexed.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        let index = writer.line_index();
        writer.append(b"first\r\nsec").unwrap();
        writer.append(b"ond\n\nlast").unwrap();

        assert_eq!(index.total_line_count(), 4);
        let page = index.read_page(Some(2), 1, 2).unwrap();
        assert_eq!(page.anchor_line_id, 2);
        assert_eq!(page.total_line_count, 4);
        assert_eq!(
            page.rows
                .iter()
                .map(|row| (row.line_id, row.bytes.as_slice(), row.truncated))
                .collect::<Vec<_>>(),
            vec![
                (1, b"first".as_slice(), false),
                (2, b"second".as_slice(), false),
                (3, b"".as_slice(), false),
                (4, b"last".as_slice(), false),
            ]
        );
        assert!(!page.has_before);
        assert!(!page.has_after);
    }

    #[test]
    fn reopened_writer_rebuilds_the_index_and_keeps_line_ids_stable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reopened.csjr");
        {
            let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
            writer.append(b"one\ntwo").unwrap();
        }
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        let index = writer.line_index();
        assert_eq!(index.total_line_count(), 2);
        writer.append(b" continued\nthree").unwrap();
        let page = index.read_page(None, 2, 0).unwrap();
        assert_eq!(page.anchor_line_id, 3);
        assert_eq!(page.rows[1].bytes, b"two continued");
        assert_eq!(page.rows[2].bytes, b"three");
    }

    #[test]
    fn page_limits_rows_and_truncates_an_oversized_anchor_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bounded.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        for _ in 0..MAX_LOG_PAGE_LINES + 10 {
            writer.append(b"x\n").unwrap();
        }
        let page = writer
            .line_index()
            .read_page(None, MAX_LOG_PAGE_LINES * 2, 0)
            .unwrap();
        assert_eq!(page.rows.len(), MAX_LOG_PAGE_LINES);
        assert!(page.has_before);

        let huge_path = directory.path().join("huge.csjr");
        let mut huge = JournalWriter::create(&huge_path, Durability::Ephemeral).unwrap();
        huge.append(&vec![b'z'; MAX_LOG_PAGE_BYTES + 17]).unwrap();
        let page = huge.line_index().read_page(None, 0, 0).unwrap();
        assert_eq!(page.rows[0].bytes.len(), MAX_LOG_PAGE_BYTES);
        assert!(page.rows[0].truncated);
    }

    #[test]
    fn blocked_line_directory_uses_about_four_bytes_per_short_line() {
        const LINE_COUNT: usize = 250_000;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("many-lines.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        writer.append(&b"x\n".repeat(LINE_COUNT)).unwrap();
        let index = writer.line_index();

        assert_eq!(index.total_line_count(), LINE_COUNT as u64);
        let resident_before_reads = index.resident_line_index_bytes();
        assert!(resident_before_reads < 64 * 1024);
        assert!(line_block_store_path(&path).exists());
        let page = index
            .read_page(Some((LINE_COUNT / 2) as u64), 1, 1)
            .unwrap();
        assert_eq!(page.rows.len(), 3);
        assert!(page.rows.iter().all(|row| row.bytes == b"x"));
        index.read_page(Some(2), 1, 1).unwrap();
        index.read_page(Some(LINE_COUNT as u64), 1, 0).unwrap();
        assert_eq!(index.resident_line_index_bytes(), resident_before_reads);
    }

    #[test]
    fn corrupt_cold_line_block_is_reported_without_falling_back_to_bad_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("corrupt-index.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        writer.append(&b"x\n".repeat(4096)).unwrap();
        writer.flush().unwrap();

        let mut store = OpenOptions::new()
            .read(true)
            .write(true)
            .open(line_block_store_path(&path))
            .unwrap();
        store
            .seek(SeekFrom::Start(LINE_BLOCK_HEADER_BYTES as u64))
            .unwrap();
        store.write_all(&[0xff]).unwrap();
        store.flush().unwrap();

        assert!(matches!(
            writer.line_index().read_page(Some(1), 0, 0),
            Err(JournalError::CorruptLineBlock { block_id: 0, .. })
        ));
    }

    #[test]
    fn partial_cold_block_write_keeps_committed_journal_recoverable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("full-cold-store.csjr");
        let mut writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, DEFAULT_SEGMENT_BYTES)
                .unwrap();
        let index = writer.line_index();
        writer.append(&b"old\n".repeat(4096)).unwrap();
        let committed_store_bytes = std::fs::metadata(line_block_store_path(&path))
            .unwrap()
            .len();
        index.fail_line_store_after(LINE_BLOCK_HEADER_BYTES + 17);

        assert!(matches!(
            writer.append(&b"new\n".repeat(4096)),
            Err(JournalError::PostCommitIndex { sequence: 1, source })
                if matches!(*source, JournalError::Io(ref error)
                    if error.kind() == std::io::ErrorKind::StorageFull)
        ));
        assert_eq!(index.total_line_count(), 8192);
        assert_eq!(
            std::fs::metadata(line_block_store_path(&path))
                .unwrap()
                .len(),
            committed_store_bytes
        );
        let boundary = index.read_page(Some(4096), 0, 1).unwrap();
        assert_eq!(boundary.rows[0].bytes, b"old");
        assert_eq!(boundary.rows[1].bytes, b"new");
        assert_eq!(writer.append(b"tail").unwrap(), 2);
        drop(writer);

        let report = scan(&path, false).unwrap();
        assert_eq!(report.records.len(), 3);
        assert_eq!(
            report
                .records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, DEFAULT_SEGMENT_BYTES)
                .unwrap();
        assert_eq!(writer.line_index().total_line_count(), 8193);
        assert_eq!(
            writer
                .line_index()
                .read_page(Some(8193), 0, 0)
                .unwrap()
                .rows[0]
                .bytes,
            b"tail"
        );
    }

    #[test]
    fn stalled_cold_read_does_not_hold_the_index_lock_or_block_append() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("slow-cold-read.csjr");
        let mut writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, DEFAULT_SEGMENT_BYTES)
                .unwrap();
        writer.append(&b"cold\n".repeat(4096)).unwrap();
        let index = writer.line_index();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        assert!(index.gate_cold_block_read(0, entered.clone(), release.clone()));

        let read_index = index.clone();
        let reader = std::thread::spawn(move || read_index.read_page(Some(1), 0, 0));
        entered.wait();

        let (send, receive) = mpsc::channel();
        let appender = std::thread::spawn(move || {
            send.send(writer.append(b"tail")).unwrap();
        });
        let append_while_read_is_stalled = receive.recv_timeout(Duration::from_secs(2));
        release.wait();
        let page = reader.join().unwrap().unwrap();
        appender.join().unwrap();

        assert_eq!(page.rows[0].bytes, b"cold");
        let append_result = match append_while_read_is_stalled {
            Ok(result) => result,
            Err(error) => panic!("append was blocked by cold-index disk I/O: {error}"),
        };
        assert_eq!(append_result.unwrap(), 1);
    }

    #[test]
    fn oversized_logical_block_upgrades_without_losing_offsets() {
        let mut index = BlockedLineIndex::default();
        let end = u64::from(u32::MAX) + 17;
        index.push(0, end).unwrap();
        assert_eq!(
            index.line(0, &Default::default()),
            Some(super::LineLocation { start: 0, end })
        );
    }
}
