use crate::JournalError;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Instant,
};
pub const MAX_RAW_SLICE: usize = 64 * 1024;
#[derive(Clone, Debug)]
pub struct JournalRawSlice {
    pub sequence: u64,
    pub offset: u64,
    pub record_bytes: u64,
    pub total_records: u64,
    pub data: Vec<u8>,
    pub timing: Option<JournalTiming>,
}
#[derive(Clone, Copy, Debug)]
pub struct JournalTiming {
    pub elapsed_us: u64,
    pub rows: u16,
    pub cols: u16,
}
pub(super) fn timing_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".cstime");
    name.into()
}
fn options() -> OpenOptions {
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.custom_flags(0x0020_0000);
    }
    o
}
fn regular(meta: &std::fs::Metadata) -> bool {
    if !meta.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}
pub(super) fn open_regular(path: &Path) -> std::io::Result<File> {
    let file = options().read(true).open(path)?;
    if !regular(&file.metadata()?) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "journal input is not a regular file",
        ));
    }
    Ok(file)
}
fn position(sequence: u64) -> Option<u64> {
    sequence.checked_mul(24)?.checked_add(8)
}
pub(super) fn read_timing(path: &Path, sequence: u64) -> Option<JournalTiming> {
    let mut file = options().read(true).open(timing_path(path)).ok()?;
    if !regular(&file.metadata().ok()?) {
        return None;
    }
    let mut header = [0u8; 8];
    file.read_exact(&mut header).ok()?;
    if &header != b"CSJT\x01\x00\x00\x00" {
        return None;
    }
    file.seek(SeekFrom::Start(position(sequence)?)).ok()?;
    let mut row = [0u8; 24];
    file.read_exact(&mut row).ok()?;
    if u64::from_le_bytes(row[0..8].try_into().ok()?) != sequence
        || crc32fast::hash(&row[..20]) != u32::from_le_bytes(row[20..24].try_into().ok()?)
    {
        return None;
    }
    let timing = JournalTiming {
        elapsed_us: u64::from_le_bytes(row[8..16].try_into().ok()?),
        rows: u16::from_le_bytes(row[16..18].try_into().ok()?),
        cols: u16::from_le_bytes(row[18..20].try_into().ok()?),
    };
    (timing.rows > 0 && timing.cols > 0).then_some(timing)
}
#[derive(Debug)]
pub struct JournalTimingWriter {
    file: File,
    started: Instant,
    base: u64,
}
impl JournalTimingWriter {
    pub fn create(path: &Path, next_sequence: u64) -> Result<Self, JournalError> {
        let sidecar = timing_path(path);
        let existed = sidecar.exists();
        let mut file = options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&sidecar)?;
        if !regular(&file.metadata()?) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid journal timing file",
            )
            .into());
        }
        if existed && file.metadata()?.len() > 0 {
            let mut header = [0u8; 8];
            file.read_exact(&mut header)?;
            if &header != b"CSJT\x01\x00\x00\x00" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid journal timing format",
                )
                .into());
            }
        } else {
            file.write_all(b"CSJT\x01\x00\x00\x00")?;
            file.sync_data()?;
        }
        let base = next_sequence
            .checked_sub(1)
            .and_then(|s| read_timing(path, s))
            .map_or(0, |t| t.elapsed_us);
        // The repaired raw journal determines truth. Discard orphan timing rows.
        file.set_len(position(next_sequence).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "timing range overflow")
        })?)?;
        Ok(Self {
            file,
            started: Instant::now(),
            base,
        })
    }
    pub fn append(&mut self, sequence: u64, rows: u16, cols: u16) -> Result<(), JournalError> {
        let micros = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut row = [0u8; 24];
        row[..8].copy_from_slice(&sequence.to_le_bytes());
        row[8..16].copy_from_slice(&self.base.saturating_add(micros).to_le_bytes());
        row[16..18].copy_from_slice(&rows.to_le_bytes());
        row[18..20].copy_from_slice(&cols.to_le_bytes());
        let crc = crc32fast::hash(&row[..20]);
        row[20..].copy_from_slice(&crc.to_le_bytes());
        self.file
            .seek(SeekFrom::Start(position(sequence).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "timing range overflow")
            })?))?;
        self.file.write_all(&row)?;
        Ok(())
    }
    pub fn flush(&self) -> Result<(), JournalError> {
        self.file.sync_data()?;
        Ok(())
    }
}
