//! Bounded per-command recording journal; raw stream files remain separate.
use super::*;
use cshell_application::{MAX_OUTPUT_MERGE_EVENTS, SystemTaskClock, TaskClock};
use sha2::{Digest, Sha256};
pub(super) const RECORD_BYTES: usize = 72;
pub(super) const MAX_JOURNAL_BYTES: u64 = (MAX_OUTPUT_MERGE_EVENTS + 2) * RECORD_BYTES as u64;
pub(super) const MAGIC: &[u8; 8] = b"CSHTEV01";
pub(super) fn number(
    bytes: &[u8],
    offset: usize,
) -> Result<u64, cshell_application::OutputAnalysisError> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(cshell_application::OutputAnalysisError::Invalid)?
            .try_into()
            .map_err(|_| cshell_application::OutputAnalysisError::Invalid)?,
    ))
}
pub(super) struct TaskEventWriter {
    file: tokio::fs::File,
    hash: Sha256,
    clock: SystemTaskClock,
    events: u64,
    indexed: [u64; 2],
    truncated: bool,
    sealed: bool,
}
impl TaskEventWriter {
    pub(super) async fn new(
        file: tokio::fs::File,
        run: TaskRunId,
        host: HostId,
        step: usize,
    ) -> Result<Self, TaskOutputError> {
        let clock = SystemTaskClock::new().map_err(|_| TaskOutputError::Unavailable)?;
        let mut header = [0_u8; RECORD_BYTES];
        header[..8].copy_from_slice(MAGIC);
        header[8..24].copy_from_slice(run.as_uuid().as_bytes());
        header[24..40].copy_from_slice(host.as_uuid().as_bytes());
        header[40..44].copy_from_slice(&(step as u32).to_le_bytes());
        header[44..52].copy_from_slice(
            &clock
                .now_ms()
                .map_err(|_| TaskOutputError::Unavailable)?
                .to_le_bytes(),
        );
        let mut writer = Self {
            file,
            hash: Sha256::new(),
            clock,
            events: 0,
            indexed: [0, 0],
            truncated: false,
            sealed: false,
        };
        writer
            .file
            .write_all(&header)
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        writer.hash.update(header);
        Ok(writer)
    }
    pub(super) fn now(&self) -> Result<u64, TaskOutputError> {
        self.clock
            .now_ms()
            .map_err(|_| TaskOutputError::Unavailable)
    }
    pub(super) fn is_sealed(&self) -> bool {
        self.sealed
    }
    pub(super) async fn append(
        &mut self,
        stderr: bool,
        offset: u64,
        data: &[u8],
        time: u64,
    ) -> Result<(), TaskOutputError> {
        if self.sealed {
            return Err(TaskOutputError::Invalid);
        }
        if data.is_empty() {
            return Ok(());
        }
        if self.events == MAX_OUTPUT_MERGE_EVENTS {
            self.truncated = true;
            return Ok(());
        }
        if self.indexed[usize::from(stderr)] != offset || data.len() > MAX_TASK_OUTPUT_PAGE {
            return Err(TaskOutputError::Invalid);
        }
        let mut record = [0_u8; RECORD_BYTES];
        record[0] = if stderr { 2 } else { 1 };
        record[8..16].copy_from_slice(&(self.events + 1).to_le_bytes());
        record[16..24].copy_from_slice(&time.to_le_bytes());
        record[24..32].copy_from_slice(&offset.to_le_bytes());
        record[32..40].copy_from_slice(&(data.len() as u64).to_le_bytes());
        record[40..72].copy_from_slice(&Sha256::digest(data));
        self.file
            .write_all(&record)
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.hash.update(record);
        self.events += 1;
        self.indexed[usize::from(stderr)] += data.len() as u64;
        Ok(())
    }
    /// Called only after both raw streams are synced. Exhaustion seals an explicit prefix.
    pub(super) async fn finish(&mut self, streams: [u64; 2]) -> Result<(), TaskOutputError> {
        if self.sealed {
            return Err(TaskOutputError::Invalid);
        }
        let mut record = [0_u8; RECORD_BYTES];
        record[0] = 3;
        record[1] = u8::from(self.truncated);
        record[8..16].copy_from_slice(&self.events.to_le_bytes());
        record[16..24].copy_from_slice(&self.now()?.to_le_bytes());
        record[24..32].copy_from_slice(&streams[0].to_le_bytes());
        record[32..40].copy_from_slice(&streams[1].to_le_bytes());
        record[40..72].copy_from_slice(&self.hash.clone().finalize());
        self.file
            .write_all(&record)
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.file
            .flush()
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.file
            .sync_all()
            .await
            .map_err(|_| TaskOutputError::Unavailable)?;
        self.sealed = true;
        Ok(())
    }
}
