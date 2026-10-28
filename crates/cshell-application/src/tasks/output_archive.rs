//! Portable raw output container. Parsing and exporting never execute its content.
use super::*;
use std::io::{self, Read};
use zeroize::Zeroize;
pub struct OutputArchiveBytes(pub Vec<u8>);
impl Drop for OutputArchiveBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
pub const OUTPUT_ARCHIVE_MAGIC: &[u8; 8] = b"CSHOUT01";
pub const MAX_ARCHIVE_JOURNAL_BYTES: u64 = (MAX_OUTPUT_MERGE_EVENTS + 2) * 72;
pub const MAX_OUTPUT_ARCHIVE_BYTES: u64 = MAX_ANALYSIS_STREAM_BYTES
    + MAX_ARCHIVE_JOURNAL_BYTES
    + MAX_OUTPUT_ANALYSIS_DOCUMENT as u64
    + 44;
pub const MAX_OUTPUT_ARCHIVE_CHUNK: usize = 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputArchiveEntryKind {
    Stdout,
    Stderr,
    Journal,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputArchiveEntry {
    pub kind: OutputArchiveEntryKind,
    pub bytes: u64,
    pub sha256: [u8; 32],
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputArchiveManifest {
    pub format: String,
    pub version: u32,
    pub archive_id: TaskRunId,
    pub scope: OutputAnalysisRequest,
    pub binding: OutputAnalysisBinding,
    pub captured_at_ms: u64,
    pub entries: Vec<OutputArchiveEntry>,
}
impl OutputArchiveManifest {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.scope.validate()?;
        self.binding.validate_for(&self.scope)?;
        if self.format != "cshell.output-archive"
            || self.version != 1
            || self.archive_id.as_uuid().is_nil()
            || self.archive_id == self.scope.run_id
            || self.scope.right.is_some()
            || self.scope.stderr
            || self.captured_at_ms > i64::MAX as u64
            || !(2..=3).contains(&self.entries.len())
            || self.entries[0].kind != OutputArchiveEntryKind::Stdout
            || self.entries[1].kind != OutputArchiveEntryKind::Stderr
            || self.entries[0]
                .bytes
                .checked_add(self.entries[1].bytes)
                .is_none_or(|bytes| bytes > MAX_ANALYSIS_STREAM_BYTES)
            || self.entries.get(2).is_some_and(|entry| {
                entry.kind != OutputArchiveEntryKind::Journal
                    || entry.bytes > MAX_ARCHIVE_JOURNAL_BYTES
            })
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
    pub fn encoded_bytes(&self) -> Result<u64, TaskError> {
        self.validate()?;
        let metadata = encode(self, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
        Ok(44 + metadata.len() as u64 + self.entries.iter().map(|entry| entry.bytes).sum::<u64>())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputArchiveArtifact {
    pub manifest: OutputArchiveManifest,
    pub bytes: u64,
    pub sha256: [u8; 32],
}
impl OutputArchiveArtifact {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.bytes != self.manifest.encoded_bytes()? || self.bytes > MAX_OUTPUT_ARCHIVE_BYTES {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputArchiveDocument {
    pub format: String,
    pub version: u32,
    pub job_id: TaskRunId,
    pub scope: OutputAnalysisRequest,
    pub binding: OutputAnalysisBinding,
    pub state: OutputAnalysisState,
    pub source_bytes: u64,
    pub bytes: u64,
    pub artifact: Option<OutputArchiveArtifact>,
    pub error: Option<OutputAnalysisError>,
}
impl OutputArchiveDocument {
    pub fn validate(&self) -> Result<(), TaskError> {
        self.scope.validate()?;
        self.binding.validate_for(&self.scope)?;
        if self.format != "cshell.output-archive-job"
            || self.version != 1
            || self.job_id.as_uuid().is_nil()
            || self.job_id == self.scope.run_id
            || self.scope.right.is_some()
            || self.scope.stderr
            || self.bytes > MAX_OUTPUT_ARCHIVE_BYTES
            || self.source_bytes > MAX_ANALYSIS_STREAM_BYTES + MAX_ARCHIVE_JOURNAL_BYTES
            || self.artifact.is_some() != (self.state == OutputAnalysisState::Completed)
            || self.error.is_some() != (self.state == OutputAnalysisState::Failed)
        {
            return Err(TaskError::Invalid);
        }
        if let Some(artifact) = &self.artifact {
            artifact.validate()?;
            if artifact.manifest.scope != self.scope
                || artifact.manifest.binding != self.binding
                || artifact.bytes != self.bytes
                || self.source_bytes
                    != artifact
                        .manifest
                        .entries
                        .iter()
                        .map(|entry| entry.bytes)
                        .sum::<u64>()
            {
                return Err(TaskError::Invalid);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputArchiveChunk {
    pub format: String,
    pub version: u32,
    pub archive_id: TaskRunId,
    pub run_id: TaskRunId,
    pub archive_sha256: [u8; 32],
    pub offset: u64,
    pub bytes: usize,
    pub next: u64,
    pub total_bytes: u64,
    pub chunk_sha256: [u8; 32],
}
impl OutputArchiveChunk {
    pub fn validate(&self, artifact: &OutputArchiveArtifact, body: &[u8]) -> Result<(), TaskError> {
        artifact.validate()?;
        let hash: [u8; 32] = Sha256::digest(body).into();
        if self.format != "cshell.output-archive-chunk"
            || self.version != 1
            || self.archive_id != artifact.manifest.archive_id
            || self.run_id != artifact.manifest.scope.run_id
            || self.archive_sha256 != artifact.sha256
            || self.total_bytes != artifact.bytes
            || self.bytes != body.len()
            || self.bytes > MAX_OUTPUT_ARCHIVE_CHUNK
            || self.offset > self.total_bytes
            || self.offset.checked_add(self.bytes as u64) != Some(self.next)
            || self.next > self.total_bytes
            || self.bytes == 0 && self.offset != self.total_bytes
            || hash != self.chunk_sha256
        {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
pub fn decode_output_archive(bytes: &[u8]) -> Result<OutputArchiveDocument, TaskError> {
    let value: OutputArchiveDocument = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate()?;
    Ok(value)
}
pub fn decode_output_archive_chunk(
    bytes: &[u8],
    body: &[u8],
    artifact: &OutputArchiveArtifact,
) -> Result<OutputArchiveChunk, TaskError> {
    let value: OutputArchiveChunk = decode(bytes, MAX_OUTPUT_ANALYSIS_DOCUMENT)?;
    value.validate(artifact, body)?;
    Ok(value)
}
/// Receives one ordered transfer without buffering its payload.
pub struct OutputArchiveCollector {
    artifact: OutputArchiveArtifact,
    offset: u64,
    hash: Sha256,
}
impl OutputArchiveCollector {
    pub fn new(artifact: OutputArchiveArtifact) -> Result<Self, TaskError> {
        artifact.validate()?;
        Ok(Self {
            artifact,
            offset: 0,
            hash: Sha256::new(),
        })
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn push(&mut self, metadata: &[u8], body: &[u8]) -> Result<bool, TaskError> {
        let chunk = decode_output_archive_chunk(metadata, body, &self.artifact)?;
        if chunk.offset != self.offset
            || chunk.bytes
                != MAX_OUTPUT_ARCHIVE_CHUNK.min((self.artifact.bytes - self.offset) as usize)
        {
            return Err(TaskError::Invalid);
        }
        self.hash.update(body);
        self.offset = chunk.next;
        Ok(self.offset == self.artifact.bytes)
    }
    pub fn finish(self) -> Result<(), TaskError> {
        let hash: [u8; 32] = self.hash.finalize().into();
        if self.offset != self.artifact.bytes || hash != self.artifact.sha256 {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
struct ArchiveWriter<'a, W> {
    output: &'a mut W,
    hash: Sha256,
    bytes: u64,
}
impl<W: Write> Write for ArchiveWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .checked_add(bytes.len() as u64)
            .is_none_or(|bytes| bytes > MAX_OUTPUT_ARCHIVE_BYTES)
        {
            return Err(io::Error::other("archive limit"));
        }
        let count = self.output.write(bytes)?;
        self.hash.update(&bytes[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
pub fn write_output_archive(
    manifest: &OutputArchiveManifest,
    sources: &mut [&mut dyn Read],
    output: &mut impl Write,
) -> io::Result<OutputArchiveArtifact> {
    manifest
        .validate()
        .map_err(|_| io::Error::other("invalid archive manifest"))?;
    if sources.len() != manifest.entries.len() {
        return Err(io::Error::other("archive entry count"));
    }
    let metadata = encode(manifest, MAX_OUTPUT_ANALYSIS_DOCUMENT)
        .map_err(|_| io::Error::other("archive metadata limit"))?;
    let mut writer = ArchiveWriter {
        output,
        hash: Sha256::new(),
        bytes: 0,
    };
    writer.write_all(OUTPUT_ARCHIVE_MAGIC)?;
    writer.write_all(&(metadata.len() as u32).to_le_bytes())?;
    writer.write_all(&metadata)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    for (entry, source) in manifest.entries.iter().zip(sources.iter_mut()) {
        let mut remaining = entry.bytes;
        let mut hash = Sha256::new();
        while remaining > 0 {
            let count = buffer.len().min(remaining as usize);
            source.read_exact(&mut buffer[..count])?;
            hash.update(&buffer[..count]);
            writer.write_all(&buffer[..count])?;
            remaining -= count as u64;
        }
        let actual: [u8; 32] = hash.finalize().into();
        if actual != entry.sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive source changed",
            ));
        }
    }
    let footer: [u8; 32] = writer.hash.clone().finalize().into();
    writer.write_all(&footer)?;
    writer.flush()?;
    let artifact = OutputArchiveArtifact {
        manifest: manifest.clone(),
        bytes: writer.bytes,
        sha256: writer.hash.finalize().into(),
    };
    artifact
        .validate()
        .map_err(|_| io::Error::other("invalid archive"))?;
    Ok(artifact)
}
/// Reads and validates exactly one complete container, including EOF; never extracts files.
pub async fn copy_output_archive<F, Fut, W, C, P>(
    artifact: &OutputArchiveArtifact,
    output: &mut W,
    mut fetch: F,
    mut check: C,
    mut progress: P,
) -> io::Result<()>
where
    F: FnMut(u64, usize) -> Fut,
    Fut: std::future::Future<Output = io::Result<(Vec<u8>, OutputArchiveBytes)>>,
    W: tokio::io::AsyncWrite + Unpin,
    C: FnMut() -> io::Result<()>,
    P: FnMut(u64),
{
    use tokio::io::AsyncWriteExt;
    let mut collector = OutputArchiveCollector::new(artifact.clone())
        .map_err(|_| io::Error::other("invalid archive artifact"))?;
    while collector.offset() < artifact.bytes {
        check()?;
        let (metadata, body) = fetch(collector.offset(), MAX_OUTPUT_ARCHIVE_CHUNK).await?;
        collector
            .push(&metadata, &body.0)
            .map_err(|_| io::Error::other("archive transfer mismatch"))?;
        output.write_all(&body.0).await?;
        progress(body.0.len() as u64);
    }
    output.flush().await?;
    check()?;
    collector
        .finish()
        .map_err(|_| io::Error::other("archive transfer digest"))
}
pub fn inspect_output_archive_checked(
    input: &mut impl Read,
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<OutputArchiveArtifact> {
    struct Checked<'a, R, C> {
        input: &'a mut R,
        check: &'a mut C,
    }
    impl<R: Read, C: FnMut() -> io::Result<()>> Read for Checked<'_, R, C> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            (self.check)()?;
            self.input.read(bytes)
        }
    }
    inspect_output_archive(&mut Checked {
        input,
        check: &mut check,
    })
}
pub fn inspect_output_archive(input: &mut impl Read) -> io::Result<OutputArchiveArtifact> {
    let mut header = [0_u8; 12];
    input.read_exact(&mut header)?;
    if &header[..8] != OUTPUT_ARCHIVE_MAGIC {
        return Err(io::Error::other("invalid archive magic"));
    }
    let length = u32::from_le_bytes(
        header[8..12]
            .try_into()
            .map_err(|_| io::Error::other("invalid archive header"))?,
    ) as usize;
    if length == 0 || length > MAX_OUTPUT_ANALYSIS_DOCUMENT {
        return Err(io::Error::other("archive metadata limit"));
    }
    let mut metadata = vec![0; length];
    input.read_exact(&mut metadata)?;
    let manifest: OutputArchiveManifest = decode(&metadata, MAX_OUTPUT_ANALYSIS_DOCUMENT)
        .map_err(|_| io::Error::other("invalid archive metadata"))?;
    manifest
        .validate()
        .map_err(|_| io::Error::other("invalid archive manifest"))?;
    if encode(&manifest, MAX_OUTPUT_ANALYSIS_DOCUMENT)
        .map_err(|_| io::Error::other("archive metadata limit"))?
        != metadata
    {
        return Err(io::Error::other("noncanonical archive metadata"));
    }
    let mut hash = Sha256::new();
    hash.update(header);
    hash.update(&metadata);
    let mut bytes = 12 + length as u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    for entry in &manifest.entries {
        let mut remaining = entry.bytes;
        let mut entry_hash = Sha256::new();
        while remaining > 0 {
            let count = buffer.len().min(remaining as usize);
            input.read_exact(&mut buffer[..count])?;
            entry_hash.update(&buffer[..count]);
            hash.update(&buffer[..count]);
            bytes += count as u64;
            remaining -= count as u64;
        }
        let actual: [u8; 32] = entry_hash.finalize().into();
        if actual != entry.sha256 {
            return Err(io::Error::other("archive entry checksum"));
        }
    }
    let expected: [u8; 32] = hash.clone().finalize().into();
    let mut footer = [0_u8; 32];
    input.read_exact(&mut footer)?;
    if footer != expected {
        return Err(io::Error::other("archive checksum"));
    }
    hash.update(footer);
    bytes += 32;
    let mut extra = [0_u8; 1];
    if input.read(&mut extra)? != 0 {
        return Err(io::Error::other("archive trailing bytes"));
    }
    let artifact = OutputArchiveArtifact {
        manifest,
        bytes,
        sha256: hash.finalize().into(),
    };
    artifact
        .validate()
        .map_err(|_| io::Error::other("invalid archive length"))?;
    Ok(artifact)
}
