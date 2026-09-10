use crate::{
    Durability, HEADER_SIZE, JournalError, JournalLineIndex, MAGIC, MAX_RECORD_SIZE, VERSION, scan,
    write_record_at_tail,
};
use crc32fast::hash;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// A bounded-segment journal that presents one global line and sequence space.
///
/// Segment zero uses the supplied path. Later segments insert an eight-digit
/// segment identifier before its extension, for example `output.00000001.csjr`.
#[derive(Debug)]
pub struct SegmentedJournalWriter {
    base_path: PathBuf,
    file: File,
    durability: Durability,
    max_segment_bytes: u64,
    current_segment_id: u32,
    current_segment_bytes: u64,
    next_sequence: u64,
    line_index: JournalLineIndex,
    poisoned: bool,
    #[cfg(test)]
    fail_append_after_bytes: Option<usize>,
    #[cfg(test)]
    fail_rollback: bool,
}

impl SegmentedJournalWriter {
    pub fn create(
        base_path: impl AsRef<Path>,
        durability: Durability,
        max_segment_bytes: u64,
    ) -> Result<Self, JournalError> {
        let minimum = HEADER_SIZE as u64 + 1;
        if max_segment_bytes < minimum {
            return Err(JournalError::InvalidSegmentSize {
                actual: max_segment_bytes,
                minimum,
            });
        }

        let base_path = base_path.as_ref().to_owned();
        let mut paths = vec![base_path.clone()];
        let mut segment_id = 1_u32;
        loop {
            let path = segment_path(&base_path, segment_id);
            if !path.exists() {
                break;
            }
            paths.push(path);
            segment_id = segment_id
                .checked_add(1)
                .ok_or(JournalError::TooManySegments)?;
        }

        let numbered_paths: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                u32::try_from(index)
                    .map(|segment_id| (segment_id, path.clone()))
                    .map_err(|_| JournalError::TooManySegments)
            })
            .collect::<Result<_, _>>()?;
        let last_segment_index = paths.len().saturating_sub(1);
        let current_segment_id =
            u32::try_from(last_segment_index).map_err(|_| JournalError::TooManySegments)?;
        let current_path = paths.last().cloned().unwrap_or_else(|| base_path.clone());
        let mut cached_index = None;
        for cache_path in index_cache_candidates(&base_path) {
            if let Some(line_index) =
                JournalLineIndex::load_segmented_cache(&cache_path, &numbered_paths)?
            {
                cached_index = Some(line_index);
                break;
            }
        }
        if let Some(line_index) = cached_index {
            let current_segment_bytes = current_path.metadata()?.len();
            let next_sequence = line_index.next_sequence();
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(current_path)?;
            file.seek(SeekFrom::Start(current_segment_bytes))?;
            return Ok(Self {
                base_path,
                file,
                durability,
                max_segment_bytes,
                current_segment_id,
                current_segment_bytes,
                next_sequence,
                line_index,
                poisoned: false,
                #[cfg(test)]
                fail_append_after_bytes: None,
                #[cfg(test)]
                fail_rollback: false,
            });
        }

        let line_index = JournalLineIndex::empty_segmented(base_path.clone());
        let mut next_sequence = 0_u64;
        let mut current_segment_bytes = 0_u64;
        for (index, path) in paths.iter().enumerate() {
            let segment_id = u32::try_from(index).map_err(|_| JournalError::TooManySegments)?;
            line_index.register_segment(segment_id, path.clone());
            let is_last = index == last_segment_index;
            let report = scan(path, is_last)?;
            if !is_last && let Some(detail) = report.integrity_error {
                return Err(JournalError::CorruptSegment { segment_id, detail });
            }
            let mut file_offset = 0_u64;
            for record in report.records {
                if record.sequence != next_sequence {
                    return Err(JournalError::SequenceGap {
                        segment_id,
                        expected: next_sequence,
                        actual: record.sequence,
                    });
                }
                line_index.append_segment_record(
                    segment_id,
                    file_offset,
                    record.sequence,
                    &record.payload,
                )?;
                file_offset = file_offset
                    .saturating_add(HEADER_SIZE as u64)
                    .saturating_add(record.payload.len() as u64);
                next_sequence = next_sequence.saturating_add(1);
            }
            if is_last {
                current_segment_bytes = report.valid_bytes;
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(current_path)?;
        file.seek(SeekFrom::Start(current_segment_bytes))?;
        Ok(Self {
            base_path,
            file,
            durability,
            max_segment_bytes,
            current_segment_id,
            current_segment_bytes,
            next_sequence,
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
        let record_bytes = (HEADER_SIZE as u64).saturating_add(payload.len() as u64);
        if self.current_segment_bytes > 0
            && self.current_segment_bytes.saturating_add(record_bytes) > self.max_segment_bytes
        {
            self.rotate()?;
        }

        let sequence = self.next_sequence;
        let header = record_header(sequence, payload);
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
            self.current_segment_bytes,
            failure,
            fail_rollback,
        ) {
            if matches!(error, JournalError::JournalTailRollback { .. }) {
                self.poisoned = true;
            }
            return Err(error);
        }
        let index_result = self.line_index.append_segment_record(
            self.current_segment_id,
            self.current_segment_bytes,
            sequence,
            payload,
        );
        self.current_segment_bytes = self.current_segment_bytes.saturating_add(record_bytes);
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
        flush_file(&mut self.file, self.durability)?;
        self.line_index.flush_line_blocks(self.durability)?;
        let cache_path = index_cache_path(&self.base_path);
        let temporary_path = companion_path(&cache_path, ".tmp");
        self.line_index.write_segmented_cache(&temporary_path)?;
        sync_path(&temporary_path, self.durability)?;
        replace_index_cache(&temporary_path, &cache_path)?;
        sync_path(&cache_path, self.durability)?;
        sync_parent_directory(&cache_path, self.durability)
    }

    #[must_use]
    pub fn line_index(&self) -> JournalLineIndex {
        self.line_index.clone()
    }

    #[must_use]
    pub fn current_segment_id(&self) -> u32 {
        self.current_segment_id
    }

    #[must_use]
    pub fn segment_count(&self) -> u64 {
        u64::from(self.current_segment_id) + 1
    }

    #[must_use]
    pub fn current_path(&self) -> PathBuf {
        segment_path(&self.base_path, self.current_segment_id)
    }

    fn rotate(&mut self) -> Result<(), JournalError> {
        flush_file(&mut self.file, self.durability)?;
        let next_segment_id = self
            .current_segment_id
            .checked_add(1)
            .ok_or(JournalError::TooManySegments)?;
        let path = segment_path(&self.base_path, next_segment_id);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        self.line_index.register_segment(next_segment_id, path);
        self.file = file;
        self.current_segment_id = next_segment_id;
        self.current_segment_bytes = 0;
        Ok(())
    }
}

fn flush_file(file: &mut File, durability: Durability) -> Result<(), JournalError> {
    file.flush()?;
    match durability {
        Durability::Ephemeral => Ok(()),
        Durability::SessionLog => file.sync_data().map_err(Into::into),
        Durability::Audit => file.sync_all().map_err(Into::into),
    }
}

fn record_header(sequence: u64, payload: &[u8]) -> [u8; HEADER_SIZE] {
    let mut header = [0_u8; HEADER_SIZE];
    header[0..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&0_u16.to_le_bytes());
    header[8..16].copy_from_slice(&sequence.to_le_bytes());
    header[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[20..24].copy_from_slice(&hash(payload).to_le_bytes());
    header
}

fn segment_path(base_path: &Path, segment_id: u32) -> PathBuf {
    if segment_id == 0 {
        return base_path.to_owned();
    }
    let mut name = base_path
        .file_stem()
        .or_else(|| base_path.file_name())
        .map_or_else(|| OsString::from("journal"), OsString::from);
    name.push(format!(".{segment_id:08}"));
    if let Some(extension) = base_path.extension() {
        name.push(".");
        name.push(extension);
    }
    base_path.with_file_name(name)
}

fn index_cache_path(base_path: &Path) -> PathBuf {
    let mut name = base_path
        .file_name()
        .map_or_else(|| OsString::from("journal"), OsString::from);
    name.push(".csix");
    base_path.with_file_name(name)
}

fn companion_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| OsString::from("journal"), OsString::from);
    name.push(suffix);
    path.with_file_name(name)
}

