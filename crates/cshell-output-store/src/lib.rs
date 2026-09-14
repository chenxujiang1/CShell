//! Append-only output journal with bounded records and recoverable tail corruption.

mod ansi;
mod history_search;
mod line_index;
mod segmented;

use crc32fast::hash;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub use ansi::{JournalColor, JournalStyle, JournalStyleSpan, StyledLogText};
pub use history_search::{
    HistorySearchCursor, HistorySearchDirection, HistorySearchError, HistorySearchMatch,
    HistorySearchOptions, HistorySearchRequest, HistorySearchResult, MAX_HISTORY_SEARCH_MATCHES,
    MAX_HISTORY_SEARCH_QUERY_BYTES, MAX_HISTORY_SEARCH_SCAN_LINES,
};
pub use line_index::{
    IndexedLogLine, JournalLineIndex, JournalLogPage, JournalStyledLogPage, MAX_LOG_PAGE_BYTES,
    MAX_LOG_PAGE_LINES, MAX_LOG_PAGE_STYLE_SPANS, StyledIndexedLogLine,
};
pub use segmented::{DEFAULT_SEGMENT_BYTES, SegmentedJournalWriter};

pub(crate) const MAGIC: [u8; 4] = *b"CSJR";
pub(crate) const VERSION: u16 = 1;
pub(crate) const HEADER_SIZE: usize = 24;
pub const MAX_RECORD_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Durability {
    Ephemeral,
    SessionLog,
    Audit,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("record is {actual} bytes; limit is {limit} bytes")]
    RecordTooLarge { actual: usize, limit: usize },
    #[error("log line {0} is outside the indexed journal")]
    UnknownLine(u64),
    #[error("segment size is {actual} bytes; minimum is {minimum} bytes")]
    InvalidSegmentSize { actual: u64, minimum: u64 },
    #[error("journal segment {segment_id} is corrupt: {detail}")]
    CorruptSegment { segment_id: u32, detail: String },
    #[error(
        "journal sequence discontinuity in segment {segment_id}: expected {expected}, found {actual}"
    )]
    SequenceGap {
        segment_id: u32,
        expected: u64,
        actual: u64,
    },
    #[error("journal has exhausted its segment identifier space")]
    TooManySegments,
    #[error("journal segment {segment_id} is missing from the persisted segment topology")]
    MissingSegment { segment_id: u32 },
    #[error("cold line-index block {block_id} is corrupt: {detail}")]
    CorruptLineBlock { block_id: u64, detail: String },
    #[error("journal record {sequence} was committed, but its line-index update failed: {source}")]
    PostCommitIndex {
        sequence: u64,
        #[source]
        source: Box<JournalError>,
    },
    #[error(
        "journal write failed at byte {record_start}, and its partial tail could not be removed: write={write_error}; rollback={rollback_error}"
    )]
    JournalTailRollback {
        record_start: u64,
        write_error: String,
        rollback_error: String,
    },
    #[error("journal writer is poisoned after an unrecoverable partial write")]
    JournalWriterPoisoned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRecord {
    pub sequence: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScanReport {
    pub records: Vec<JournalRecord>,
    pub valid_bytes: u64,
    pub repaired_tail: bool,
    pub integrity_error: Option<String>,
}

#[derive(Debug)]
pub struct JournalWriter {
    path: PathBuf,
    file: File,
    durability: Durability,
    next_sequence: u64,
    next_file_offset: u64,
    line_index: JournalLineIndex,
    poisoned: bool,
    #[cfg(test)]
    fail_append_after_bytes: Option<usize>,
    #[cfg(test)]
    fail_rollback: bool,
}

impl JournalWriter {
    pub fn create(path: impl AsRef<Path>, durability: Durability) -> Result<Self, JournalError> {
        let path = path.as_ref().to_owned();
        let (line_index, next_sequence, next_file_offset) = JournalLineIndex::load(&path, true)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        file.seek(SeekFrom::Start(next_file_offset))?;
        Ok(Self {
            path,
            file,
            durability,
            next_sequence,
            next_file_offset,
            line_index,
            poisoned: false,
            #[cfg(test)]
            fail_append_after_bytes: None,
            #[cfg(test)]
            fail_rollback: false,
        })
    }

    pub fn append(&mut self, payload: &[u8]) -> Result<u64, JournalError> {
        if self.poisoned {
            return Err(JournalError::JournalWriterPoisoned);
        }
        if payload.len() > MAX_RECORD_SIZE || payload.len() > u32::MAX as usize {
            return Err(JournalError::RecordTooLarge {
                actual: payload.len(),
                limit: MAX_RECORD_SIZE,
            });
        }

        let sequence = self.next_sequence;
        let mut header = [0_u8; HEADER_SIZE];
        header[0..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&VERSION.to_le_bytes());
        header[6..8].copy_from_slice(&0_u16.to_le_bytes());
        header[8..16].copy_from_slice(&sequence.to_le_bytes());
        header[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[20..24].copy_from_slice(&hash(payload).to_le_bytes());

        #[cfg(test)]
        let failure = self.fail_append_after_bytes.as_mut();
        #[cfg(not(test))]
        let failure = None;
        #[cfg(test)]
        let fail_rollback = self.fail_rollback;
        #[cfg(not(test))]
        let fail_rollback = false;
        if let Err(error) = write_record_at_tail(
            &mut self.file,
            &header,
            payload,
            self.next_file_offset,
            failure,
            fail_rollback,
        ) {
            if matches!(error, JournalError::JournalTailRollback { .. }) {
                self.poisoned = true;
            }
            return Err(error);
        }
        let index_result = self
            .line_index
            .append_record(self.next_file_offset, sequence, payload);
        self.next_file_offset = self
            .next_file_offset
            .saturating_add((HEADER_SIZE + payload.len()) as u64);
        self.next_sequence = self.next_sequence.saturating_add(1);
        if let Err(source) = index_result {
            return Err(JournalError::PostCommitIndex {
                sequence,
                source: Box::new(source),
            });
        }
        Ok(sequence)
    }

    pub fn flush(&mut self) -> Result<(), JournalError> {
        self.file.flush()?;
        match self.durability {
            Durability::Ephemeral => Ok(()),
            Durability::SessionLog => self.file.sync_data(),
            Durability::Audit => self.file.sync_all(),
        }?;
        self.line_index.flush_line_blocks(self.durability)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn line_index(&self) -> JournalLineIndex {
        self.line_index.clone()
    }

    #[cfg(test)]
    fn fail_append_after(&mut self, bytes: usize) {
        self.fail_append_after_bytes = Some(bytes);
    }

    #[cfg(test)]
    fn clear_append_failure(&mut self) {
        self.fail_append_after_bytes = None;
    }

    #[cfg(test)]
    fn fail_next_rollback(&mut self) {
        self.fail_rollback = true;
    }
}

pub(crate) fn write_record_at_tail(
    file: &mut File,
    header: &[u8],
    payload: &[u8],
    record_start: u64,
    mut failure: Option<&mut usize>,
    fail_rollback: bool,
) -> Result<(), JournalError> {
    let write_result = write_all_maybe_fault(file, header, failure.as_deref_mut())
        .and_then(|()| write_all_maybe_fault(file, payload, failure));
    if let Err(write_error) = write_result {
        let rollback_result = if fail_rollback {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected journal tail rollback failure",
            ))
        } else {
            file.set_len(record_start)
                .and_then(|()| file.seek(SeekFrom::Start(record_start)).map(|_| ()))
        };
        if let Err(rollback_error) = rollback_result {
            return Err(JournalError::JournalTailRollback {
                record_start,
                write_error: write_error.to_string(),
                rollback_error: rollback_error.to_string(),
            });
        }
        return Err(write_error.into());
    }
    Ok(())
}

fn write_all_maybe_fault(
    file: &mut File,
    bytes: &[u8],
    failure: Option<&mut usize>,
) -> Result<(), io::Error> {
    let Some(remaining) = failure else {
        return file.write_all(bytes);
    };
    let writable = bytes.len().min(*remaining);
    if writable > 0 {
        file.write_all(&bytes[..writable])?;
        *remaining -= writable;
    }
    if writable != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "injected journal storage exhaustion",
        ));
    }
    Ok(())
}

