//! Bounded readonly search jobs share the full-output I/O worker budget.
use super::*;
use crate::task_output::AnalysisProgress;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
#[derive(Debug)]
struct Job {
    document: OutputSearchDocument,
    cancel: Arc<AtomicBool>,
    progress: Arc<AnalysisProgress>,
    expires: Instant,
}
#[derive(Clone, Debug)]
pub(super) struct SearchJobs {
    jobs: Arc<Mutex<BTreeMap<TaskRunId, Job>>>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl SearchJobs {
    pub(super) fn new(slots: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            slots,
        }
    }
    fn get(
        &self,
        run: TaskRunId,
        id: TaskRunId,
        cancel: bool,
    ) -> Result<OutputSearchDocument, TaskStatus> {
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        jobs.retain(|_, job| {
            job.document.state == OutputAnalysisState::Running || job.expires > Instant::now()
        });
        let job = jobs.get(&id).ok_or(TaskStatus::Missing)?;
        if job.document.request.scope.run_id != run {
            return Err(TaskStatus::Invalid);
        }
        if cancel && job.document.state == OutputAnalysisState::Running {
            job.cancel.store(true, Ordering::Release);
        }
        let mut document = job.document.clone();
        document.bytes = job.progress.left.load(Ordering::Acquire);
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        Ok(document)
    }
    fn launch(
        &self,
        request: OutputSearchRequest,
        binding: OutputAnalysisBinding,
        output: TaskOutputStore,
    ) -> Result<OutputSearchDocument, TaskStatus> {
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| TaskStatus::Busy)?;
        let descriptor = request.descriptor().map_err(|_| TaskStatus::Invalid)?;
        let document = OutputSearchDocument {
            format: "cshell.output-search".into(),
            version: 1,
            job_id: TaskRunId::new(),
            request: descriptor,
            binding,
            state: OutputAnalysisState::Running,
            bytes: 0,
            result: None,
            error: None,
        };
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AnalysisProgress::default());
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        jobs.retain(|_, job| {
            job.document.state == OutputAnalysisState::Running || job.expires > Instant::now()
        });
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
                expires: Instant::now() + Duration::from_secs(600),
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
                output.search_stored_output(
                    (
                        work.binding.child_id,
                        work.request.scope.left,
                        work.binding.child_step as usize,
                    ),
                    &request,
                    &cancel,
                    &progress,
                )
            })
            .await;
            if let Ok(mut jobs) = state.lock()
                && let Some(job) = jobs.get_mut(&id)
            {
                if stopped.load(Ordering::Acquire) {
                    job.document.state = OutputAnalysisState::Cancelled;
                } else {
                    match result {
                        Ok(Ok(result)) => {
                            job.document.state = OutputAnalysisState::Completed;
                            job.document.result = Some(result);
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
                job.expires = Instant::now() + Duration::from_secs(600);
            }
        });
        Ok(document)
    }
}
impl TaskIpcService {
    pub(super) async fn dispatch_output_search(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        canonical.run_id.clone_from(&request.run_id);
        if operation == TaskOperation::OutputSearch {
            canonical
                .definition_json
                .clone_from(&request.definition_json);
        } else {
            canonical.result_id.clone_from(&request.result_id);
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        let run =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        let document = if operation == TaskOperation::OutputSearch {
            let query = decode_output_search_request(&request.definition_json)
                .map_err(|_| TaskStatus::Invalid)?;
            if query.scope.run_id != run {
                return Err(TaskStatus::Invalid);
            }
            let (binding, right) = self.bind_output_analysis(&query.scope).await?;
            if right.is_some() {
                return Err(TaskStatus::Invalid);
            }
            self.0
                .search
                .launch(query, binding, self.0.output.clone())?
        } else {
            let job = TaskRunId::from_bytes(
                decode_id(&request.result_id).map_err(|_| TaskStatus::Invalid)?,
            );
            self.0
                .search
                .get(run, job, operation == TaskOperation::OutputSearchCancel)?
        };
        let bytes = serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?;
        if bytes.len() > MAX_OUTPUT_ANALYSIS_DOCUMENT {
            return Err(TaskStatus::Limit);
        }
        Ok(reply!(run_id:request.run_id.clone(),document_json:bytes))
    }
}