fn index_cache_candidates(base_path: &Path) -> [PathBuf; 3] {
    let primary = index_cache_path(base_path);
    [
        primary.clone(),
        companion_path(&primary, ".tmp"),
        companion_path(&primary, ".bak"),
    ]
}

fn replace_index_cache(temporary_path: &Path, cache_path: &Path) -> Result<(), JournalError> {
    let backup_path = companion_path(cache_path, ".bak");
    match std::fs::remove_file(&backup_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let had_primary = cache_path.exists();
    if had_primary {
        std::fs::rename(cache_path, &backup_path)?;
    }
    if let Err(error) = std::fs::rename(temporary_path, cache_path) {
        if had_primary {
            let _ = std::fs::rename(&backup_path, cache_path);
        }
        return Err(error.into());
    }
    if had_primary {
        match std::fs::remove_file(backup_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sync_path(path: &Path, durability: Durability) -> Result<(), JournalError> {
    match durability {
        Durability::Ephemeral => Ok(()),
        Durability::SessionLog => OpenOptions::new()
            .write(true)
            .open(path)?
            .sync_data()
            .map_err(Into::into),
        Durability::Audit => OpenOptions::new()
            .write(true)
            .open(path)?
            .sync_all()
            .map_err(Into::into),
    }
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path, durability: Durability) -> Result<(), JournalError> {
    if durability == Durability::Ephemeral {
        return Ok(());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all().map_err(Into::into)
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path, _durability: Durability) -> Result<(), JournalError> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::SegmentedJournalWriter;
    use crate::{Durability, HEADER_SIZE, JournalColor, JournalError, scan};
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn line_block_store_path(path: &std::path::Path) -> std::path::PathBuf {
        let mut name = path
            .file_name()
            .map_or_else(|| OsString::from("journal"), OsString::from);
        name.push(".csli");
        path.with_file_name(name)
    }

    const SMALL_SEGMENT: u64 = 35;

    #[test]
    fn rotates_and_reads_a_line_across_segment_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("output.csjr");
        let mut writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        writer.append(b"first ").unwrap();
        writer.append(b"line\nsecond").unwrap();
        assert_eq!(writer.segment_count(), 2);

        let page = writer.line_index().read_page(None, 1, 0).unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0].bytes, b"first line");
        assert_eq!(page.rows[1].bytes, b"second");
    }

    #[test]
    fn partial_segment_record_write_rolls_back_without_advancing_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("partial-segment.csjr");
        let mut writer = SegmentedJournalWriter::create(
            &path,
            Durability::Ephemeral,
            super::DEFAULT_SEGMENT_BYTES,
        )
        .unwrap();
        writer.append(b"old\n").unwrap();
        let committed_bytes = std::fs::metadata(writer.current_path()).unwrap().len();
        writer.fail_append_after_bytes = Some(HEADER_SIZE + 2);

        let error = writer.append(b"bad\n").unwrap_err();
        assert!(
            matches!(error, JournalError::Io(ref source)
                if source.kind() == std::io::ErrorKind::StorageFull),
            "unexpected partial-write error: {error:?}"
        );
        assert_eq!(
            std::fs::metadata(writer.current_path()).unwrap().len(),
            committed_bytes
        );
        assert_eq!(writer.line_index().total_line_count(), 1);

        writer.fail_append_after_bytes = None;
        assert_eq!(writer.append(b"new\n").unwrap(), 1);
        assert_eq!(writer.line_index().total_line_count(), 2);
    }

    #[test]
    fn reopen_preserves_global_sequence_and_line_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reopen.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"one\n").unwrap();
            writer.append(b"two").unwrap();
            writer.flush().unwrap();
        }

        let mut writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        assert_eq!(writer.append(b" continued\nthree").unwrap(), 2);
        let page = writer.line_index().read_page(None, 2, 0).unwrap();
        assert_eq!(page.rows[0].line_id, 1);
        assert_eq!(page.rows[1].bytes, b"two continued");
        assert_eq!(page.rows[2].bytes, b"three");
    }

    #[test]
    fn repairs_only_the_last_segment_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tail.csjr");
        let last_path = {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"one\n").unwrap();
            writer.append(b"two\n").unwrap();
            writer.flush().unwrap();
            writer.current_path()
        };
        OpenOptions::new()
            .append(true)
            .open(&last_path)
            .unwrap()
            .write_all(b"broken")
            .unwrap();

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        assert_eq!(writer.line_index().total_line_count(), 2);
        assert!(scan(last_path, false).unwrap().integrity_error.is_none());
    }

    #[test]
    fn rejects_corruption_in_a_sealed_segment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("corrupt.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"one\n").unwrap();
            writer.append(b"two\n").unwrap();
            writer.flush().unwrap();
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"broken")
            .unwrap();

        let error = SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
            .unwrap_err();
        assert!(matches!(
            error,
            JournalError::CorruptSegment { segment_id: 0, .. }
        ));
    }

    #[test]
    fn ignores_a_torn_index_checkpoint_and_rebuilds_from_segments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"one\ntwo").unwrap();
            writer.flush().unwrap();
        }
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(super::index_cache_path(&path))
            .unwrap()
            .write_all(b"torn")
            .unwrap();

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        let page = writer.line_index().read_page(None, 1, 0).unwrap();
        assert_eq!(page.rows[0].bytes, b"one");
        assert_eq!(page.rows[1].bytes, b"two");
    }

    #[test]
    fn stale_checkpoint_after_unflushed_append_rebuilds_all_cold_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stale-cold-checkpoint.csjr");
        {
            let mut writer = SegmentedJournalWriter::create(
                &path,
                Durability::Ephemeral,
                super::DEFAULT_SEGMENT_BYTES,
            )
            .unwrap();
            writer.append(&b"old\n".repeat(4096)).unwrap();
            writer.flush().unwrap();
            writer.append(&b"new\n".repeat(4096)).unwrap();
        }

        let writer = SegmentedJournalWriter::create(
            &path,
            Durability::Ephemeral,
            super::DEFAULT_SEGMENT_BYTES,
        )
        .unwrap();
        assert_eq!(writer.line_index().total_line_count(), 8192);
        assert_eq!(
            writer
                .line_index()
                .read_page(Some(4096), 0, 1)
                .unwrap()
                .rows
                .iter()
                .map(|row| row.bytes.as_slice())
                .collect::<Vec<_>>(),
            vec![b"old".as_slice(), b"new".as_slice()]
        );
    }

    #[test]
    fn failed_temporary_checkpoint_write_preserves_the_previous_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint-write-failure.csjr");
        let mut writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        writer.append(b"old\n").unwrap();
        writer.flush().unwrap();
        let cache_path = super::index_cache_path(&path);
        let previous_cache = std::fs::read(&cache_path).unwrap();

        writer.append(b"new\n").unwrap();
        let temporary_path = super::companion_path(&cache_path, ".tmp");
        std::fs::create_dir(&temporary_path).unwrap();
        assert!(matches!(writer.flush(), Err(JournalError::Io(_))));
        assert_eq!(std::fs::read(&cache_path).unwrap(), previous_cache);
        std::fs::remove_dir(temporary_path).unwrap();
        drop(writer);

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        assert_eq!(writer.line_index().total_line_count(), 2);
    }

    #[test]
    fn complete_temporary_checkpoint_is_a_startup_recovery_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint-temp-recovery.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"recoverable\n").unwrap();
            writer.flush().unwrap();
        }
        let cache_path = super::index_cache_path(&path);
        let temporary_path = super::companion_path(&cache_path, ".tmp");
        std::fs::rename(&cache_path, &temporary_path).unwrap();

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        assert_eq!(
            writer.line_index().read_page(Some(1), 0, 0).unwrap().rows[0].bytes,
            b"recoverable"
        );
    }

    #[test]
    fn backup_checkpoint_is_a_startup_recovery_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint-backup-recovery.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"backup\n").unwrap();
            writer.flush().unwrap();
        }
        let cache_path = super::index_cache_path(&path);
        let backup_path = super::companion_path(&cache_path, ".bak");
        std::fs::rename(&cache_path, &backup_path).unwrap();

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        assert_eq!(
            writer.line_index().read_page(Some(1), 0, 0).unwrap().rows[0].bytes,
            b"backup"
        );
    }

    #[test]
    fn persisted_topology_rejects_a_missing_middle_segment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"one\n").unwrap();
            writer.append(b"two\n").unwrap();
            writer.append(b"three\n").unwrap();
            writer.flush().unwrap();
            assert_eq!(writer.segment_count(), 3);
        }
        std::fs::remove_file(super::segment_path(&path, 1)).unwrap();

        let error = SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
            .unwrap_err();
        assert!(matches!(
            error,
            JournalError::MissingSegment { segment_id: 1 }
        ));
    }

    #[test]
    fn ansi_checkpoint_restores_style_across_segment_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("styled.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"\x1b[38;5;196mfirst\n").unwrap();
            writer.append(b"second\x1b[0m\nplain").unwrap();
            writer.flush().unwrap();
            assert_eq!(writer.segment_count(), 2);
        }

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        let page = writer.line_index().read_styled_page(Some(2), 0, 1).unwrap();
        assert_eq!(page.rows[0].text, "second");
        assert_eq!(page.rows[0].style_spans.len(), 1);
        assert_eq!(
            page.rows[0].style_spans[0].style.foreground,
            JournalColor::Indexed(196)
        );
        assert_eq!(page.rows[1].text, "plain");
        assert!(page.rows[1].style_spans.is_empty());
    }

    #[test]
    fn ansi_checkpoint_handles_a_csi_split_across_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("split-sgr.csjr");
        let mut writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        writer.append(b"\x1b[38;2;10;").unwrap();
        writer.append(b"20;30mcolored\n").unwrap();

        let page = writer.line_index().read_styled_page(None, 0, 0).unwrap();
        assert_eq!(page.rows[0].text, "colored");
        assert_eq!(
            page.rows[0].style_spans[0].style.foreground,
            JournalColor::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn ansi_checkpoint_restores_colon_true_color_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("colon-sgr.csjr");
        {
            let mut writer =
                SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT)
                    .unwrap();
            writer.append(b"\x1b[38:2::10:20:30mfirst\n").unwrap();
            writer.append(b"second\x1b[0m\n").unwrap();
            writer.flush().unwrap();
        }

        let writer =
            SegmentedJournalWriter::create(&path, Durability::Ephemeral, SMALL_SEGMENT).unwrap();
        let page = writer.line_index().read_styled_page(Some(2), 0, 0).unwrap();
        assert_eq!(page.rows[0].text, "second");
        assert_eq!(
            page.rows[0].style_spans[0].style.foreground,
            JournalColor::Rgb(10, 20, 30)
        );
    }

    #[test]
    fn checkpoint_round_trip_preserves_multiple_line_blocks() {
        const LINE_COUNT: usize = 262_144;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blocked-checkpoint.csjr");
        {
            let mut writer = SegmentedJournalWriter::create(
                &path,
                Durability::Ephemeral,
                super::DEFAULT_SEGMENT_BYTES,
            )
            .unwrap();
            writer.append(&b"x\n".repeat(LINE_COUNT)).unwrap();
            writer.flush().unwrap();
        }
        let cache_bytes = std::fs::metadata(super::index_cache_path(&path))
            .unwrap()
            .len();
        assert!(cache_bytes < 8 * 1024);
        let initial_store_bytes = std::fs::metadata(line_block_store_path(&path))
            .unwrap()
            .len();

        let mut writer = SegmentedJournalWriter::create(
            &path,
            Durability::Ephemeral,
            super::DEFAULT_SEGMENT_BYTES,
        )
        .unwrap();
        assert_eq!(writer.line_index().total_line_count(), LINE_COUNT as u64);
        let page = writer.line_index().read_page(Some(131_073), 1, 1).unwrap();
        assert_eq!(page.rows.len(), 3);
        assert!(page.rows.iter().all(|row| row.bytes == b"x"));

        writer.append(&b"y\n".repeat(4_096)).unwrap();
        assert_eq!(writer.line_index().total_line_count(), 266_240);
        writer.flush().unwrap();
        let appended_store_bytes = std::fs::metadata(line_block_store_path(&path))
            .unwrap()
            .len();
        assert!(
            appended_store_bytes > initial_store_bytes,
            "cold store did not grow: initial={initial_store_bytes}, appended={appended_store_bytes}"
        );
        drop(writer);

        let writer = SegmentedJournalWriter::create(
            &path,
            Durability::Ephemeral,
            super::DEFAULT_SEGMENT_BYTES,
        )
        .unwrap();
        assert_eq!(writer.line_index().total_line_count(), 266_240);
        assert_eq!(
            writer.line_index().read_page(Some(1), 0, 0).unwrap().rows[0].bytes,
            b"x"
        );
        assert_eq!(
            writer
                .line_index()
                .read_page(Some(266_240), 0, 0)
                .unwrap()
                .rows[0]
                .bytes,
            b"y"
        );
    }

    #[test]
    fn changed_cold_store_invalidates_checkpoint_and_rebuilds_from_journal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cold-rebuild.csjr");
        {
            let mut writer = SegmentedJournalWriter::create(
                &path,
                Durability::Ephemeral,
                super::DEFAULT_SEGMENT_BYTES,
            )
            .unwrap();
            writer.append(&b"safe\n".repeat(4096)).unwrap();
            writer.flush().unwrap();
        }
        let store_path = line_block_store_path(&path);
        let store_len = std::fs::metadata(&store_path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&store_path)
            .unwrap()
            .set_len(store_len - 1)
            .unwrap();

        let writer = SegmentedJournalWriter::create(
            &path,
            Durability::Ephemeral,
            super::DEFAULT_SEGMENT_BYTES,
        )
        .unwrap();
        assert_eq!(writer.line_index().total_line_count(), 4096);
        assert_eq!(
            writer.line_index().read_page(Some(1), 0, 0).unwrap().rows[0].bytes,
            b"safe"
        );
    }

    #[test]
    fn process_crash_helper() {
        let Some(mode) = std::env::var_os("CSHELL_CRASH_HELPER_MODE") else {
            return;
        };
        let path = std::path::PathBuf::from(std::env::var_os("CSHELL_CRASH_JOURNAL").unwrap());
        let marker = std::path::PathBuf::from(std::env::var_os("CSHELL_CRASH_MARKER").unwrap());
        let mode = mode.to_string_lossy();
        let segment_bytes = if mode == "rotated-tail" {
            SMALL_SEGMENT
        } else {
            super::DEFAULT_SEGMENT_BYTES
        };
        let mut writer =
            SegmentedJournalWriter::create(&path, Durability::SessionLog, segment_bytes).unwrap();
        match mode.as_ref() {
            "active-tail" => {
                writer.append(b"base\n").unwrap();
                writer.flush().unwrap();
                writer.append(b"tail\n").unwrap();
                std::fs::write(&marker, b"ready").unwrap();
            }
            "rotated-tail" => {
                writer.append(b"base\n").unwrap();
                writer.flush().unwrap();
                writer.append(b"tail\n").unwrap();
                assert_eq!(writer.current_segment_id(), 1);
                std::fs::write(&marker, b"ready").unwrap();
            }
            "stale-cold-checkpoint" => {
                writer.append(&b"a\n".repeat(4096)).unwrap();
                writer.flush().unwrap();
                writer.append(&b"b\n".repeat(4096)).unwrap();
                std::fs::write(&marker, b"ready").unwrap();
            }
            "torn-header" | "torn-payload" => {
                writer.append(b"base\n").unwrap();
                writer.flush().unwrap();
                let header = super::record_header(1, b"tail\n");
                let mut raw = OpenOptions::new()
                    .append(true)
                    .open(writer.current_path())
                    .unwrap();
                if mode == "torn-header" {
                    raw.write_all(&header[..12]).unwrap();
                } else {
                    raw.write_all(&header).unwrap();
                    raw.write_all(b"ta").unwrap();
                }
                raw.sync_data().unwrap();
                std::fs::write(&marker, b"ready").unwrap();
            }
            "partial-large-record" => {
                writer.append(b"base\n").unwrap();
                writer.flush().unwrap();
                std::fs::write(&marker, b"ready").unwrap();
                let mut payload = vec![b'x'; crate::MAX_RECORD_SIZE];
                if let Some(last) = payload.last_mut() {
                    *last = b'\n';
                }
                loop {
                    writer.append(&payload).unwrap();
                }
            }
            _ => panic!("unknown crash helper mode: {mode}"),
        }
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn forced_process_termination_recovers_committed_journal_matrix() {
        for mode in [
            "active-tail",
            "rotated-tail",
            "stale-cold-checkpoint",
            "torn-header",
            "torn-payload",
            "partial-large-record",
        ] {
            run_crash_case(mode);
        }
    }

    fn run_crash_case(mode: &str) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(format!("{mode}.csjr"));
        let marker = directory.path().join("ready.marker");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "segmented::tests::process_crash_helper",
                "--nocapture",
            ])
            .env("CSHELL_CRASH_HELPER_MODE", mode)
            .env("CSHELL_CRASH_JOURNAL", &path)
            .env("CSHELL_CRASH_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("crash helper exited before marker in {mode}: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "crash helper timed out in {mode}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        if mode == "partial-large-record" {
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "crash helper was not force-terminated");

        let segment_bytes = if mode == "rotated-tail" {
            SMALL_SEGMENT
        } else {
            super::DEFAULT_SEGMENT_BYTES
        };
        let mut recovered =
            SegmentedJournalWriter::create(&path, Durability::SessionLog, segment_bytes).unwrap();
        let index = recovered.line_index();
        assert!(
            index.integrity_error().is_none(),
            "integrity failure in {mode}"
        );
        match mode {
            "active-tail" | "rotated-tail" => {
                assert_eq!(index.logical_bytes(), 10);
                assert_eq!(index.record_count(), 2);
                assert_eq!(index.total_line_count(), 2);
                assert_eq!(index.read_page(Some(1), 0, 1).unwrap().rows.len(), 2);
            }
            "stale-cold-checkpoint" => {
                assert_eq!(index.logical_bytes(), 16_384);
                assert_eq!(index.record_count(), 2);
                assert_eq!(index.total_line_count(), 8192);
                assert_eq!(index.read_page(Some(1), 0, 0).unwrap().rows[0].bytes, b"a");
                assert_eq!(
                    index.read_page(Some(8192), 0, 0).unwrap().rows[0].bytes,
                    b"b"
                );
            }
            "partial-large-record" => {
                assert!(index.logical_bytes() >= 5);
                assert!(index.record_count() >= 1);
                let expected_sequence = index.record_count();
                assert_eq!(recovered.append(b"after\n").unwrap(), expected_sequence);
            }
            "torn-header" | "torn-payload" => {
                assert_eq!(index.logical_bytes(), 5);
                assert_eq!(index.record_count(), 1);
                assert_eq!(index.total_line_count(), 1);
                assert_eq!(recovered.append(b"after\n").unwrap(), 1);
            }
            _ => unreachable!(),
        }
        recovered.flush().unwrap();
    }
}