pub fn scan(path: impl AsRef<Path>, repair_tail: bool) -> Result<ScanReport, JournalError> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(ScanReport::default());
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(repair_tail)
        .open(path)?;
    let mut report = ScanReport::default();
    let mut offset = 0_u64;

    loop {
        let mut header = [0_u8; HEADER_SIZE];
        let first_read = file.read(&mut header)?;
        if first_read == 0 {
            break;
        }
        if first_read < HEADER_SIZE
            && let Err(error) = file.read_exact(&mut header[first_read..])
        {
            report.integrity_error = Some(format!("partial header at byte {offset}: {error}"));
            break;
        }
        if header[0..4] != MAGIC {
            report.integrity_error = Some(format!("invalid magic at byte {offset}"));
            break;
        }
        let version = u16::from_le_bytes([header[4], header[5]]);
        if version != VERSION {
            report.integrity_error =
                Some(format!("unsupported version {version} at byte {offset}"));
            break;
        }
        let sequence = u64::from_le_bytes(header[8..16].try_into().unwrap_or_default());
        let payload_len =
            u32::from_le_bytes(header[16..20].try_into().unwrap_or_default()) as usize;
        let expected_checksum = u32::from_le_bytes(header[20..24].try_into().unwrap_or_default());
        if payload_len > MAX_RECORD_SIZE {
            report.integrity_error = Some(format!("oversized record at byte {offset}"));
            break;
        }
        let mut payload = vec![0_u8; payload_len];
        if let Err(error) = file.read_exact(&mut payload) {
            report.integrity_error = Some(format!("partial payload at byte {offset}: {error}"));
            break;
        }
        if hash(&payload) != expected_checksum {
            report.integrity_error = Some(format!("checksum mismatch at byte {offset}"));
            break;
        }
        offset = offset.saturating_add((HEADER_SIZE + payload_len) as u64);
        report.valid_bytes = offset;
        report.records.push(JournalRecord { sequence, payload });
    }

    if repair_tail && report.integrity_error.is_some() {
        file.set_len(report.valid_bytes)?;
        report.repaired_tail = true;
    }
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Durability, HEADER_SIZE, JournalError, JournalWriter, scan};
    use std::fs::OpenOptions;
    use std::io::Write;

    #[test]
    fn round_trip_preserves_order_and_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("output.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        assert_eq!(writer.append(b"alpha").unwrap(), 0);
        assert_eq!(writer.append(b"beta\0gamma").unwrap(), 1);
        writer.flush().unwrap();

        let report = scan(&path, false).unwrap();
        assert_eq!(report.records.len(), 2);
        assert_eq!(report.records[1].payload, b"beta\0gamma");
        assert!(report.integrity_error.is_none());
    }

    #[test]
    fn corrupt_tail_is_reported_and_repaired() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("output.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        writer.append(b"valid").unwrap();
        writer.flush().unwrap();
        drop(writer);

        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"broken-tail")
            .unwrap();

        let report = scan(&path, true).unwrap();
        assert!(report.repaired_tail);
        assert!(report.integrity_error.is_some());
        assert_eq!(scan(&path, false).unwrap().records.len(), 1);
    }

    #[test]
    fn partial_record_write_rolls_back_and_can_retry_without_sequence_gap() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("partial-record.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        assert_eq!(writer.append(b"old\n").unwrap(), 0);
        let committed_bytes = std::fs::metadata(&path).unwrap().len();
        writer.fail_append_after(HEADER_SIZE + 3);

        let error = writer.append(b"new\n").unwrap_err();
        assert!(
            matches!(error, JournalError::Io(ref source)
                if source.kind() == std::io::ErrorKind::StorageFull),
            "unexpected partial-write error: {error:?}"
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), committed_bytes);
        assert_eq!(writer.line_index().total_line_count(), 1);

        writer.clear_append_failure();
        assert_eq!(writer.append(b"new\n").unwrap(), 1);
        writer.flush().unwrap();
        let report = scan(&path, false).unwrap();
        assert_eq!(
            report
                .records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(writer.line_index().total_line_count(), 2);
    }

    #[test]
    fn rollback_failure_poisons_writer_until_reopen_repairs_the_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poisoned-record.csjr");
        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        writer.append(b"old\n").unwrap();
        writer.fail_append_after(HEADER_SIZE + 1);
        writer.fail_next_rollback();

        assert!(matches!(
            writer.append(b"broken\n"),
            Err(JournalError::JournalTailRollback {
                record_start: 28,
                ..
            })
        ));
        assert!(matches!(
            writer.append(b"must-not-write\n"),
            Err(JournalError::JournalWriterPoisoned)
        ));
        drop(writer);

        let mut writer = JournalWriter::create(&path, Durability::Ephemeral).unwrap();
        assert_eq!(writer.line_index().total_line_count(), 1);
        assert_eq!(writer.append(b"recovered\n").unwrap(), 1);
        assert_eq!(writer.line_index().total_line_count(), 2);
        assert!(scan(&path, false).unwrap().integrity_error.is_none());
    }
}
