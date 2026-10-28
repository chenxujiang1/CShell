//! Literal byte search with bounded result pages and whole-stream digest binding.
use super::*;
use zeroize::Zeroize;
pub const MAX_OUTPUT_SEARCH_PATTERN: usize = 1024;
pub const MAX_OUTPUT_SEARCH_MATCHES: usize = 256;
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSearchRequest {
    pub format: String,
    pub version: u32,
    pub scope: OutputAnalysisRequest,
    pub pattern: Vec<u8>,
    pub start: u64,
    pub maximum_matches: usize,
    pub expected_stream_sha256: Option<[u8; 32]>,
}
impl fmt::Debug for OutputSearchRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputSearchRequest")
            .field("scope", &self.scope)
            .field("pattern_bytes", &self.pattern.len())
            .finish_non_exhaustive()
    }
}
impl Drop for OutputSearchRequest {
    fn drop(&mut self) {
        self.pattern.zeroize();
    }
}
impl OutputSearchRequest {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.scope.validate()?;
        if self.format != "cshell.output-search-request"
            || self.version != 1
            || self.scope.right.is_some()
            || self.pattern.is_empty()
            || self.pattern.len() > MAX_OUTPUT_SEARCH_PATTERN
            || self.start > MAX_ANALYSIS_STREAM_BYTES
            || self.maximum_matches == 0
            || self.maximum_matches > MAX_OUTPUT_SEARCH_MATCHES
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn descriptor(&self) -> Result<OutputSearchDescriptor, TaskError> {
        self.validate()?;
        Ok(OutputSearchDescriptor {
            scope: self.scope.clone(),
            pattern_bytes: self.pattern.len(),
            pattern_sha256: Sha256::digest(&self.pattern).into(),
            start: self.start,
            maximum_matches: self.maximum_matches,
            expected_stream_sha256: self.expected_stream_sha256,
        })
    }
    pub fn next(&self, document: &OutputSearchDocument) -> Result<Option<Self>, TaskError> {
        document.validate()?;
        if !document.request.same_query(&self.descriptor()?) {
            return Err(TaskError::Invalid);
        }
        let result = document.result.as_ref().ok_or(TaskError::Transition)?;
        if self
            .expected_stream_sha256
            .is_some_and(|expected| expected != result.stream.sha256)
        {
            return Err(TaskError::Invalid);
        }
        Ok(result.next.map(|start| {
            let mut next = self.clone();
            next.start = start;
            next.expected_stream_sha256 = Some(result.stream.sha256);
            next
        }))
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSearchDescriptor {
    pub scope: OutputAnalysisRequest,
    pub pattern_bytes: usize,
    pub pattern_sha256: [u8; 32],
    pub start: u64,
    pub maximum_matches: usize,
    pub expected_stream_sha256: Option<[u8; 32]>,
}
impl OutputSearchDescriptor {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.scope.validate()?;
        if self.scope.right.is_some()
            || self.pattern_bytes == 0
            || self.pattern_bytes > MAX_OUTPUT_SEARCH_PATTERN
            || self.start > MAX_ANALYSIS_STREAM_BYTES
            || self.maximum_matches == 0
            || self.maximum_matches > MAX_OUTPUT_SEARCH_MATCHES
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn same_query(&self, other: &Self) -> bool {
        self.scope == other.scope
            && self.pattern_bytes == other.pattern_bytes
            && self.pattern_sha256 == other.pattern_sha256
            && self.maximum_matches == other.maximum_matches
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSearchResult {
    pub stream: OutputStreamDigest,
    pub searched_through: u64,
    pub matches: Vec<u64>,
    pub next: Option<u64>,
}
impl OutputSearchResult {
    pub fn validate(&self, request: &OutputSearchDescriptor) -> Result<(), TaskError> {
        request.validate()?;
        if self.stream.bytes > MAX_ANALYSIS_STREAM_BYTES
            || request.start > self.stream.bytes
            || self.searched_through < request.start
            || self.searched_through > self.stream.bytes
            || self.matches.len() > request.maximum_matches
            || self.matches.windows(2).any(|pair| pair[0] >= pair[1])
            || self.matches.iter().any(|offset| {
                *offset < request.start
                    || offset
                        .checked_add(request.pattern_bytes as u64)
                        .is_none_or(|end| end > self.searched_through)
            })
            || request
                .expected_stream_sha256
                .is_some_and(|hash| hash != self.stream.sha256)
        {
            return Err(TaskError::Invalid);
        }
        if let Some(next) = self.next {
            let last = *self.matches.last().ok_or(TaskError::Invalid)?;
            if self.matches.len() != request.maximum_matches
                || next != last + 1
                || next <= request.start
                || next
                    .checked_add(request.pattern_bytes as u64)
                    .is_none_or(|end| end > self.searched_through)
            {
                return Err(TaskError::Invalid);
            }
        } else if self.searched_through != self.stream.bytes {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSearchDocument {
    pub format: String,
    pub version: u32,
    pub job_id: TaskRunId,
    pub request: OutputSearchDescriptor,
    pub binding: OutputAnalysisBinding,
    pub state: OutputAnalysisState,
    pub bytes: u64,
    pub result: Option<OutputSearchResult>,
    pub error: Option<OutputAnalysisError>,
}
impl OutputSearchDocument {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.request.validate()?;
        if self.format != "cshell.output-search" || self.version != 1 {
            return Err(TaskError::Invalid);
        }
        OutputAnalysisDocument {
            format: "cshell.output-analysis".into(),
            version: 1,
            job_id: self.job_id,
            request: self.request.scope.clone(),
            left: self.binding.clone(),
            right: None,
            state: self.state,
            left_bytes: self.bytes,
            right_bytes: None,
            result: self.result.as_ref().map(|result| OutputAnalysisResult {
                left: result.stream.clone(),
                right: None,
                equal: None,
                differing_bytes: None,
                first_difference: None,
            }),
            error: self.error,
        }
        .validate()?;
        if let Some(result) = &self.result {
            result.validate(&self.request)?;
        }
        Ok(())
    }
}
pub fn decode_output_search_request(bytes: &[u8]) -> Result<OutputSearchRequest, TaskError> {
    let value: OutputSearchRequest = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
pub fn decode_output_search(bytes: &[u8]) -> Result<OutputSearchDocument, TaskError> {
    let value: OutputSearchDocument = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
pub fn output_search_pattern(text: &str, hex: bool) -> Result<Vec<u8>, TaskError> {
    if text.len() > MAX_OUTPUT_SEARCH_PATTERN * 3 {
        return Err(TaskError::Limit);
    }
    let bytes = if hex {
        let digits: Vec<_> = text
            .bytes()
            .filter(|byte| *byte == b' ' || byte.is_ascii_hexdigit())
            .filter(|byte| *byte != b' ')
            .collect();
        if text
            .bytes()
            .any(|byte| byte != b' ' && !byte.is_ascii_hexdigit())
            || digits.len() % 2 != 0
        {
            return Err(TaskError::Invalid);
        }
        digits
            .chunks_exact(2)
            .map(|pair| {
                let digit = |byte: u8| {
                    if byte.is_ascii_digit() {
                        byte - b'0'
                    } else {
                        byte.to_ascii_lowercase() - b'a' + 10
                    }
                };
                digit(pair[0]) * 16 + digit(pair[1])
            })
            .collect()
    } else {
        text.as_bytes().to_vec()
    };
    if bytes.is_empty() || bytes.len() > MAX_OUTPUT_SEARCH_PATTERN {
        return Err(TaskError::Invalid);
    }
    Ok(bytes)
}
