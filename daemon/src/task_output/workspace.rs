//! Full pinned observations, private persistent line indexes and explicit retention.
use super::analysis::{PinnedOutput, storage};
use super::*;
use cshell_application::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};
const MAX_INDEX_LINES: usize = 131_072;
const MAX_MATCHES: usize = 8192;
#[derive(Debug, Default)]
pub(crate) struct WorkspaceProgress {
    pub bytes: AtomicU64,
    pub sources: AtomicU64,
}
pub(crate) struct WorkspaceCapture {
    pub rows: Vec<OutputWorkspaceRow>,
    pub groups: Vec<OutputDigestGroup>,
    pub retention: Option<OutputRetentionPlan>,
    pub artifact: Option<OutputBundleArtifact>,
    pub bundle: Option<WorkspaceBundle>,
    pub truncated: bool,
}
pub(crate) struct WorkspaceBundle {
    pub artifact: OutputBundleArtifact,
    file: File,
    _lease: File,
    _directory: tempfile::TempDir,
}
impl WorkspaceBundle {
    pub(crate) fn read(&self, offset: u64, limit: usize) -> Result<Vec<u8>, OutputAnalysisError> {
        if limit == 0 || limit > MAX_OUTPUT_ARCHIVE_CHUNK || offset > self.artifact.bytes {
            return Err(OutputAnalysisError::Invalid);
        }
        let mut bytes = vec![0; limit.min((self.artifact.bytes - offset) as usize)];
        let mut done = 0;
        while done < bytes.len() {
            #[cfg(unix)]
            let n = {
                use std::os::unix::fs::FileExt;
                self.file.read_at(&mut bytes[done..], offset + done as u64)
            };
            #[cfg(windows)]
            let n = {
                use std::os::windows::fs::FileExt;
                self.file
                    .seek_read(&mut bytes[done..], offset + done as u64)
            };
            let n = n.map_err(storage)?;
            if n == 0 {
                return Err(OutputAnalysisError::Changed);
            }
            done += n;
        }
        Ok(bytes)
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LineIndex {
    format: String,
    version: u32,
    binding: OutputAnalysisBinding,
    scope: OutputAnalysisRequest,
    digest: OutputStreamDigest,
    lines: u64,
    offsets: Vec<u64>,
}
fn check(cancel: &AtomicBool, started: Instant) -> Result<(), OutputAnalysisError> {
    if cancel.load(Ordering::Acquire) {
        return Err(OutputAnalysisError::Worker);
    }
    if started.elapsed() > Duration::from_secs(600) {
        return Err(OutputAnalysisError::Timeout);
    }
    Ok(())
}
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn now() -> Result<u64, OutputAnalysisError> {
    SystemTaskClock::new()
        .and_then(|c| c.now_ms())
        .map_err(|_| OutputAnalysisError::Storage)
}
impl TaskOutputStore {
    pub(crate) fn workspace_index(
        &self,
        scope: OutputAnalysisRequest,
        entry: OutputIndexEntry,
        offset: usize,
    ) -> Result<OutputIndexPage, OutputAnalysisError> {
        let _guard = self
            .indexes
            .lock()
            .map_err(|_| OutputAnalysisError::Storage)?;
        let root = RuntimePaths::prepare(self.root.join("indexes"))
            .map_err(|_| OutputAnalysisError::Storage)?;
        let name = entry
            .index_sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = root.root().join(format!("{name}.json"));
        let metadata = std::fs::symlink_metadata(&path).map_err(storage)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_WORKSPACE_DOCUMENT as u64
        {
            return Err(OutputAnalysisError::Invalid);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(OutputAnalysisError::Invalid);
            }
        }
        let bytes = std::fs::read(path).map_err(storage)?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let index: LineIndex =
            serde_json::from_slice(&bytes).map_err(|_| OutputAnalysisError::Invalid)?;
        if digest != entry.index_sha256
            || index.format != "cshell.output-line-index"
            || index.version != 1
            || index.scope != scope
            || index.binding != entry.binding
            || index.digest != entry.digest
            || index.lines != entry.lines
            || index.offsets.len() != entry.indexed_lines
            || index.offsets.windows(2).any(|p| p[0] >= p[1])
            || index.offsets.iter().any(|p| *p >= entry.digest.bytes)
            || offset > index.offsets.len()
        {
            return Err(OutputAnalysisError::Changed);
        }
        let positions = index
            .offsets
            .iter()
            .skip(offset)
            .take(WORKSPACE_PAGE_ROWS)
            .copied()
            .collect::<Vec<_>>();
        let page = OutputIndexPage {
            format: "cshell.output-index-page".into(),
            version: 1,
            entry,
            scope,
            offset,
            next: offset + positions.len(),
            positions,
        };
        page.validate().map_err(|_| OutputAnalysisError::Invalid)?;
        Ok(page)
    }
    fn observe_workspace(
        &self,
        source: &OutputAnalysisRequest,
        binding: &OutputAnalysisBinding,
        query: Option<&[u8]>,
        text: bool,
        cancel: &AtomicBool,
        progress: &WorkspaceProgress,
        started: Instant,
    ) -> Result<(OutputIndexEntry, Vec<(u64, u64)>, Vec<u8>, bool), OutputAnalysisError> {
        let mut input = self.pin_analysis(
            binding.child_id,
            source.left,
            binding.child_step as usize,
            source.stderr,
        )?;
        if text && input.metadata.len() > MAX_TEXT_DIFF_BYTES as u64 {
            return Err(OutputAnalysisError::Invalid);
        }
        let mut hash = Sha256::new();
        let mut offsets = Vec::from([0_u64]);
        let mut lines = 0_u64;
        let mut buffer = vec![0; 64 * 1024];
        let mut offset = 0;
        let mut body = Vec::new();
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut last_newline = false;
        let mut prefix = Vec::new();
        let mut matched = 0;
        let mut line_ring = Vec::new();
        if let Some(query) = query {
            prefix.resize(query.len(), 0);
            line_ring.resize(query.len(), 0_u64);
            let mut k = 0;
            for i in 1..query.len() {
                while k > 0 && query[k] != query[i] {
                    k = prefix[k - 1];
                }
                if query[k] == query[i] {
                    k += 1;
                }
                prefix[i] = k;
            }
        }
        while offset < input.metadata.len() {
            check(cancel, started)?;
            let n = buffer.len().min((input.metadata.len() - offset) as usize);
            input.file.read_exact(&mut buffer[..n]).map_err(storage)?;
            hash.update(&buffer[..n]);
            if text {
                body.extend_from_slice(&buffer[..n]);
            }
            for byte in &buffer[..n] {
                if let Some(query) = query {
                    line_ring[offset as usize % query.len()] = lines + 1;
                    while matched > 0 && query[matched] != *byte {
                        matched = prefix[matched - 1];
                    }
                    if query[matched] == *byte {
                        matched += 1;
                    }
                    if matched == query.len() {
                        let at = offset + 1 - query.len() as u64;
                        if matches.len() < MAX_MATCHES {
                            matches.push((at, line_ring[at as usize % query.len()]));
                        } else {
                            truncated = true;
                        }
                        matched = prefix[matched - 1];
                    }
                }
                last_newline = *byte == b'\n';
                offset += 1;
                if last_newline {
                    lines += 1;
                    if offsets.len() < MAX_INDEX_LINES {
                        offsets.push(offset);
                    }
                }
            }
            progress.bytes.fetch_add(n as u64, Ordering::Release);
        }
        if offset > 0 && !last_newline {
            lines += 1;
        }
        if offsets.last().copied() == Some(offset) {
            offsets.pop();
        }
        input.check_after(self.limit)?;
        let digest = OutputStreamDigest {
            bytes: offset,
            sha256: hash.finalize().into(),
        };
        let index = LineIndex {
            format: "cshell.output-line-index".into(),
            version: 1,
            scope: source.clone(),
            binding: binding.clone(),
            digest: digest.clone(),
            lines,
            offsets,
        };
        let metadata = serde_json::to_vec(&index).map_err(|_| OutputAnalysisError::Storage)?;
        let index_sha256: [u8; 32] = Sha256::digest(&metadata).into();
        let _index_guard = self
            .indexes
            .lock()
            .map_err(|_| OutputAnalysisError::Storage)?;
        let parent = RuntimePaths::prepare(self.root.join("indexes"))
            .map_err(|_| OutputAnalysisError::Storage)?;
        self.trim_workspace_indexes(parent.root(), metadata.len() as u64)?;
        let name = index_sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = parent.root().join(format!("{name}.json"));
        match options().open(&path) {
            Ok(mut file) => {
                file.write_all(&metadata)
                    .and_then(|_| file.sync_all())
                    .map_err(storage)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(&path).map_err(storage)?;
                if !meta.is_file()
                    || meta.file_type().is_symlink()
                    || meta.len() != metadata.len() as u64
                {
                    return Err(OutputAnalysisError::Changed);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err(OutputAnalysisError::Invalid);
                    }
                }
                let actual = std::fs::read(&path).map_err(storage)?;
                if actual != metadata {
                    return Err(OutputAnalysisError::Changed);
                }
            }
            Err(error) => return Err(storage(error)),
        }
        Ok((
            OutputIndexEntry {
                source: 0,
                binding: binding.clone(),
                digest,
                lines,
                indexed_lines: index.offsets.len(),
                index_sha256,
            },
            matches,
            body,
            truncated,
        ))
    }
    fn trim_workspace_indexes(
        &self,
        directory: &std::path::Path,
        incoming: u64,
    ) -> Result<(), OutputAnalysisError> {
        let mut files = Vec::new();
        let mut total = incoming;
        for entry in std::fs::read_dir(directory).map_err(storage)?.take(4097) {
            let entry = entry.map_err(storage)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(OutputAnalysisError::Invalid);
            };
            let Some(stem) = name.strip_suffix(".json") else {
                return Err(OutputAnalysisError::Invalid);
            };
            if stem.len() != 64 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(OutputAnalysisError::Invalid);
            }
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(storage)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(OutputAnalysisError::Invalid);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(OutputAnalysisError::Invalid);
                }
            }
            total = total
                .checked_add(metadata.len())
                .ok_or(OutputAnalysisError::Invalid)?;
            files.push((
                metadata.modified().map_err(storage)?,
                entry.path(),
                metadata.len(),
            ));
        }
        if files.len() > 4096 {
            return Err(OutputAnalysisError::Invalid);
        }
        files.sort_by_key(|entry| entry.0);
        let mut count = files.len() + 1;
        for (_, path, bytes) in files {
            if total <= 256 * 1024 * 1024 && count <= 1024 {
                break;
            }
            match std::fs::remove_file(path) {
                Ok(()) => {
                    total = total.saturating_sub(bytes);
                    count = count.saturating_sub(1);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(storage(e)),
            }
        }
        Ok(())
    }
    pub(crate) fn run_workspace(
        &self,
        request: OutputWorkspaceRequest,
        bindings: Vec<OutputAnalysisBinding>,
        cancel: &AtomicBool,
        progress: &WorkspaceProgress,
    ) -> Result<WorkspaceCapture, OutputAnalysisError> {
        let started = Instant::now();
        let _cleaned = self.clean_old_spools();
        request
            .validate()
            .map_err(|_| OutputAnalysisError::Invalid)?;
        if bindings.len() != request.sources.len() {
            return Err(OutputAnalysisError::Invalid);
        }
        let mut result = WorkspaceCapture {
            rows: Vec::new(),
            groups: Vec::new(),
            retention: None,
            artifact: None,
            bundle: None,
            truncated: false,
        };
        if request.action == OutputWorkspaceAction::Bundle {
            let mut captures = Vec::new();
            let mut bytes = 0_u64;
            for (source, binding) in request.sources.iter().zip(&bindings) {
                check(cancel, started)?;
                let capture = self.capture_archive(
                    source.clone(),
                    binding.clone(),
                    cancel,
                    &AnalysisProgress::default(),
                )?;
                bytes = bytes
                    .checked_add(capture.artifact.bytes)
                    .ok_or(OutputAnalysisError::Invalid)?;
                if bytes > MAX_OUTPUT_BUNDLE_BYTES {
                    return Err(OutputAnalysisError::Invalid);
                }
                progress
                    .bytes
                    .fetch_add(capture.artifact.bytes, Ordering::Release);
                progress.sources.fetch_add(1, Ordering::Release);
                result.rows.push(OutputWorkspaceRow::Bundle {
                    manifest: capture.artifact.manifest.clone(),
                });
                captures.push(capture);
            }
            let manifest = OutputBundleManifest {
                format: "cshell.output-bundle".into(),
                version: 1,
                id: TaskRunId::new(),
                entries: captures
                    .iter()
                    .map(|capture| capture.artifact.clone())
                    .collect(),
            };
            manifest
                .validate()
                .map_err(|_| OutputAnalysisError::Invalid)?;
            let parent = RuntimePaths::prepare(self.root.join("bundles"))
                .map_err(|_| OutputAnalysisError::Storage)?;
            let directory = tempfile::Builder::new()
                .prefix("bundle-")
                .tempdir_in(parent.root())
                .map_err(storage)?;
            RuntimePaths::prepare(directory.path().to_owned())
                .map_err(|_| OutputAnalysisError::Storage)?;
            let lease = super::spool::lease(directory.path(), request.sources[0].run_id)
                .map_err(|_| OutputAnalysisError::Storage)?;
            let mut file = options()
                .open(directory.path().join("bundle.bin"))
                .map_err(storage)?;
            struct Member<'a> {
                capture: &'a ArchiveCapture,
                offset: u64,
                cancel: &'a AtomicBool,
                started: Instant,
            }
            impl Read for Member<'_> {
                fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                    check(self.cancel, self.started)
                        .map_err(|_| std::io::Error::other("bundle cancelled/deadline"))?;
                    if bytes.is_empty() {
                        return Ok(0);
                    }
                    let body = self
                        .capture
                        .read(self.offset, bytes.len().min(MAX_OUTPUT_ARCHIVE_CHUNK))
                        .map_err(|_| std::io::Error::other("bundle member read"))?;
                    bytes[..body.len()].copy_from_slice(&body);
                    self.offset += body.len() as u64;
                    Ok(body.len())
                }
            }
            let mut readers = captures
                .iter()
                .map(|capture| Member {
                    capture,
                    offset: 0,
                    cancel,
                    started,
                })
                .collect::<Vec<_>>();
            let mut readers = readers
                .iter_mut()
                .map(|reader| reader as &mut dyn Read)
                .collect::<Vec<_>>();
            let artifact =
                write_output_bundle(&manifest, &mut readers, &mut file).map_err(storage)?;
            file.sync_all().map_err(storage)?;
            check(cancel, started)?;
            result.artifact = Some(artifact.clone());
            result.bundle = Some(WorkspaceBundle {
                _lease: lease,
                artifact,
                file,
                _directory: directory,
            });
            return Ok(result);
        }
        if matches!(
            request.action,
            OutputWorkspaceAction::RetentionPreview | OutputWorkspaceAction::RetentionApply
        ) {
            return self.retention_workspace(request, bindings, cancel, progress, started);
        }
        let mut bodies = Vec::new();
        for (i, (source, binding)) in request.sources.iter().zip(&bindings).enumerate() {
            let (mut entry, matches, body, truncated) = self.observe_workspace(
                source,
                binding,
                request.query.as_ref().map(|s| s.as_bytes()),
                request.action == OutputWorkspaceAction::TextDiff,
                cancel,
                progress,
                started,
            )?;
            entry.source = i;
            if (entry.indexed_lines as u64) < entry.lines {
                result.truncated = true;
            }
            if let Some(group) = result
                .groups
                .iter_mut()
                .find(|group| group.digest == entry.digest)
            {
                group.sources.push(i);
            } else {
                result.groups.push(OutputDigestGroup {
                    digest: entry.digest.clone(),
                    sources: vec![i],
                });
            }
            match request.action {
                OutputWorkspaceAction::Search => {
                    for (offset, line) in matches {
                        if result.rows.len() >= MAX_MATCHES {
                            result.truncated = true;
                            break;
                        }
                        result.rows.push(OutputWorkspaceRow::Match {
                            source: i,
                            offset,
                            bytes: request.query.as_ref().map_or(0, |q| q.len()),
                            line,
                        });
                    }
                    result.truncated |= truncated;
                }
                OutputWorkspaceAction::TextDiff => bodies.push(body),
                _ => result.rows.push(OutputWorkspaceRow::Index { entry }),
            }
            progress.sources.fetch_add(1, Ordering::Release);
        }
        if request.action == OutputWorkspaceAction::TextDiff {
            check(cancel, started)?;
            result.rows = align_text_output(&bodies[0], &bodies[1])
                .map_err(|_| OutputAnalysisError::Invalid)?
                .into_iter()
                .map(|row| OutputWorkspaceRow::Diff { row })
                .collect();
        }
        if serde_json::to_vec(&result.rows)
            .map_err(|_| OutputAnalysisError::Storage)?
            .len()
            > MAX_WORKSPACE_RESULT
        {
            return Err(OutputAnalysisError::Invalid);
        }
        Ok(result)
    }
    fn digest_retention(
        &self,
        scope: &OutputAnalysisRequest,
        binding: &OutputAnalysisBinding,
        cancel: &AtomicBool,
        started: Instant,
    ) -> Result<(OutputRetentionEntry, Vec<PinnedOutput>), OutputAnalysisError> {
        let mut inputs = vec![
            self.pin_analysis(
                binding.child_id,
                scope.left,
                binding.child_step as usize,
                false,
            )?,
            self.pin_analysis(
                binding.child_id,
                scope.left,
                binding.child_step as usize,
                true,
            )?,
        ];
        match self.pin_merge_journal(binding.child_id, scope.left, binding.child_step as usize) {
            Ok(file) => inputs.push(file),
            Err(OutputAnalysisError::Missing) => {}
            Err(e) => return Err(e),
        }
        let mut digests = Vec::new();
        let mut buffer = vec![0; 64 * 1024];
        for (i, input) in inputs.iter_mut().enumerate() {
            let mut hash = Sha256::new();
            let mut remaining = input.metadata.len();
            while remaining > 0 {
                check(cancel, started)?;
                let n = buffer.len().min(remaining as usize);
                input.file.read_exact(&mut buffer[..n]).map_err(storage)?;
                hash.update(&buffer[..n]);
                remaining -= n as u64;
            }
            input.check_after(if i == 2 {
                MAX_ARCHIVE_JOURNAL_BYTES
            } else {
                self.limit
            })?;
            digests.push(OutputStreamDigest {
                bytes: input.metadata.len(),
                sha256: hash.finalize().into(),
            });
        }
        Ok((
            OutputRetentionEntry {
                binding: binding.clone(),
                stdout: digests[0].clone(),
                stderr: digests[1].clone(),
                journal: digests.get(2).cloned(),
            },
            inputs,
        ))
    }
    fn save_retention(&self, receipt: &OutputRetentionReceipt) -> Result<(), OutputAnalysisError> {
        receipt
            .validate()
            .map_err(|_| OutputAnalysisError::Invalid)?;
        let parent = RuntimePaths::prepare(self.root.join("retention"))
            .map_err(|_| OutputAnalysisError::Storage)?;
        let mut file = tempfile::NamedTempFile::new_in(parent.root()).map_err(storage)?;
        let bytes = serde_json::to_vec(receipt).map_err(|_| OutputAnalysisError::Storage)?;
        file.write_all(&bytes)
            .and_then(|_| file.as_file().sync_all())
            .map_err(storage)?;
        file.persist(parent.root().join(format!("{}.json", receipt.plan.id)))
            .map_err(|e| storage(e.error))?;
        #[cfg(unix)]
        File::open(parent.root())
            .and_then(|f| f.sync_all())
            .map_err(storage)?;
        Ok(())
    }
    pub(crate) fn retention_receipt(
        &self,
        id: TaskRunId,
    ) -> Result<OutputRetentionReceipt, OutputAnalysisError> {
        let root = RuntimePaths::prepare(self.root.join("retention"))
            .map_err(|_| OutputAnalysisError::Storage)?;
        let path = root.root().join(format!("{id}.json"));
        let meta = std::fs::symlink_metadata(&path).map_err(storage)?;
        if !meta.is_file()
            || meta.file_type().is_symlink()
            || meta.len() > MAX_WORKSPACE_DOCUMENT as u64
        {
            return Err(OutputAnalysisError::Invalid);
        }
        let bytes = std::fs::read(path).map_err(storage)?;
        let receipt: OutputRetentionReceipt =
            serde_json::from_slice(&bytes).map_err(|_| OutputAnalysisError::Invalid)?;
        receipt
            .validate()
            .map_err(|_| OutputAnalysisError::Invalid)?;
        if receipt.plan.id != id {
            return Err(OutputAnalysisError::Changed);
        }
        Ok(receipt)
    }
    fn retention_workspace(
        &self,
        request: OutputWorkspaceRequest,
        bindings: Vec<OutputAnalysisBinding>,
        cancel: &AtomicBool,
        progress: &WorkspaceProgress,
        started: Instant,
    ) -> Result<WorkspaceCapture, OutputAnalysisError> {
        let mut entries = Vec::new();
        let mut pins = Vec::new();
        for (source, binding) in request.sources.iter().zip(&bindings) {
            let (entry, files) = self.digest_retention(source, binding, cancel, started)?;
            progress.bytes.fetch_add(
                entry.stdout.bytes
                    + entry.stderr.bytes
                    + entry.journal.as_ref().map_or(0, |j| j.bytes),
                Ordering::Release,
            );
            entries.push(entry);
            pins.push(files);
            progress.sources.fetch_add(1, Ordering::Release);
        }
        let plan = if let Some(plan) = request.retention {
            if plan.entries != entries || now()?.saturating_sub(plan.created_at_ms) > 600_000 {
                return Err(OutputAnalysisError::Changed);
            }
            plan
        } else {
            OutputRetentionPlan {
                format: "cshell.output-retention".into(),
                version: 1,
                id: TaskRunId::new(),
                sources: request.sources,
                entries,
                created_at_ms: now()?,
            }
        };
        let mut result = WorkspaceCapture {
            rows: Vec::new(),
            groups: Vec::new(),
            retention: Some(plan.clone()),
            artifact: None,
            bundle: None,
            truncated: false,
        };
        if request.action == OutputWorkspaceAction::RetentionPreview {
            return Ok(result);
        }
        let parent = RuntimePaths::prepare(self.root.join("retention"))
            .map_err(|_| OutputAnalysisError::Storage)?;
        // Exclusive directory claims this plan before any mutation, including after restart.
        let owned = parent.root().join(plan.id.to_string());
        std::fs::create_dir(&owned).map_err(storage)?;
        RuntimePaths::prepare(owned.clone()).map_err(|_| OutputAnalysisError::Storage)?;
        let mut receipt = OutputRetentionReceipt {
            format: "cshell.output-retention-receipt".into(),
            version: 1,
            plan: plan.clone(),
            states: vec![OutputRetentionState::Pending; plan.sources.len()],
            updated_at_ms: now()?,
        };
        self.save_retention(&receipt)?;
        for (i, (scope, binding)) in plan.sources.iter().zip(&bindings).enumerate() {
            check(cancel, started)?;
            let directory = self
                .directory(binding.child_id, scope.left, binding.child_step as usize)
                .map_err(|_| OutputAnalysisError::Invalid)?;
            let names = std::fs::read_dir(&directory)
                .map_err(storage)?
                .map(|e| e.map(|e| e.file_name()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            if names.iter().any(|name| {
                !matches!(
                    name.to_str(),
                    Some("stdout.bin" | "stderr.bin" | "events.bin")
                )
            }) {
                return Err(OutputAnalysisError::Changed);
            }
            for (j, input) in pins[i].iter().enumerate() {
                input.check_after(if j == 2 {
                    MAX_ARCHIVE_JOURNAL_BYTES
                } else {
                    self.limit
                })?;
            }
            let quarantine = owned.join(i.to_string());
            // Windows readers deny deletion; release the original pins after the
            // last check, then digest the moved private files again before deletion.
            drop(std::mem::take(&mut pins[i]));
            std::fs::rename(&directory, &quarantine).map_err(storage)?;
            receipt.states[i] = OutputRetentionState::Quarantined;
            receipt.updated_at_ms = now()?;
            self.save_retention(&receipt)?;
            // Only the three typed, already digested files are removed; unexpected entries fail closed.
            for name in &names {
                let path = quarantine.join(name);
                let meta = std::fs::symlink_metadata(&path).map_err(storage)?;
                if !meta.is_file() || meta.file_type().is_symlink() {
                    receipt.states[i] = OutputRetentionState::Changed;
                    self.save_retention(&receipt)?;
                    return Err(OutputAnalysisError::Changed);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err(OutputAnalysisError::Changed);
                    }
                }
                let expected = match name.to_str() {
                    Some("stdout.bin") => &plan.entries[i].stdout,
                    Some("stderr.bin") => &plan.entries[i].stderr,
                    Some("events.bin") => plan.entries[i]
                        .journal
                        .as_ref()
                        .ok_or(OutputAnalysisError::Changed)?,
                    _ => return Err(OutputAnalysisError::Changed),
                };
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    options.custom_flags(0x0020_0000).share_mode(1);
                }
                let mut file = options.open(&path).map_err(storage)?;
                let before = file.metadata().map_err(storage)?;
                let mut hash = Sha256::new();
                let mut bytes = 0;
                let mut buffer = vec![0; 64 * 1024];
                loop {
                    check(cancel, started)?;
                    let n = file.read(&mut buffer).map_err(storage)?;
                    if n == 0 {
                        break;
                    }
                    bytes += n as u64;
                    if bytes > expected.bytes {
                        return Err(OutputAnalysisError::Changed);
                    }
                    hash.update(&buffer[..n]);
                }
                let digest: [u8; 32] = hash.finalize().into();
                if bytes != expected.bytes
                    || digest != expected.sha256
                    || !super::analysis::same(&meta, &before)
                    || !super::analysis::same(&before, &file.metadata().map_err(storage)?)
                    || !super::analysis::same(
                        &before,
                        &std::fs::symlink_metadata(&path).map_err(storage)?,
                    )
                {
                    receipt.states[i] = OutputRetentionState::Changed;
                    self.save_retention(&receipt)?;
                    return Err(OutputAnalysisError::Changed);
                }
            }
            drop(std::mem::take(&mut pins[i]));
            for name in names {
                std::fs::remove_file(quarantine.join(name)).map_err(storage)?;
            }
            std::fs::remove_dir(&quarantine).map_err(storage)?;
            receipt.states[i] = OutputRetentionState::Deleted;
            receipt.updated_at_ms = now()?;
            self.save_retention(&receipt)?;
            result.rows.push(OutputWorkspaceRow::Retained {
                source: i,
                removed: true,
            });
        }
        Ok(result)
    }
}
