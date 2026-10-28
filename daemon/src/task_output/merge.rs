use super::*;
use super::{
    analysis::storage,
    events::{MAGIC, MAX_JOURNAL_BYTES, RECORD_BYTES, number},
};
use cshell_application::*;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read, Seek, SeekFrom};
impl TaskOutputStore {
    pub(crate) fn merged_page(
        &self,
        request: OutputMergeRequest,
        binding: OutputAnalysisBinding,
    ) -> Result<(OutputMergePage, OutputMergedBody), OutputAnalysisError> {
        request
            .validate()
            .map_err(|_| OutputAnalysisError::Invalid)?;
        let source = (
            binding.child_id,
            request.scope.left,
            binding.child_step as usize,
        );
        let mut journal = self.pin_merge_journal(source.0, source.1, source.2)?;
        let mut stdout = self.pin_analysis(source.0, source.1, source.2, false)?;
        let mut stderr = self.pin_analysis(source.0, source.1, source.2, true)?;
        let length = journal.metadata.len();
        if length < RECORD_BYTES as u64 {
            return Err(OutputAnalysisError::Invalid);
        }
        let full = length / RECORD_BYTES as u64;
        let tail = (length % RECORD_BYTES as u64) as usize;
        let mut reader = BufReader::with_capacity(MAX_TASK_OUTPUT_PAGE, &mut journal.file);
        let started = std::time::Instant::now();
        let mut row = [0_u8; RECORD_BYTES];
        reader.read_exact(&mut row).map_err(storage)?;
        if &row[..8] != MAGIC
            || &row[8..24] != source.0.as_uuid().as_bytes().as_slice()
            || &row[24..40] != source.1.as_uuid().as_bytes().as_slice()
            || u32::from_le_bytes(
                row[40..44]
                    .try_into()
                    .map_err(|_| OutputAnalysisError::Invalid)?,
            ) != source.2 as u32
            || row[52..].iter().any(|byte| *byte != 0)
        {
            return Err(OutputAnalysisError::Invalid);
        }
        let created = number(&row, 44)?;
        let mut time = created;
        let mut hash = Sha256::new();
        hash.update(row);
        let streams = [stdout.metadata.len(), stderr.metadata.len()];
        let mut indexed = [0_u64; 2];
        let mut count = 0_u64;
        let mut sealed = false;
        let mut truncated = false;
        let mut events = Vec::new();
        let mut bytes = 0_usize;
        let mut selecting = true;
        for index in 1..full {
            if index % 1024 == 0 && started.elapsed() > std::time::Duration::from_secs(5) {
                return Err(OutputAnalysisError::Timeout);
            }
            reader.read_exact(&mut row).map_err(storage)?;
            let sequence = number(&row, 8)?;
            let recorded = number(&row, 16)?;
            let offset = number(&row, 24)?;
            let size = number(&row, 32)?;
            if recorded < time || recorded > i64::MAX as u64 {
                return Err(OutputAnalysisError::Invalid);
            }
            time = recorded;
            match row[0] {
                1 | 2 => {
                    let stream = usize::from(row[0] == 2);
                    if row[1..8].iter().any(|byte| *byte != 0)
                        || sequence != count + 1
                        || sequence > MAX_OUTPUT_MERGE_EVENTS
                        || size == 0
                        || size > MAX_TASK_OUTPUT_PAGE as u64
                        || offset != indexed[stream]
                        || offset
                            .checked_add(size)
                            .is_none_or(|end| end > streams[stream])
                    {
                        return Err(OutputAnalysisError::Invalid);
                    }
                    count += 1;
                    indexed[stream] += size;
                    if selecting && sequence > request.after {
                        if events.len() == request.limit
                            || bytes + size as usize > MAX_OUTPUT_MERGE_BYTES
                        {
                            selecting = false;
                        } else {
                            events.push(OutputMergeEvent {
                                sequence,
                                recorded_at_ms: recorded,
                                stderr: stream == 1,
                                stream_offset: offset,
                                bytes: size as u32,
                                sha256: row[40..72]
                                    .try_into()
                                    .map_err(|_| OutputAnalysisError::Invalid)?,
                                page_offset: bytes,
                            });
                            bytes += size as usize;
                        }
                    }
                }
                3 => {
                    let prefix: [u8; 32] = hash.clone().finalize().into();
                    if index != full - 1
                        || tail != 0
                        || row[1] > 1
                        || row[2..8].iter().any(|byte| *byte != 0)
                        || sequence != count
                        || offset != streams[0]
                        || size != streams[1]
                        || &row[40..72] != prefix.as_slice()
                    {
                        return Err(OutputAnalysisError::Invalid);
                    }
                    sealed = true;
                    truncated = row[1] == 1;
                }
                _ => return Err(OutputAnalysisError::Invalid),
            }
            hash.update(row);
        }
        if tail != 0 {
            let mut remainder = [0_u8; RECORD_BYTES];
            reader.read_exact(&mut remainder[..tail]).map_err(storage)?;
            hash.update(&remainder[..tail]);
        }
        let journal_sha256 = hash.finalize().into();
        if request
            .expected_journal_sha256
            .is_some_and(|expected| expected != journal_sha256)
        {
            return Err(OutputAnalysisError::Changed);
        }
        let next = events
            .last()
            .and_then(|event| (event.sequence < count).then_some(event.sequence));
        let page = OutputMergePage {
            format: "cshell.output-merge".into(),
            version: 1,
            request,
            binding,
            journal_sha256,
            created_at_ms: created,
            total_events: count,
            sealed,
            truncated,
            stdout_bytes: streams[0],
            stderr_bytes: streams[1],
            indexed_stdout_bytes: indexed[0],
            indexed_stderr_bytes: indexed[1],
            partial_tail_bytes: tail,
            next,
            events,
            body_bytes: bytes,
        };
        page.validate_metadata()
            .map_err(|_| OutputAnalysisError::Invalid)?;
        let mut body = OutputMergedBody(vec![0; bytes]);
        for event in &page.events {
            let file = if event.stderr {
                &mut stderr.file
            } else {
                &mut stdout.file
            };
            file.seek(SeekFrom::Start(event.stream_offset))
                .map_err(storage)?;
            file.read_exact(
                &mut body.0[event.page_offset..event.page_offset + event.bytes as usize],
            )
            .map_err(storage)?;
        }
        drop(reader);
        journal.check_after(MAX_JOURNAL_BYTES)?;
        stdout.check_after(self.limit)?;
        stderr.check_after(self.limit)?;
        page.validate(&body.0)
            .map_err(|_| OutputAnalysisError::Changed)?;
        Ok((page, body))
    }
}
