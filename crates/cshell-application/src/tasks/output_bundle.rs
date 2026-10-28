//! A bounded bundle of independently verifiable CSHOUT01 archives.
use super::*;
use std::io::{self, Read, Write};
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "artifact", rename_all = "snake_case")]
pub enum OutputContainerArtifact {
    Archive(OutputArchiveArtifact),
    Bundle(OutputBundleArtifact),
}
struct CheckedContainer<'a, R, C> {
    input: &'a mut R,
    check: C,
}
impl<R: Read, C: FnMut() -> io::Result<()>> Read for CheckedContainer<'_, R, C> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (self.check)()?;
        self.input.read(bytes)
    }
}
impl<R: std::io::Seek, C: FnMut() -> io::Result<()>> std::io::Seek for CheckedContainer<'_, R, C> {
    fn seek(&mut self, at: std::io::SeekFrom) -> io::Result<u64> {
        (self.check)()?;
        self.input.seek(at)
    }
}
pub fn inspect_output_container_checked(
    input: &mut (impl Read + std::io::Seek),
    check: impl FnMut() -> io::Result<()>,
) -> io::Result<OutputContainerArtifact> {
    inspect_output_container(&mut CheckedContainer { input, check })
}
pub fn extract_output_container_checked(
    input: &mut (impl Read + std::io::Seek),
    expected: &OutputContainerArtifact,
    check: impl FnMut() -> io::Result<()>,
    open: impl FnMut(&OutputArchiveManifest, OutputArchiveEntryKind) -> io::Result<Box<dyn Write>>,
) -> io::Result<()> {
    extract_output_container(&mut CheckedContainer { input, check }, expected, open)
}
pub fn inspect_output_container(
    input: &mut (impl Read + std::io::Seek),
) -> io::Result<OutputContainerArtifact> {
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    input.rewind()?;
    if &magic == OUTPUT_ARCHIVE_MAGIC {
        inspect_output_archive(input).map(OutputContainerArtifact::Archive)
    } else if &magic == OUTPUT_BUNDLE_MAGIC {
        inspect_output_bundle(input).map(OutputContainerArtifact::Bundle)
    } else {
        Err(io::Error::other("unsupported output container"))
    }
}
pub fn extract_output_container(
    input: &mut (impl Read + std::io::Seek),
    expected: &OutputContainerArtifact,
    open: impl FnMut(&OutputArchiveManifest, OutputArchiveEntryKind) -> io::Result<Box<dyn Write>>,
) -> io::Result<()> {
    input.rewind()?;
    match expected {
        OutputContainerArtifact::Archive(artifact) => extract_output_archive(input, artifact, open),
        OutputContainerArtifact::Bundle(artifact) => extract_output_bundle(input, artifact, open),
    }
}
pub const OUTPUT_BUNDLE_MAGIC: &[u8; 8] = b"CSBND001";
pub const MAX_OUTPUT_BUNDLE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputBundleManifest {
    pub format: String,
    pub version: u32,
    pub id: TaskRunId,
    pub entries: Vec<OutputArchiveArtifact>,
}
impl OutputBundleManifest {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.format != "cshell.output-bundle"
            || self.version != 1
            || self.id.as_uuid().is_nil()
            || self.entries.is_empty()
            || self.entries.len() > MAX_WORKSPACE_SOURCES
        {
            return Err(TaskError::Invalid);
        }
        let mut keys = BTreeSet::new();
        let mut bytes = 44_u64;
        for entry in &self.entries {
            entry.validate()?;
            let s = &entry.manifest.scope;
            if !keys.insert((s.run_id, s.left, s.step)) {
                return Err(TaskError::Invalid);
            }
            bytes = bytes.checked_add(entry.bytes).ok_or(TaskError::Limit)?;
        }
        bytes = bytes
            .checked_add(encode(self, MAX_WORKSPACE_DOCUMENT)?.len() as u64)
            .ok_or(TaskError::Limit)?;
        if bytes > MAX_OUTPUT_BUNDLE_BYTES {
            return Err(TaskError::Limit);
        }
        Ok(())
    }
    pub fn bytes(&self) -> Result<u64, TaskError> {
        self.validate()?;
        Ok(44
            + encode(self, MAX_WORKSPACE_DOCUMENT)?.len() as u64
            + self.entries.iter().map(|entry| entry.bytes).sum::<u64>())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputBundleArtifact {
    pub manifest: OutputBundleManifest,
    pub bytes: u64,
    pub sha256: [u8; 32],
}
impl OutputBundleArtifact {
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.bytes != self.manifest.bytes()? {
            return Err(TaskError::Invalid);
        }
        Ok(())
    }
}
struct HashWriter<'a, W: Write> {
    output: &'a mut W,
    hash: Sha256,
    bytes: u64,
}
impl<W: Write> Write for HashWriter<'_, W> {
    fn write(&mut self, body: &[u8]) -> io::Result<usize> {
        let n = self.output.write(body)?;
        self.hash.update(&body[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
struct HashReader<'a, R: Read> {
    input: &'a mut R,
    hash: Sha256,
    bytes: u64,
}
impl<R: Read> Read for HashReader<'_, R> {
    fn read(&mut self, body: &mut [u8]) -> io::Result<usize> {
        let n = self.input.read(body)?;
        self.hash.update(&body[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}
pub fn write_output_bundle(
    manifest: &OutputBundleManifest,
    inputs: &mut [&mut dyn Read],
    output: &mut impl Write,
) -> io::Result<OutputBundleArtifact> {
    manifest
        .validate()
        .map_err(|_| io::Error::other("invalid bundle manifest"))?;
    if inputs.len() != manifest.entries.len() {
        return Err(io::Error::other("bundle entry count"));
    }
    let metadata = encode(manifest, MAX_WORKSPACE_DOCUMENT)
        .map_err(|_| io::Error::other("bundle metadata limit"))?;
    let mut output = HashWriter {
        output,
        hash: Sha256::new(),
        bytes: 0,
    };
    output.write_all(OUTPUT_BUNDLE_MAGIC)?;
    output.write_all(&(metadata.len() as u32).to_le_bytes())?;
    output.write_all(&metadata)?;
    let mut buffer = vec![0; 64 * 1024];
    for (entry, input) in manifest.entries.iter().zip(inputs.iter_mut()) {
        let mut hash = Sha256::new();
        let mut remaining = entry.bytes;
        while remaining > 0 {
            let n = buffer.len().min(remaining as usize);
            input.read_exact(&mut buffer[..n])?;
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?;
            remaining -= n as u64;
        }
        let digest: [u8; 32] = hash.finalize().into();
        let mut extra = [0; 1];
        if digest != entry.sha256 || input.read(&mut extra)? != 0 {
            return Err(io::Error::other("bundle member changed"));
        }
    }
    let footer: [u8; 32] = output.hash.clone().finalize().into();
    output.write_all(&footer)?;
    output.flush()?;
    Ok(OutputBundleArtifact {
        manifest: manifest.clone(),
        bytes: output.bytes,
        sha256: output.hash.finalize().into(),
    })
}
pub fn inspect_output_bundle(input: &mut impl Read) -> io::Result<OutputBundleArtifact> {
    let mut input = HashReader {
        input,
        hash: Sha256::new(),
        bytes: 0,
    };
    let mut header = [0; 12];
    input.read_exact(&mut header)?;
    if &header[..8] != OUTPUT_BUNDLE_MAGIC {
        return Err(io::Error::other("invalid bundle magic"));
    }
    let length = u32::from_le_bytes(
        header[8..]
            .try_into()
            .map_err(|_| io::Error::other("bundle header"))?,
    ) as usize;
    if length == 0 || length > MAX_WORKSPACE_DOCUMENT {
        return Err(io::Error::other("bundle metadata limit"));
    }
    let mut metadata = vec![0; length];
    input.read_exact(&mut metadata)?;
    let manifest: OutputBundleManifest = decode(&metadata, MAX_WORKSPACE_DOCUMENT)
        .map_err(|_| io::Error::other("invalid bundle metadata"))?;
    manifest
        .validate()
        .map_err(|_| io::Error::other("invalid bundle manifest"))?;
    if encode(&manifest, MAX_WORKSPACE_DOCUMENT).map_err(|_| io::Error::other("bundle metadata"))?
        != metadata
    {
        return Err(io::Error::other("noncanonical bundle manifest"));
    }
    for entry in &manifest.entries {
        let mut member = (&mut input).take(entry.bytes);
        if inspect_output_archive(&mut member)? != *entry || member.limit() != 0 {
            return Err(io::Error::other("bundle member mismatch"));
        }
    }
    let expected: [u8; 32] = input.hash.clone().finalize().into();
    let mut footer = [0; 32];
    input.read_exact(&mut footer)?;
    if footer != expected {
        return Err(io::Error::other("bundle footer"));
    }
    let mut extra = [0; 1];
    if input.read(&mut extra)? != 0 {
        return Err(io::Error::other("bundle trailing bytes"));
    }
    let artifact = OutputBundleArtifact {
        manifest,
        bytes: input.bytes,
        sha256: input.hash.finalize().into(),
    };
    artifact
        .validate()
        .map_err(|_| io::Error::other("bundle length"))?;
    Ok(artifact)
}
/// Caller verifies the whole container first, rewinds the same handle, then extracts
/// to private staging. Only typed entry names reach the output callback.
pub fn extract_output_archive(
    input: &mut impl Read,
    expected: &OutputArchiveArtifact,
    mut open: impl FnMut(&OutputArchiveManifest, OutputArchiveEntryKind) -> io::Result<Box<dyn Write>>,
) -> io::Result<()> {
    expected
        .validate()
        .map_err(|_| io::Error::other("invalid archive artifact"))?;
    let mut header = [0; 12];
    input.read_exact(&mut header)?;
    let length = u32::from_le_bytes(
        header[8..]
            .try_into()
            .map_err(|_| io::Error::other("archive header"))?,
    ) as usize;
    if &header[..8] != OUTPUT_ARCHIVE_MAGIC || length == 0 || length > MAX_OUTPUT_ANALYSIS_DOCUMENT
    {
        return Err(io::Error::other("invalid archive header"));
    }
    let mut metadata = vec![0; length];
    input.read_exact(&mut metadata)?;
    if encode(&expected.manifest, MAX_OUTPUT_ANALYSIS_DOCUMENT)
        .map_err(|_| io::Error::other("archive metadata"))?
        != metadata
    {
        return Err(io::Error::other("archive manifest changed"));
    }
    let mut hash = Sha256::new();
    hash.update(header);
    hash.update(&metadata);
    let mut buffer = vec![0; 64 * 1024];
    for entry in &expected.manifest.entries {
        let mut output = open(&expected.manifest, entry.kind)?;
        let mut digest = Sha256::new();
        let mut remaining = entry.bytes;
        while remaining > 0 {
            let n = buffer.len().min(remaining as usize);
            input.read_exact(&mut buffer[..n])?;
            digest.update(&buffer[..n]);
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?;
            remaining -= n as u64;
        }
        output.flush()?;
        let actual: [u8; 32] = digest.finalize().into();
        if actual != entry.sha256 {
            return Err(io::Error::other("extracted entry changed"));
        }
    }
    let footer: [u8; 32] = hash.clone().finalize().into();
    let mut actual = [0; 32];
    input.read_exact(&mut actual)?;
    hash.update(actual);
    let digest: [u8; 32] = hash.finalize().into();
    let mut extra = [0; 1];
    if footer != actual || digest != expected.sha256 || input.read(&mut extra)? != 0 {
        return Err(io::Error::other("archive changed during extraction"));
    }
    Ok(())
}
pub fn extract_output_bundle(
    input: &mut impl Read,
    expected: &OutputBundleArtifact,
    mut open: impl FnMut(&OutputArchiveManifest, OutputArchiveEntryKind) -> io::Result<Box<dyn Write>>,
) -> io::Result<()> {
    expected
        .validate()
        .map_err(|_| io::Error::other("invalid bundle artifact"))?;
    let mut input = HashReader {
        input,
        hash: Sha256::new(),
        bytes: 0,
    };
    let mut header = [0; 12];
    input.read_exact(&mut header)?;
    let length = u32::from_le_bytes(
        header[8..]
            .try_into()
            .map_err(|_| io::Error::other("bundle header"))?,
    ) as usize;
    if &header[..8] != OUTPUT_BUNDLE_MAGIC || length == 0 || length > MAX_WORKSPACE_DOCUMENT {
        return Err(io::Error::other("bundle header"));
    }
    let mut metadata = vec![0; length];
    input.read_exact(&mut metadata)?;
    if encode(&expected.manifest, MAX_WORKSPACE_DOCUMENT)
        .map_err(|_| io::Error::other("bundle metadata"))?
        != metadata
    {
        return Err(io::Error::other("bundle metadata changed"));
    }
    for entry in &expected.manifest.entries {
        let mut member = (&mut input).take(entry.bytes);
        extract_output_archive(&mut member, entry, &mut open)?;
        if member.limit() != 0 {
            return Err(io::Error::other("bundle member length"));
        }
    }
    let footer: [u8; 32] = input.hash.clone().finalize().into();
    let mut actual = [0; 32];
    input.read_exact(&mut actual)?;
    let mut extra = [0; 1];
    if input.read(&mut extra)? != 0 {
        return Err(io::Error::other("bundle EOF"));
    }
    let digest: [u8; 32] = input.hash.finalize().into();
    if footer != actual || digest != expected.sha256 || input.bytes != expected.bytes {
        return Err(io::Error::other("bundle changed during extraction"));
    }
    Ok(())
}
