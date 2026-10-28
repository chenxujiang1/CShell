//! Bounded daemon-owned readonly jobs. Output files, bodies and paths never enter replies.
use super::*;
use crate::task_output::AnalysisProgress;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
#[derive(Debug)]
struct Job {
    document: OutputAnalysisDocument,
    cancel: Arc<AtomicBool>,
    progress: Arc<AnalysisProgress>,
    expires: Instant,
}
#[derive(Clone, Debug)]
pub(super) struct AnalysisJobs {
    jobs: Arc<Mutex<BTreeMap<TaskRunId, Job>>>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for AnalysisJobs {
    fn default() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}
impl AnalysisJobs {
    pub(super) fn slots(&self) -> Arc<tokio::sync::Semaphore> {
        self.slots.clone()
    }
    fn get(
        &self,
        run: TaskRunId,
        id: TaskRunId,
        cancel: bool,
    ) -> Result<OutputAnalysisDocument, TaskStatus> {
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        jobs.retain(|_, job| {
            job.document.state == OutputAnalysisState::Running || job.expires > Instant::now()
        });
        let job = jobs.get(&id).ok_or(TaskStatus::Missing)?;
        if job.document.request.run_id != run {
            return Err(TaskStatus::Invalid);
        }
        if cancel && job.document.state == OutputAnalysisState::Running {
            job.cancel.store(true, Ordering::Release);
        }
        let mut document = job.document.clone();
        document.left_bytes = job.progress.left.load(Ordering::Acquire);
        document.right_bytes = document
            .request
            .right
            .map(|_| job.progress.right.load(Ordering::Acquire));
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        Ok(document)
    }
    fn launch(
        &self,
        request: OutputAnalysisRequest,
        left: OutputAnalysisBinding,
        right: Option<OutputAnalysisBinding>,
        output: TaskOutputStore,
    ) -> Result<OutputAnalysisDocument, TaskStatus> {
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| TaskStatus::Busy)?;
        let document = OutputAnalysisDocument {
            format: "cshell.output-analysis".into(),
            version: 1,
            job_id: TaskRunId::new(),
            request,
            left,
            right,
            state: OutputAnalysisState::Running,
            left_bytes: 0,
            right_bytes: None,
            result: None,
            error: None,
        };
        let mut document = document;
        document.right_bytes = document.request.right.map(|_| 0);
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
                output.analyze_stored_output(
                    (
                        work.left.child_id,
                        work.request.left,
                        work.left.child_step as usize,
                    ),
                    work.right
                        .as_ref()
                        .zip(work.request.right)
                        .map(|(binding, host)| {
                            (binding.child_id, host, binding.child_step as usize)
                        }),
                    work.request.stderr,
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
    pub(super) async fn bind_output_analysis(
        &self,
        analysis: &OutputAnalysisRequest,
    ) -> Result<(OutputAnalysisBinding, Option<OutputAnalysisBinding>), TaskStatus> {
        analysis.validate().map_err(|_| TaskStatus::Invalid)?;
        let run = analysis.run_id;
        Ok(match analysis.kind {
            OutputAnalysisKind::Task => {
                let record = self.load(run).await?;
                if self.control(run)?.is_some()
                    || !task_terminal(record.meta().state)
                    || record.meta().revision != analysis.left_revision
                {
                    return Err(TaskStatus::Conflict);
                }
                let bind = |host: HostId| -> Result<OutputAnalysisBinding, TaskStatus> {
                    let target = record.target(host).ok_or(TaskStatus::Invalid)?;
                    let step = target
                        .steps
                        .get(analysis.step as usize)
                        .ok_or(TaskStatus::Invalid)?;
                    if !target_terminal(target.state) || !target_terminal(step.state) {
                        return Err(TaskStatus::Conflict);
                    }
                    Ok(OutputAnalysisBinding {
                        child_id: run,
                        child_step: analysis.step,
                        snapshot_sha256: *record.snapshot().fingerprint(),
                        target_state: target.state,
                        step_state: step.state,
                    })
                };
                (bind(analysis.left)?, analysis.right.map(bind).transpose()?)
            }
            OutputAnalysisKind::Mixed => {
                let (snapshot, rows) = self.0.mixed.get(run).await.map_err(storage)?;
                if self.0.mixed.is_active(run).map_err(storage)? {
                    return Err(TaskStatus::Conflict);
                }
                let bind = |host: HostId,
                            revision: u64|
                 -> Result<OutputAnalysisBinding, TaskStatus> {
                    let row = rows
                        .iter()
                        .find(|row| row.host_id == host)
                        .ok_or(TaskStatus::Invalid)?;
                    if row.revision != revision || row.state == MixedTaskState::Running {
                        return Err(TaskStatus::Conflict);
                    }
                    let step = row
                        .steps
                        .get(analysis.step as usize)
                        .ok_or(TaskStatus::Invalid)?;
                    let MixedReceipt::Command { target, .. } = &step.receipt else {
                        return Err(TaskStatus::Invalid);
                    };
                    if !target_terminal(target.state) || !target_terminal(target.steps[0].state) {
                        return Err(TaskStatus::Conflict);
                    }
                    let child = snapshot
                        .child(host, analysis.step as usize)
                        .map_err(|_| TaskStatus::Corrupt)?;
                    Ok(OutputAnalysisBinding {
                        child_id: child.id(),
                        child_step: 0,
                        snapshot_sha256: *snapshot.fingerprint(),
                        target_state: target.state,
                        step_state: target.steps[0].state,
                    })
                };
                (
                    bind(analysis.left, analysis.left_revision)?,
                    analysis
                        .right
                        .zip(analysis.right_revision)
                        .map(|(host, revision)| bind(host, revision))
                        .transpose()?,
                )
            }
        })
    }
    pub(super) async fn dispatch_output_analysis(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        canonical.run_id.clone_from(&request.run_id);
        if operation == TaskOperation::OutputAnalyze {
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
        let document = if operation == TaskOperation::OutputAnalyze {
            let analysis = decode_output_analysis_request(&request.definition_json)
                .map_err(|_| TaskStatus::Invalid)?;
            if analysis.run_id != run {
                return Err(TaskStatus::Invalid);
            }
            let (left, right) = self.bind_output_analysis(&analysis).await?;
            self.0
                .analysis
                .launch(analysis, left, right, self.0.output.clone())?
        } else {
            let job = TaskRunId::from_bytes(
                decode_id(&request.result_id).map_err(|_| TaskStatus::Invalid)?,
            );
            self.0
                .analysis
                .get(run, job, operation == TaskOperation::OutputAnalysisCancel)?
        };
        let bytes = serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?;
        if bytes.len() > MAX_OUTPUT_ANALYSIS_DOCUMENT {
            return Err(TaskStatus::Limit);
        }
        Ok(reply!(run_id:request.run_id.clone(),document_json:bytes))
    }
}
