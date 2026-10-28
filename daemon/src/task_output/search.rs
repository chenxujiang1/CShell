//! Overlapping literal matches with a complete-stream digest on every result page.
use super::analysis::storage;
use super::*;
use cshell_application::{
    OutputAnalysisError, OutputSearchRequest, OutputSearchResult, OutputStreamDigest,
};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
impl TaskOutputStore {
    pub(crate) fn search_stored_output(
        &self,
        source: (TaskRunId, HostId, usize),
        request: &OutputSearchRequest,
        cancel: &AtomicBool,
        progress: &AnalysisProgress,
    ) -> Result<OutputSearchResult, OutputAnalysisError> {
        request
            .validate()
            .map_err(|_| OutputAnalysisError::Invalid)?;
        let started = Instant::now();
        let check = || {
            if cancel.load(Ordering::Acquire) {
                Err(OutputAnalysisError::Worker)
            } else if started.elapsed() > Duration::from_secs(300) {
                Err(OutputAnalysisError::Timeout)
            } else {
                Ok(())
            }
        };
        check()?;
        let mut output = self.pin_analysis(source.0, source.1, source.2, request.scope.stderr)?;
        let length = output.metadata.len();
        if request.start > length {
            return Err(OutputAnalysisError::Invalid);
        }
        // Prefix failure links keep matching linear even for repetitive data and patterns.
        let mut failure = vec![0_usize; request.pattern.len()];
        let mut matched = 0;
        for index in 1..request.pattern.len() {
            while matched > 0 && request.pattern[index] != request.pattern[matched] {
                matched = failure[matched - 1];
            }
            if request.pattern[index] == request.pattern[matched] {
                matched += 1;
            }
            failure[index] = matched;
        }
        matched = 0;
        let mut matches = Vec::with_capacity(request.maximum_matches);
        let mut more = false;
        let mut searched_through = length;
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; MAX_TASK_OUTPUT_PAGE];
        let mut offset = 0_u64;
        while offset < length {
            check()?;
            let count = MAX_TASK_OUTPUT_PAGE.min((length - offset) as usize);
            output
                .file
                .read_exact(&mut buffer[..count])
                .map_err(storage)?;
            hash.update(&buffer[..count]);
            if !more {
                for (index, byte) in buffer[..count].iter().copied().enumerate() {
                    let position = offset + index as u64;
                    if position < request.start {
                        continue;
                    }
                    while matched > 0 && byte != request.pattern[matched] {
                        matched = failure[matched - 1];
                    }
                    if byte == request.pattern[matched] {
                        matched += 1;
                    }
                    if matched == request.pattern.len() {
                        if matches.len() < request.maximum_matches {
                            matches.push(position + 1 - request.pattern.len() as u64);
                        } else {
                            more = true;
                            searched_through = position + 1;
                            break;
                        }
                        matched = failure[matched - 1];
                    }
                }
            }
            offset += count as u64;
            progress.left.store(offset, Ordering::Release);
        }
        check()?;
        output.check_after(self.limit)?;
        let sha256 = hash.finalize().into();
        if request
            .expected_stream_sha256
            .is_some_and(|expected| expected != sha256)
        {
            return Err(OutputAnalysisError::Changed);
        }
        let next = if more {
            matches.last().map(|offset| offset + 1)
        } else {
            None
        };
        let result = OutputSearchResult {
            stream: OutputStreamDigest {
                bytes: length,
                sha256,
            },
            searched_through,
            matches,
            next,
        };
        result
            .validate(
                &request
                    .descriptor()
                    .map_err(|_| OutputAnalysisError::Invalid)?,
            )
            .map_err(|_| OutputAnalysisError::Worker)?;
        Ok(result)
    }
}
