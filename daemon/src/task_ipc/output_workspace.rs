use super::*;
use crate::task_output::{WorkspaceBundle, WorkspaceCapture, WorkspaceProgress};
use sha2::{Digest, Sha256};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
struct Job {
    document: OutputWorkspaceDocument,
    cancel: Arc<AtomicBool>,
    progress: Arc<WorkspaceProgress>,
    rows: Arc<Vec<OutputWorkspaceRow>>,
    bundle: Option<Arc<WorkspaceBundle>>,
    expires: Instant,
}
#[derive(Clone)]
pub(super) struct WorkspaceJobs {
    jobs: Arc<Mutex<BTreeMap<TaskRunId, Job>>>,
    slots: Arc<tokio::sync::Semaphore>,
    bundle_slots: Arc<tokio::sync::Semaphore>,
}
impl std::fmt::Debug for WorkspaceJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceJobs").finish_non_exhaustive()
    }
}
impl WorkspaceJobs {
    pub(super) fn new(slots: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
            slots,
            bundle_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    fn launch(
        &self,
        request: OutputWorkspaceRequest,
        bindings: Vec<OutputAnalysisBinding>,
        output: TaskOutputStore,
    ) -> Result<OutputWorkspaceDocument, TaskStatus> {
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| TaskStatus::Busy)?;
        let bundle_slot = if request.action == OutputWorkspaceAction::Bundle {
            Some(
                self.bundle_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| TaskStatus::Busy)?,
            )
        } else {
            None
        };
        let document = OutputWorkspaceDocument {
            format: "cshell.output-workspace".into(),
            version: 1,
            job_id: TaskRunId::new(),
            action: request.action,
            sources: request.sources.clone(),
            bindings: bindings.clone(),
            query_sha256: request
                .query
                .as_ref()
                .map(|q| Sha256::digest(q.as_bytes()).into()),
            state: OutputAnalysisState::Running,
            completed_sources: 0,
            bytes: 0,
            rows: 0,
            result_sha256: None,
            groups: Vec::new(),
            retention: None,
            artifact: None,
            truncated: false,
            error: None,
        };
        document.validate().map_err(|_| TaskStatus::Invalid)?;
        if serde_json::to_vec(&document)
            .map_err(|_| TaskStatus::Corrupt)?
            .len()
            > MAX_WORKSPACE_DOCUMENT
        {
            return Err(TaskStatus::Limit);
        }
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        jobs.retain(|_, j| {
            j.document.state == OutputAnalysisState::Running || j.expires > Instant::now()
        });
        // At most one retained bundle and eight bounded result sets.
        if request.action == OutputWorkspaceAction::Bundle {
            jobs.retain(|_, j| {
                j.document.action != OutputWorkspaceAction::Bundle
                    || j.document.state == OutputAnalysisState::Running
            });
        }
        if jobs.len() >= 8 {
            let id = jobs
                .iter()
                .filter(|(_, j)| j.document.state != OutputAnalysisState::Running)
                .min_by_key(|(_, j)| j.expires)
                .map(|(id, _)| *id)
                .ok_or(TaskStatus::Busy)?;
            jobs.remove(&id);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(WorkspaceProgress::default());
        jobs.insert(
            document.job_id,
            Job {
                document: document.clone(),
                cancel: cancel.clone(),
                progress: progress.clone(),
                rows: Arc::new(Vec::new()),
                bundle: None,
                expires: Instant::now() + Duration::from_secs(600),
            },
        );
        drop(jobs);
        let jobs = self.jobs.clone();
        let id = document.job_id;
        let cancelled = cancel.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let _bundle_slot = bundle_slot;
                output.run_workspace(request, bindings, &cancel, &progress)
            })
            .await;
            if let Ok(mut jobs) = jobs.lock()
                && let Some(job) = jobs.get_mut(&id)
            {
                // Retention may already have made durable changes when cancellation wins.
                match result {
                    Ok(Ok(WorkspaceCapture {
                        rows,
                        groups,
                        retention,
                        artifact,
                        bundle,
                        truncated,
                    })) => match serde_json::to_vec(&rows) {
                        Ok(bytes) if bytes.len() <= MAX_WORKSPACE_RESULT => {
                            job.document.state = OutputAnalysisState::Completed;
                            job.document.rows = rows.len();
                            job.document.result_sha256 = Some(Sha256::digest(&bytes).into());
                            job.document.groups = groups;
                            job.document.retention = retention;
                            job.document.artifact = artifact;
                            job.document.truncated = truncated;
                            job.rows = Arc::new(rows);
                            job.bundle = bundle.map(Arc::new);
                        }
                        _ => {
                            job.document.state = OutputAnalysisState::Failed;
                            job.document.error = Some(OutputAnalysisError::Invalid);
                        }
                    },
                    _ if cancelled.load(Ordering::Acquire) => {
                        job.document.state = OutputAnalysisState::Cancelled
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
                job.expires = Instant::now() + Duration::from_secs(600);
                if serde_json::to_vec(&job.document)
                    .map_or(true, |bytes| bytes.len() > MAX_WORKSPACE_DOCUMENT)
                {
                    job.document.state = OutputAnalysisState::Failed;
                    job.document.error = Some(OutputAnalysisError::Invalid);
                    job.document.result_sha256 = None;
                    job.document.artifact = None;
                    job.document.retention = None;
                    job.document.groups.clear();
                    job.document.rows = 0;
                    job.rows = Arc::new(Vec::new());
                    job.bundle = None;
                }
            }
        });
        Ok(document)
    }
    fn get(
        &self,
        run: TaskRunId,
        id: TaskRunId,
        cancel: bool,
    ) -> Result<OutputWorkspaceDocument, TaskStatus> {
        let mut jobs = self.jobs.lock().map_err(|_| TaskStatus::Unavailable)?;
        jobs.retain(|_, j| {
            j.document.state == OutputAnalysisState::Running || j.expires > Instant::now()
        });
        let job = jobs.get(&id).ok_or(TaskStatus::Missing)?;
        if job.document.sources[0].run_id != run {
            return Err(TaskStatus::Invalid);
        }
        if cancel {
            job.cancel.store(true, Ordering::Release);
        }
        let mut document = job.document.clone();
        document.bytes = job.progress.bytes.load(Ordering::Acquire);
        document.completed_sources = job.progress.sources.load(Ordering::Acquire) as usize;
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        Ok(document)
    }
}
impl TaskIpcService {
    async fn bind_workspace_sources(
        &self,
        scopes: &[OutputAnalysisRequest],
    ) -> Result<Vec<OutputAnalysisBinding>, TaskStatus> {
        let first = scopes.first().ok_or(TaskStatus::Invalid)?;
        match first.kind {
            OutputAnalysisKind::Task => {
                let record = self.load(first.run_id).await?;
                if self.control(first.run_id)?.is_some() || !task_terminal(record.meta().state) {
                    return Err(TaskStatus::Conflict);
                }
                scopes
                    .iter()
                    .map(|scope| {
                        if scope.left_revision != record.meta().revision {
                            return Err(TaskStatus::Conflict);
                        }
                        let target = record.target(scope.left).ok_or(TaskStatus::Invalid)?;
                        let step = target
                            .steps
                            .get(scope.step as usize)
                            .ok_or(TaskStatus::Invalid)?;
                        if !target_terminal(target.state) || !target_terminal(step.state) {
                            return Err(TaskStatus::Conflict);
                        }
                        Ok(OutputAnalysisBinding {
                            child_id: first.run_id,
                            child_step: scope.step,
                            snapshot_sha256: *record.snapshot().fingerprint(),
                            target_state: target.state,
                            step_state: step.state,
                        })
                    })
                    .collect()
            }
            OutputAnalysisKind::Mixed => {
                let (snapshot, rows) = self.0.mixed.get(first.run_id).await.map_err(storage)?;
                if self.0.mixed.is_active(first.run_id).map_err(storage)? {
                    return Err(TaskStatus::Conflict);
                }
                let rows: BTreeMap<_, _> = rows.iter().map(|row| (row.host_id, row)).collect();
                scopes
                    .iter()
                    .map(|scope| {
                        let row = rows.get(&scope.left).ok_or(TaskStatus::Invalid)?;
                        if row.revision != scope.left_revision
                            || row.state == MixedTaskState::Running
                        {
                            return Err(TaskStatus::Conflict);
                        }
                        let step = row
                            .steps
                            .get(scope.step as usize)
                            .ok_or(TaskStatus::Invalid)?;
                        let MixedReceipt::Command { target, .. } = &step.receipt else {
                            return Err(TaskStatus::Invalid);
                        };
                        if !target_terminal(target.state) || !target_terminal(target.steps[0].state)
                        {
                            return Err(TaskStatus::Conflict);
                        }
                        Ok(OutputAnalysisBinding {
                            child_id: snapshot
                                .child(scope.left, scope.step as usize)
                                .map_err(|_| TaskStatus::Corrupt)?
                                .id(),
                            child_step: 0,
                            snapshot_sha256: *snapshot.fingerprint(),
                            target_state: target.state,
                            step_state: target.steps[0].state,
                        })
                    })
                    .collect()
            }
        }
    }
    pub(super) async fn dispatch_workspace(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let run =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        canonical.run_id.clone_from(&request.run_id);
        if operation == TaskOperation::OutputIndexRead {
            canonical
                .definition_json
                .clone_from(&request.definition_json);
            canonical
                .expected_targets_json
                .clone_from(&request.expected_targets_json);
            canonical.offset = request.offset;
            if canonical != *request {
                return Err(TaskStatus::Invalid);
            }
            let scope = decode_output_analysis_request(&request.definition_json)
                .map_err(|_| TaskStatus::Invalid)?;
            let entry: OutputIndexEntry = serde_json::from_slice(&request.expected_targets_json)
                .map_err(|_| TaskStatus::Invalid)?;
            if scope.run_id != run
                || scope.right.is_some()
                || entry.binding != self.bind_output_analysis(&scope).await?.0
            {
                return Err(TaskStatus::Conflict);
            }
            let output = self.0.output.clone();
            let offset = usize::try_from(request.offset).map_err(|_| TaskStatus::Invalid)?;
            let page =
                tokio::task::spawn_blocking(move || output.workspace_index(scope, entry, offset))
                    .await
                    .map_err(|_| TaskStatus::Unavailable)?
                    .map_err(|_| TaskStatus::Missing)?;
            return Ok(
                reply!(run_id: request.run_id.clone(), document_json: serde_json::to_vec(&page).map_err(|_| TaskStatus::Corrupt)?),
            );
        }
        let mut body = Vec::new();
        let document_json = if operation == TaskOperation::OutputWorkspace {
            canonical
                .definition_json
                .clone_from(&request.definition_json);
            canonical.confirmed = request.confirmed;
            if canonical != *request {
                return Err(TaskStatus::Invalid);
            }
            let definition = decode_output_workspace_request(&request.definition_json)
                .map_err(|_| TaskStatus::Invalid)?;
            if definition.sources[0].run_id != run
                || (definition.action == OutputWorkspaceAction::RetentionApply) != request.confirmed
            {
                return Err(TaskStatus::Invalid);
            }
            let bindings = self.bind_workspace_sources(&definition.sources).await?;
            let document = self
                .0
                .workspace
                .launch(definition, bindings, self.0.output.clone())?;
            serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?
        } else {
            canonical.result_id.clone_from(&request.result_id);
            if matches!(
                operation,
                TaskOperation::OutputWorkspacePage | TaskOperation::OutputWorkspaceRead
            ) {
                canonical.offset = request.offset;
                canonical
                    .expected_targets_json
                    .clone_from(&request.expected_targets_json);
            }
            if canonical != *request {
                return Err(TaskStatus::Invalid);
            }
            let id = TaskRunId::from_bytes(
                decode_id(&request.result_id).map_err(|_| TaskStatus::Invalid)?,
            );
            if operation == TaskOperation::OutputRetentionReceipt {
                let output = self.0.output.clone();
                let receipt = tokio::task::spawn_blocking(move || output.retention_receipt(id))
                    .await
                    .map_err(|_| TaskStatus::Unavailable)?
                    .map_err(|_| TaskStatus::Missing)?;
                if receipt.plan.sources[0].run_id != run {
                    return Err(TaskStatus::Invalid);
                }
                serde_json::to_vec(&receipt).map_err(|_| TaskStatus::Corrupt)?
            } else {
                let document = self.0.workspace.get(
                    run,
                    id,
                    operation == TaskOperation::OutputWorkspaceCancel,
                )?;
                if matches!(
                    operation,
                    TaskOperation::OutputWorkspacePage | TaskOperation::OutputWorkspaceRead
                ) {
                    let expected = decode_output_workspace(&request.expected_targets_json)
                        .map_err(|_| TaskStatus::Invalid)?;
                    if expected != document || document.state != OutputAnalysisState::Completed {
                        return Err(TaskStatus::Conflict);
                    }
                    let (rows, bundle) = {
                        let jobs = self
                            .0
                            .workspace
                            .jobs
                            .lock()
                            .map_err(|_| TaskStatus::Unavailable)?;
                        let job = jobs.get(&id).ok_or(TaskStatus::Missing)?;
                        (job.rows.clone(), job.bundle.clone())
                    };
                    if operation == TaskOperation::OutputWorkspaceRead {
                        let bundle = bundle.ok_or(TaskStatus::Invalid)?;
                        let offset = request.offset;
                        body = tokio::task::spawn_blocking(move || {
                            bundle.read(offset, MAX_OUTPUT_ARCHIVE_CHUNK)
                        })
                        .await
                        .map_err(|_| TaskStatus::Unavailable)?
                        .map_err(|_| TaskStatus::Unavailable)?;
                        serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?
                    } else {
                        let offset =
                            usize::try_from(request.offset).map_err(|_| TaskStatus::Invalid)?;
                        if offset > rows.len() {
                            return Err(TaskStatus::Invalid);
                        }
                        // Text lines can be large; keep each page under the metadata limit.
                        let mut page_rows = Vec::new();
                        let mut bytes = 0;
                        for row in rows.iter().skip(offset).take(WORKSPACE_PAGE_ROWS) {
                            let size = serde_json::to_vec(row)
                                .map_err(|_| TaskStatus::Corrupt)?
                                .len();
                            if bytes + size > MAX_WORKSPACE_DOCUMENT - 4096 && !page_rows.is_empty()
                            {
                                break;
                            }
                            if size > MAX_WORKSPACE_DOCUMENT - 4096 {
                                return Err(TaskStatus::Limit);
                            }
                            bytes += size;
                            page_rows.push(row.clone());
                        }
                        let page = OutputWorkspacePage {
                            format: "cshell.output-workspace-page".into(),
                            version: 1,
                            job_id: id,
                            result_sha256: document.result_sha256.ok_or(TaskStatus::Corrupt)?,
                            offset,
                            next: offset + page_rows.len(),
                            total: rows.len(),
                            rows: page_rows,
                        };
                        page.validate(&document).map_err(|_| TaskStatus::Corrupt)?;
                        serde_json::to_vec(&page).map_err(|_| TaskStatus::Corrupt)?
                    }
                } else {
                    serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?
                }
            }
        };
        if document_json.len() > MAX_WORKSPACE_DOCUMENT {
            return Err(TaskStatus::Limit);
        }
        Ok(
            reply!(run_id: request.run_id.clone(), document_json: document_json, output_length: body.len() as u64, output: body),
        )
    }
}
