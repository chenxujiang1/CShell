//! Metadata jobs and immutable artifact chunks; capture paths never cross IPC.
use super::*;
use crate::task_output::{AnalysisProgress, ArchiveCapture};
use sha2::{Digest, Sha256};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
const CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(600);
#[derive(Debug)]
struct Job {
    document: OutputArchiveDocument,
    cancel: Arc<AtomicBool>,
    progress: Arc<AnalysisProgress>,
    capture: Option<Arc<ArchiveCapture>>,
    expires: Instant,
}
#[derive(Clone, Debug)]
pub(super) struct ArchiveJobs {
    jobs: Arc<Mutex<BTreeMap<TaskRunId, Job>>>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl ArchiveJobs {
    pub(super) fn new(slots: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            slots,
        }
    }
    fn prune(jobs: &mut BTreeMap<TaskRunId, Job>) {
        jobs.retain(|_, job| {
            job.document.state == OutputAnalysisState::Running || job.expires > Instant::now()
        });
    }
    fn get(
        &self,
        run: TaskRunId,
        id: TaskRunId,
        cancel: bool,
    ) -> Result<OutputArchiveDocument, TaskStatus> {
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        Self::prune(&mut jobs);
        let job = jobs.get(&id).ok_or(TaskStatus::Missing)?;
        if job.document.scope.run_id != run {
            return Err(TaskStatus::Invalid);
        }
        if cancel && job.document.state == OutputAnalysisState::Running {
            job.cancel.store(true, Ordering::Release);
        }
        let mut document = job.document.clone();
        document.bytes = job.progress.left.load(Ordering::Acquire);
        document.source_bytes = job.progress.right.load(Ordering::Acquire);
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        Ok(document)
    }
    fn artifact(&self, run: TaskRunId, id: TaskRunId) -> Result<Arc<ArchiveCapture>, TaskStatus> {
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        Self::prune(&mut jobs);
        let job = jobs
            .values_mut()
            .find(|job| {
                job.capture
                    .as_ref()
                    .is_some_and(|capture| capture.artifact.manifest.archive_id == id)
            })
            .ok_or(TaskStatus::Missing)?;
        if job.document.scope.run_id != run {
            return Err(TaskStatus::Invalid);
        }
        job.expires = Instant::now() + TTL;
        job.capture.clone().ok_or(TaskStatus::Missing)
    }
    fn launch(
        &self,
        scope: OutputAnalysisRequest,
        binding: OutputAnalysisBinding,
        output: TaskOutputStore,
    ) -> Result<OutputArchiveDocument, TaskStatus> {
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| TaskStatus::Busy)?;
        let document = OutputArchiveDocument {
            format: "cshell.output-archive-job".into(),
            version: 1,
            job_id: TaskRunId::new(),
            scope,
            binding,
            state: OutputAnalysisState::Running,
            source_bytes: 0,
            bytes: 0,
            artifact: None,
            error: None,
        };
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        let progress = Arc::new(AnalysisProgress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        Self::prune(&mut jobs);
        if jobs.len() >= 32 {
            let oldest = jobs
                .iter()
                .filter(|(_, job)| job.document.state != OutputAnalysisState::Running)
                .min_by_key(|(_, job)| job.expires)
                .map(|(id, _)| *id)
                .ok_or(TaskStatus::Busy)?;
            jobs.remove(&oldest);
        }
        if jobs.contains_key(&document.job_id) {
            return Err(TaskStatus::Corrupt);
        }
        jobs.insert(
            document.job_id,
            Job {
                document: document.clone(),
                cancel: cancel.clone(),
                progress: progress.clone(),
                capture: None,
                expires: Instant::now() + TTL,
            },
        );
        drop(jobs);
        let state = self.jobs.clone();
        let id = document.job_id;
        let work = document.clone();
        tokio::spawn(async move {
            let stopped = cancel.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                output.capture_archive(work.scope, work.binding, &cancel, &progress)
            })
            .await;
            if let Ok(mut jobs) = state.lock() {
                Self::prune(&mut jobs);
                if !stopped.load(Ordering::Acquire)
                    && let Ok(Ok(capture)) = &result
                {
                    loop {
                        let live = jobs.values().filter_map(|job| job.capture.as_ref()).count();
                        let bytes = jobs
                            .values()
                            .filter_map(|job| job.capture.as_ref())
                            .map(|capture| capture.artifact.bytes)
                            .sum::<u64>();
                        if live < 4 && bytes + capture.artifact.bytes <= CACHE_BYTES {
                            break;
                        }
                        let oldest = jobs
                            .iter()
                            .filter(|(_, job)| job.capture.is_some())
                            .min_by_key(|(_, job)| job.expires)
                            .map(|(id, _)| *id);
                        if let Some(oldest) = oldest {
                            if let Some(job) = jobs.get_mut(&oldest) {
                                job.capture = None;
                            }
                        } else {
                            break;
                        }
                    }
                }
                if let Some(job) = jobs.get_mut(&id) {
                    if stopped.load(Ordering::Acquire) {
                        job.document.state = OutputAnalysisState::Cancelled;
                    } else {
                        match result {
                            Ok(Ok(capture)) => {
                                job.document.state = OutputAnalysisState::Completed;
                                job.document.artifact = Some(capture.artifact.clone());
                                job.capture = Some(Arc::new(capture));
                            }
                            Ok(Err(error)) => {
                                job.document.state = OutputAnalysisState::Failed;
                                job.document.error = Some(error);
                            }
                            Err(_) => {
                                job.document.state = OutputAnalysisState::Failed;
                                job.document.error = Some(OutputAnalysisError::Worker);
                            }
                        }
                    }
                    job.expires = Instant::now() + TTL;
                }
            }
        });
        Ok(document)
    }
}
impl TaskIpcService {
    pub(super) async fn dispatch_output_archive(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        canonical.run_id.clone_from(&request.run_id);
        if operation == TaskOperation::OutputArchive {
            canonical
                .definition_json
                .clone_from(&request.definition_json);
        } else {
            canonical.result_id.clone_from(&request.result_id);
            if operation == TaskOperation::OutputArchiveRead {
                canonical.offset = request.offset;
                canonical.limit = request.limit;
            }
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        let run =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        if operation == TaskOperation::OutputArchiveRead {
            if request.limit == 0 || request.limit as usize > MAX_OUTPUT_ARCHIVE_CHUNK {
                return Err(TaskStatus::Invalid);
            }
            let id = TaskRunId::from_bytes(
                decode_id(&request.result_id).map_err(|_| TaskStatus::Invalid)?,
            );
            let capture = self.0.archives.artifact(run, id)?;
            let slot = self
                .0
                .analysis
                .slots()
                .try_acquire_owned()
                .map_err(|_| TaskStatus::Busy)?;
            let offset = request.offset;
            let limit = request.limit as usize;
            let (artifact, body) = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let body = capture.read(offset, limit)?;
                Ok::<_, OutputAnalysisError>((capture.artifact.clone(), body))
            })
            .await
            .map_err(|_| TaskStatus::Unavailable)?
            .map_err(|error| match error {
                OutputAnalysisError::Invalid => TaskStatus::Invalid,
                OutputAnalysisError::Changed => TaskStatus::Conflict,
                _ => TaskStatus::Unavailable,
            })?;
            let chunk = OutputArchiveChunk {
                format: "cshell.output-archive-chunk".into(),
                version: 1,
                archive_id: id,
                run_id: run,
                archive_sha256: artifact.sha256,
                offset,
                bytes: body.len(),
                next: offset + body.len() as u64,
                total_bytes: artifact.bytes,
                chunk_sha256: Sha256::digest(&body).into(),
            };
            chunk
                .validate(&artifact, &body)
                .map_err(|_| TaskStatus::Corrupt)?;
            return Ok(
                reply!(run_id:request.run_id.clone(),document_json:serde_json::to_vec(&chunk).map_err(|_|TaskStatus::Corrupt)?,output_length:body.len()as u64,output:body),
            );
        }
        let document = if operation == TaskOperation::OutputArchive {
            let scope = decode_output_analysis_request(&request.definition_json)
                .map_err(|_| TaskStatus::Invalid)?;
            if scope.run_id != run || scope.right.is_some() || scope.stderr {
                return Err(TaskStatus::Invalid);
            }
            let (binding, right) = self.bind_output_analysis(&scope).await?;
            if right.is_some() {
                return Err(TaskStatus::Invalid);
            }
            self.0
                .archives
                .launch(scope, binding, self.0.output.clone())?
        } else {
            let id = TaskRunId::from_bytes(
                decode_id(&request.result_id).map_err(|_| TaskStatus::Invalid)?,
            );
            self.0
                .archives
                .get(run, id, operation == TaskOperation::OutputArchiveCancel)?
        };
        Ok(
            reply!(run_id:request.run_id.clone(),document_json:serde_json::to_vec(&document).map_err(|_|TaskStatus::Corrupt)?),
        )
    }
}
