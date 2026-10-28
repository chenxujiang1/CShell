use crate::{ProfileIpcService, SshTaskExecutor, TaskOutputStore};
use cshell_application::*;
use cshell_domain::{HostCatalog, HostId, TaskDefinition, TaskGateId, TaskRunId, TaskState};
use cshell_ipc::{TaskOperation, TaskRequest, TaskResponse, TaskStatus, decode_id};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
#[cfg(test)]
mod tests;

#[derive(Debug)]
struct DaemonTaskRepository(Arc<ProfileIpcService>);
#[async_trait::async_trait]
impl TaskRepository for DaemonTaskRepository {
    async fn create_task_run(&self, record: &TaskRunRecord) -> Result<(), TaskRepositoryError> {
        self.0.service.repository().create_task_run(record).await
    }
    async fn load_task_run(
        &self,
        id: TaskRunId,
    ) -> Result<Option<TaskRunRecord>, TaskRepositoryError> {
        self.0.service.repository().load_task_run(id).await
    }
    async fn commit_task_patch(&self, patch: &TaskRunPatch) -> Result<u64, TaskRepositoryError> {
        self.0.service.repository().commit_task_patch(patch).await
    }
    async fn list_task_runs(
        &self,
        after: Option<TaskRunId>,
        limit: usize,
        unfinished_only: bool,
    ) -> Result<Vec<TaskRunSummary>, TaskRepositoryError> {
        self.0
            .service
            .repository()
            .list_task_runs(after, limit, unfinished_only)
            .await
    }
}
#[derive(Debug)]
struct TaskServiceInner {
    profiles: Arc<ProfileIpcService>,
    repository: Arc<dyn TaskRepository>,
    scheduler: Arc<TaskScheduler>,
    budget: TaskResourceBudget,
    uploads: crate::BatchUploadService,
    downloads: crate::BatchDownloadService,
    mixed: crate::MixedTaskService,
    mixed_results: mixed_results::ResultCache,
    analysis: output_analysis::AnalysisJobs,
    search: output_search::SearchJobs,
    archives: output_archive::ArchiveJobs,
    workspace: output_workspace::WorkspaceJobs,
    output: TaskOutputStore,
    clock: Arc<SystemTaskClock>,
    active: Mutex<BTreeMap<TaskRunId, TaskRunControl>>,
    operations: tokio::sync::Mutex<()>,
}
#[derive(Clone, Debug)]
pub struct TaskIpcService(Arc<TaskServiceInner>);
macro_rules! reply {
    ($($field:ident: $value:expr),* $(,)?) => {{
        let mut result = TaskResponse::default();
        $(result.$field = $value;)*
        result
    }};
}
mod downloads;
mod mixed;
mod mixed_results;
mod output_analysis;
mod output_archive;
mod output_merge;
mod output_search;
mod output_workspace;
pub(crate) fn response(status: TaskStatus) -> TaskResponse {
    reply!(status: status as i32)
}
fn storage(error: TaskRepositoryError) -> TaskStatus {
    match error {
        TaskRepositoryError::Conflict => TaskStatus::Conflict,
        TaskRepositoryError::Missing => TaskStatus::Missing,
        TaskRepositoryError::Corrupt => TaskStatus::Corrupt,
        TaskRepositoryError::Unavailable => TaskStatus::Unavailable,
    }
}
impl TaskIpcService {
    pub(crate) async fn requires_publication_feature(
        &self,
        request: &TaskRequest,
    ) -> Result<bool, TaskStatus> {
        let operation =
            TaskOperation::try_from(request.operation).map_err(|_| TaskStatus::Invalid)?;
        if matches!(
            operation,
            TaskOperation::UploadList | TaskOperation::DownloadList | TaskOperation::MixedList
        ) {
            return Ok(false);
        }
        if matches!(
            operation,
            TaskOperation::UploadCreate
                | TaskOperation::DownloadCreate
                | TaskOperation::MixedCreate
        ) {
            return Ok(definition_uses_file_publication(&request.definition_json));
        }
        if request.run_id.is_empty()
            || !(operation.is_upload() || operation.is_download() || operation.is_mixed())
        {
            return Ok(false);
        }
        let id =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        let repository = self.0.profiles.service.repository();
        if operation.is_upload() {
            return repository
                .load_upload(id)
                .await
                .map_err(|_| TaskStatus::Unavailable)?
                .map(|(snapshot, _)| snapshot.definition.publication_policy.is_some())
                .ok_or(TaskStatus::Missing);
        }
        if operation.is_download() {
            return repository
                .load_download(id)
                .await
                .map_err(storage)?
                .map(|(snapshot, _)| snapshot.definition.publication_policy.is_some())
                .ok_or(TaskStatus::Missing);
        }
        repository
            .load_mixed_task(id)
            .await
            .map_err(storage)?
            .map(|(snapshot, _)| {
                snapshot.definition().steps.iter().any(|step| match step {
                    MixedTaskAction::Upload { definition } => {
                        definition.publication_policy.is_some()
                    }
                    MixedTaskAction::Download { definition } => {
                        definition.publication_policy.is_some()
                    }
                    _ => false,
                })
            })
            .ok_or(TaskStatus::Missing)
    }
    pub fn new(
        profiles: Arc<ProfileIpcService>,
        output: TaskOutputStore,
    ) -> Result<Self, TaskError> {
        let executor = Arc::new(SshTaskExecutor::new(profiles.clone(), output.clone()));
        Self::with_executor(profiles, output, executor)
    }
    pub fn with_executor(
        profiles: Arc<ProfileIpcService>,
        output: TaskOutputStore,
        executor: Arc<dyn TaskExecutor>,
    ) -> Result<Self, TaskError> {
        let repository: Arc<dyn TaskRepository> = Arc::new(DaemonTaskRepository(profiles.clone()));
        let clock = Arc::new(SystemTaskClock::new()?);
        let budget = TaskResourceBudget::new(TaskResourceLimits::default())?;
        let scheduler = Arc::new(TaskScheduler::new(
            repository.clone(),
            executor.clone(),
            budget.clone(),
            clock.clone(),
        ));
        let uploads = crate::BatchUploadService::new(profiles.clone(), budget.clone())?;
        let downloads = crate::BatchDownloadService::new(profiles.clone(), budget.clone())?;
        let mixed = crate::MixedTaskService::new(profiles.clone(), executor, budget.clone())?;
        let analysis = output_analysis::AnalysisJobs::default();
        let search = output_search::SearchJobs::new(analysis.slots());
        let archives = output_archive::ArchiveJobs::new(analysis.slots());
        let workspace = output_workspace::WorkspaceJobs::new(analysis.slots());
        Ok(Self(Arc::new(TaskServiceInner {
            profiles,
            repository,
            scheduler,
            budget,
            uploads,
            downloads,
            mixed,
            mixed_results: mixed_results::ResultCache::default(),
            analysis,
            search,
            archives,
            workspace,
            output,
            clock,
            active: Mutex::new(BTreeMap::new()),
            operations: tokio::sync::Mutex::new(()),
        })))
    }
    fn control(&self, id: TaskRunId) -> Result<Option<TaskRunControl>, TaskStatus> {
        Ok(self
            .0
            .active
            .lock()
            .map_err(|_| TaskStatus::Unavailable)?
            .get(&id)
            .cloned())
    }
    #[must_use]
    pub fn resource_budget(&self) -> TaskResourceBudget {
        self.0.budget.clone()
    }
    #[must_use]
    pub fn mixed_task_service(&self) -> crate::MixedTaskService {
        self.0.mixed.clone()
    }
    async fn commit(
        &self,
        record: &mut TaskRunRecord,
        action: TaskRunAction,
    ) -> Result<(), TaskStatus> {
        let patch = record
            .prepare(
                action,
                self.0.clock.now_ms().map_err(|_| TaskStatus::Unavailable)?,
            )
            .map_err(|_| TaskStatus::Conflict)?;
        let revision = self
            .0
            .repository
            .commit_task_patch(&patch)
            .await
            .map_err(storage)?;
        record
            .apply_committed(&patch, revision)
            .map_err(|_| TaskStatus::Corrupt)
    }
    async fn load(&self, id: TaskRunId) -> Result<TaskRunRecord, TaskStatus> {
        let mut record = self
            .0
            .repository
            .load_task_run(id)
            .await
            .map_err(storage)?
            .ok_or(TaskStatus::Missing)?;
        if self.control(id)?.is_none() && !task_terminal(record.meta().state) {
            if matches!(
                record.meta().state,
                TaskState::Running | TaskState::Cancelling
            ) || record.targets().iter().any(|t| {
                matches!(
                    t.state,
                    cshell_domain::TargetState::Connecting | cshell_domain::TargetState::Running
                )
            }) {
                self.commit(&mut record, TaskRunAction::Recover).await?;
            }
            if record.targets().iter().all(|t| target_terminal(t.state)) {
                self.commit(&mut record, TaskRunAction::Finish).await?;
            }
        }
        Ok(record)
    }
    fn view(&self, record: &TaskRunRecord) -> Result<TaskResponse, TaskStatus> {
        // Status queries expose states and codes, without copying frozen command bodies.
        let document = TaskRunView {
            id: record.id(),
            name: record.snapshot().definition().name.clone(),
            meta: record.meta().clone(),
            targets: record.targets().to_vec(),
            runner_active: self.control(record.id())?.is_some(),
        };
        let document_json = serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?;
        if document_json.len() > 12 * 1024 * 1024 {
            return Err(TaskStatus::Limit);
        }
        Ok(
            reply!(run_id: record.id().as_uuid().as_bytes().to_vec(), revision: record.meta().revision, document_json: document_json),
        )
    }
    async fn dispatch(&self, request: &TaskRequest) -> Result<TaskResponse, TaskStatus> {
        let operation =
            TaskOperation::try_from(request.operation).map_err(|_| TaskStatus::Invalid)?;
        if operation.is_output_analysis() {
            return self.dispatch_output_analysis(request, operation).await;
        }
        if operation.is_output_search() {
            return self.dispatch_output_search(request, operation).await;
        }
        if operation.is_output_merge() {
            return self.dispatch_output_merge(request).await;
        }
        if operation.is_output_archive() {
            return self.dispatch_output_archive(request, operation).await;
        }
        if operation.is_mixed() {
            return self.dispatch_mixed(request, operation).await;
        }
        if operation.is_download() {
            return self.dispatch_download(request, operation).await;
        }
        if matches!(
            operation,
            TaskOperation::UploadCreate
                | TaskOperation::UploadGet
                | TaskOperation::UploadStart
                | TaskOperation::UploadCancel
                | TaskOperation::UploadResume
                | TaskOperation::UploadProbe
                | TaskOperation::UploadCleanup
                | TaskOperation::UploadAdopt
                | TaskOperation::UploadList
        ) {
            return self.dispatch_upload(request, operation).await;
        }
        // Every operation has one shape. Reject irrelevant fields, never reinterpret.
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        match operation {
            TaskOperation::Create => {
                canonical.host_revision = request.host_revision;
                canonical.profile_revision = request.profile_revision;
                canonical
                    .definition_json
                    .clone_from(&request.definition_json);
                canonical.selection_json.clone_from(&request.selection_json);
            }
            TaskOperation::List => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.limit = request.limit;
            }
            TaskOperation::Get | TaskOperation::Review => {
                canonical.run_id.clone_from(&request.run_id);
            }
            TaskOperation::Results => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.expected_revision = request.expected_revision;
            }
            TaskOperation::Compare => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.expected_revision = request.expected_revision;
                canonical.host_id.clone_from(&request.host_id);
                canonical
                    .compare_host_id
                    .clone_from(&request.compare_host_id);
                canonical.step = request.step;
                canonical.stderr = request.stderr;
                canonical.offset = request.offset;
                canonical.limit = request.limit;
            }
            TaskOperation::Start | TaskOperation::Approve => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.expected_revision = request.expected_revision;
                if operation != TaskOperation::Cancel {
                    canonical.confirmed = request.confirmed;
                }
                if operation == TaskOperation::Approve {
                    canonical.gate_id.clone_from(&request.gate_id);
                }
            }
            TaskOperation::Cancel => {
                canonical.run_id.clone_from(&request.run_id);
            }
            TaskOperation::Output => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.host_id.clone_from(&request.host_id);
                canonical.step = request.step;
                canonical.stderr = request.stderr;
                canonical.offset = request.offset;
                canonical.limit = request.limit;
            }
            TaskOperation::Retry => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.expected_revision = request.expected_revision;
                canonical.confirmed = request.confirmed;
                canonical.retry_hosts.clone_from(&request.retry_hosts);
                canonical.confirm_unknown = request.confirm_unknown;
            }
            TaskOperation::UploadCreate
            | TaskOperation::UploadGet
            | TaskOperation::UploadStart
            | TaskOperation::UploadCancel
            | TaskOperation::UploadResume
            | TaskOperation::UploadProbe
            | TaskOperation::UploadCleanup
            | TaskOperation::UploadAdopt
            | TaskOperation::UploadList
            | TaskOperation::DownloadCreate
            | TaskOperation::DownloadGet
            | TaskOperation::DownloadList
            | TaskOperation::DownloadStart
            | TaskOperation::DownloadCancel
            | TaskOperation::DownloadResume
            | TaskOperation::DownloadProbe
            | TaskOperation::DownloadCleanup
            | TaskOperation::DownloadAdopt
            | TaskOperation::MixedCreate
            | TaskOperation::MixedRetry
            | TaskOperation::MixedRolloutGet
            | TaskOperation::MixedRolloutApprove
            | TaskOperation::MixedGet
            | TaskOperation::MixedList
            | TaskOperation::MixedReview
            | TaskOperation::MixedStart
            | TaskOperation::MixedCancel
            | TaskOperation::MixedOutput
            | TaskOperation::MixedFileProbe
            | TaskOperation::MixedFileResume
            | TaskOperation::MixedFileCleanup
            | TaskOperation::MixedFileAdopt
            | TaskOperation::MixedFileStatus
            | TaskOperation::MixedResults
            | TaskOperation::MixedResultPage
            | TaskOperation::OutputAnalyze
            | TaskOperation::OutputAnalysisGet
            | TaskOperation::OutputAnalysisCancel
            | TaskOperation::OutputSearch
            | TaskOperation::OutputSearchGet
            | TaskOperation::OutputSearchCancel
            | TaskOperation::OutputMerge
            | TaskOperation::OutputArchive
            | TaskOperation::OutputArchiveGet
            | TaskOperation::OutputArchiveCancel
            | TaskOperation::OutputArchiveRead
            | TaskOperation::OutputWorkspace
            | TaskOperation::OutputWorkspaceGet
            | TaskOperation::OutputWorkspaceCancel
            | TaskOperation::OutputWorkspacePage
            | TaskOperation::OutputWorkspaceRead
            | TaskOperation::OutputRetentionReceipt
            | TaskOperation::OutputIndexRead => return Err(TaskStatus::Invalid),
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        if operation == TaskOperation::Create {
            if request.definition_json.len() > 1024 * 1024 {
                return Err(TaskStatus::Limit);
            }
            let definition: TaskDefinition = serde_json::from_slice(&request.definition_json)
                .map_err(|_| TaskStatus::Invalid)?;
            validate_task_definition(&definition).map_err(|_| TaskStatus::Invalid)?;
            let selection =
                decode_host_selection(&request.selection_json).map_err(|_| TaskStatus::Invalid)?;
            let _catalog = self.0.profiles.catalog_mutation.lock().await;
            let hosts = self
                .0
                .profiles
                .service
                .repository()
                .load_hosts()
                .await
                .map_err(|_| TaskStatus::Unavailable)?
                .unwrap_or(HostCatalogSnapshot {
                    revision: 0,
                    catalog: HostCatalog::default(),
                });
            let profiles = self
                .0
                .profiles
                .service
                .load()
                .await
                .map_err(|_| TaskStatus::Unavailable)?
                .snapshot();
            if hosts.revision != request.host_revision
                || profiles.revision != request.profile_revision
            {
                return Err(TaskStatus::Conflict);
            }
            let targets = freeze_hosts(&hosts, &profiles, selection, MAX_HOST_TARGETS)
                .map_err(|_| TaskStatus::Invalid)?;
            let snapshot = TaskSnapshot::new(
                definition,
                targets,
                "local-user".into(),
                self.0.clock.now_ms().map_err(|_| TaskStatus::Unavailable)?,
            )
            .map_err(|_| TaskStatus::Invalid)?;
            let record = TaskRunRecord::new(snapshot).map_err(|_| TaskStatus::Invalid)?;
            let document_json =
                encode_task_snapshot(record.snapshot()).map_err(|_| TaskStatus::Limit)?;
            self.0
                .repository
                .create_task_run(&record)
                .await
                .map_err(storage)?;
            return Ok(
                reply!(run_id: record.id().as_uuid().as_bytes().to_vec(), revision: record.meta().revision, document_json: document_json),
            );
        }
        if operation == TaskOperation::List {
            let after = if request.run_id.is_empty() {
                None
            } else {
                Some(TaskRunId::from_bytes(
                    decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?,
                ))
            };
            if request.limit == 0 || request.limit as usize > MAX_TASK_LIST_RESULTS {
                return Err(TaskStatus::Invalid);
            }
            let mut rows = self
                .0
                .repository
                .list_task_runs(after, request.limit as usize, false)
                .await
                .map_err(storage)?;
            for row in &rows {
                if matches!(row.meta.state, TaskState::Running | TaskState::Cancelling)
                    && self.control(row.id)?.is_none()
                {
                    self.load(row.id).await?;
                }
            }
            rows = self
                .0
                .repository
                .list_task_runs(after, request.limit as usize, false)
                .await
                .map_err(storage)?;
            let rows: Vec<_> = rows.iter().map(|row| serde_json::json!({ "id": row.id, "name": row.name, "operator": row.operator, "meta": row.meta, "targets": row.targets })).collect();
            return Ok(
                reply!(document_json: serde_json::to_vec(&rows).map_err(|_| TaskStatus::Corrupt)?),
            );
        }
        let id =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        let mut record = self.load(id).await?;
        if operation == TaskOperation::Get {
            return self.view(&record);
        }
        if operation == TaskOperation::Review {
            return Ok(reply!(
                run_id: request.run_id.clone(),
                revision: record.meta().revision,
                document_json: encode_task_snapshot(record.snapshot()).map_err(|_| TaskStatus::Limit)?
            ));
        }
        if operation == TaskOperation::Results {
            if request.expected_revision != 0 && request.expected_revision != record.meta().revision
            {
                return Err(TaskStatus::Conflict);
            }
            let document = TaskResultDocument::from_record(&record, self.control(id)?.is_some())
                .map_err(|_| TaskStatus::Corrupt)?;
            return Ok(
                reply!(run_id: request.run_id.clone(), revision: record.meta().revision,
                document_json: encode_task_results(&document).map_err(|_| TaskStatus::Limit)?),
            );
        }
        if operation == TaskOperation::Compare {
            if request.expected_revision == 0
                || request.expected_revision != record.meta().revision
                || !task_terminal(record.meta().state)
                || self.control(id)?.is_some()
            {
                return Err(TaskStatus::Conflict);
            }
            let left =
                HostId::from_bytes(decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?);
            let right = HostId::from_bytes(
                decode_id(&request.compare_host_id).map_err(|_| TaskStatus::Invalid)?,
            );
            if left == right
                || request.limit == 0
                || request.limit as usize > MAX_TASK_COMPARE_PAGE_BYTES
                || [left, right].iter().any(|host| {
                    record
                        .target(*host)
                        .is_none_or(|target| request.step as usize >= target.steps.len())
                })
            {
                return Err(TaskStatus::Invalid);
            }
            let page = |host| {
                self.0.output.comparison_page(
                    id,
                    host,
                    request.step as usize,
                    request.stderr,
                    request.offset,
                    request.limit as usize,
                )
            };
            let (left_bytes, left_length) =
                page(left).await.map_err(|_| TaskStatus::Unavailable)?;
            let (right_bytes, right_length) =
                page(right).await.map_err(|_| TaskStatus::Unavailable)?;
            let mut document = TaskOutputComparison {
                run_id: id,
                revision: record.meta().revision,
                left_host: left,
                right_host: right,
                step: request.step,
                stderr: request.stderr,
                offset: request.offset,
                left_length,
                right_length,
                left: left_bytes,
                right: right_bytes,
                page_equal: false,
                differing_bytes: 0,
                first_difference: None,
                covers_left_stream: false,
                covers_right_stream: false,
            };
            document.recompute().map_err(|_| TaskStatus::Invalid)?;
            return Ok(
                reply!(run_id: request.run_id.clone(), revision: record.meta().revision,
                document_json: serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?),
            );
        }
        if operation == TaskOperation::Output {
            let host =
                HostId::from_bytes(decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?);
            let target = record.target(host).ok_or(TaskStatus::Invalid)?;
            if request.step as usize >= target.steps.len() {
                return Err(TaskStatus::Invalid);
            }
            let (output, output_length) = self
                .0
                .output
                .read(
                    id,
                    host,
                    request.step as usize,
                    request.stderr,
                    request.offset,
                    request.limit as usize,
                )
                .await
                .map_err(|error| match error {
                    crate::TaskOutputError::Limit => TaskStatus::Limit,
                    crate::TaskOutputError::Invalid => TaskStatus::Invalid,
                    _ => TaskStatus::Unavailable,
                })?;
            return Ok(
                reply!(output: output, output_length: output_length, run_id: request.run_id.clone(), revision: record.meta().revision),
            );
        }
        if operation != TaskOperation::Cancel
            && (request.expected_revision == 0
                || request.expected_revision != record.meta().revision)
        {
            return Err(TaskStatus::Conflict);
        }
        match operation {
            TaskOperation::Retry => {
                if !request.confirmed
                    || !task_terminal(record.meta().state)
                    || self.control(id)?.is_some()
                    || request.retry_hosts.is_empty()
                    || request.retry_hosts.len() > MAX_HOST_TARGETS
                {
                    return Err(TaskStatus::Conflict);
                }
                let hosts: BTreeSet<_> = request
                    .retry_hosts
                    .iter()
                    .map(|bytes| {
                        decode_id(bytes)
                            .map(HostId::from_bytes)
                            .map_err(|_| TaskStatus::Invalid)
                    })
                    .collect::<Result<_, _>>()?;
                if hosts.len() != request.retry_hosts.len() {
                    return Err(TaskStatus::Invalid);
                }
                let snapshot = TaskSnapshot::retry(
                    &record,
                    &hosts,
                    request.confirm_unknown,
                    "local-user".into(),
                    self.0.clock.now_ms().map_err(|_| TaskStatus::Unavailable)?,
                )
                .map_err(|_| TaskStatus::Invalid)?;
                let next = TaskRunRecord::new(snapshot).map_err(|_| TaskStatus::Invalid)?;
                let document_json =
                    encode_task_snapshot(next.snapshot()).map_err(|_| TaskStatus::Limit)?;
                self.0
                    .repository
                    .create_task_run(&next)
                    .await
                    .map_err(storage)?;
                Ok(reply!(run_id: next.id().as_uuid().as_bytes().to_vec(),
                    revision: next.meta().revision, document_json: document_json))
            }
            TaskOperation::Start => {
                if !request.confirmed
                    || !matches!(record.meta().state, TaskState::Queued | TaskState::Paused)
                {
                    return Err(TaskStatus::Conflict);
                }
                {
                    let mut active = self.0.active.lock().map_err(|_| TaskStatus::Unavailable)?;
                    if active.contains_key(&id) {
                        return Err(TaskStatus::Busy);
                    }
                    if active.len() >= 64 {
                        return Err(TaskStatus::Limit);
                    }
                    let (control, signals) = task_run_controls();
                    active.insert(id, control);
                    let inner = self.0.clone();
                    tokio::spawn(async move {
                        if inner.scheduler.run(id, signals).await.is_err() {
                            tracing::warn!(task_run_id = %id, "task runner stopped; inspect durable state before further action");
                        }
                        if let Ok(mut active) = inner.active.lock() {
                            active.remove(&id);
                        }
                    });
                }
                self.view(&record)
            }
            TaskOperation::Cancel => {
                if task_terminal(record.meta().state) {
                    return Err(TaskStatus::Conflict);
                }
                if let Some(control) = self.control(id)? {
                    control.cancel();
                } else {
                    self.commit(
                        &mut record,
                        TaskRunAction::Stop(TaskStopReason::UserCancelled),
                    )
                    .await?;
                    if record.targets().iter().all(|t| target_terminal(t.state)) {
                        self.commit(&mut record, TaskRunAction::Finish).await?;
                    }
                }
                self.view(&record)
            }
            TaskOperation::Approve => {
                if !request.confirmed {
                    return Err(TaskStatus::Invalid);
                }
                let gate = TaskGateId::from_bytes(
                    decode_id(&request.gate_id).map_err(|_| TaskStatus::Invalid)?,
                );
                if record
                    .meta()
                    .gate
                    .as_ref()
                    .is_none_or(|current| current.id != gate)
                {
                    return Err(TaskStatus::Conflict);
                }
                let control = self.control(id)?.ok_or(TaskStatus::Conflict)?;
                control
                    .approve_gate(gate)
                    .await
                    .map_err(|_| TaskStatus::Conflict)?;
                self.view(&self.load(id).await?)
            }
            _ => Err(TaskStatus::Invalid),
        }
    }
    pub(crate) async fn handle(&self, request: &TaskRequest) -> TaskResponse {
        if let Ok(operation) = TaskOperation::try_from(request.operation)
            && operation.is_output_workspace()
        {
            return match self.dispatch_workspace(request, operation).await {
                Ok(reply) => reply,
                Err(status) => response(status),
            };
        }
        if let Ok(operation) = TaskOperation::try_from(request.operation)
            && matches!(
                operation,
                TaskOperation::OutputArchiveGet
                    | TaskOperation::OutputArchiveCancel
                    | TaskOperation::OutputArchiveRead
            )
        {
            return match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.dispatch_output_archive(request, operation),
            )
            .await
            {
                Ok(Ok(reply)) => reply,
                Ok(Err(status)) => response(status),
                Err(_) => response(TaskStatus::Unavailable),
            };
        }
        if let Ok(operation) = TaskOperation::try_from(request.operation)
            && matches!(
                operation,
                TaskOperation::OutputSearchGet | TaskOperation::OutputSearchCancel
            )
        {
            return match self.dispatch_output_search(request, operation).await {
                Ok(reply) => reply,
                Err(status) => response(status),
            };
        }
        if let Ok(operation) = TaskOperation::try_from(request.operation)
            && matches!(
                operation,
                TaskOperation::OutputAnalysisGet | TaskOperation::OutputAnalysisCancel
            )
        {
            return match self.dispatch_output_analysis(request, operation).await {
                Ok(reply) => reply,
                Err(status) => response(status),
            };
        }
        if request.operation == TaskOperation::MixedCancel as i32 {
            return self.cancel_mixed(request).await;
        }
        if request.operation == TaskOperation::DownloadCancel as i32 {
            return self.cancel_download(request).await;
        }
        if request.operation == TaskOperation::UploadCancel as i32 {
            let mut canonical = TaskRequest::default();
            canonical.operation = request.operation;
            canonical.run_id.clone_from(&request.run_id);
            if canonical != *request {
                return response(TaskStatus::Invalid);
            }
            let Ok(id) = decode_id(&request.run_id) else {
                return response(TaskStatus::Invalid);
            };
            return match self.0.uploads.cancel(TaskRunId::from_bytes(id)) {
                Ok(()) => reply!(run_id: request.run_id.clone()),
                Err(UploadRepositoryError::Conflict) => {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        self.0.uploads.cancel_pending(TaskRunId::from_bytes(id)),
                    )
                    .await
                    {
                        Ok(Ok(())) => reply!(run_id: request.run_id.clone()),
                        Ok(Err(error)) => response(upload_status(error)),
                        Err(_) => response(TaskStatus::Unavailable),
                    }
                }
                Err(error) => response(upload_status(error)),
            };
        }
        // Cancellation must not wait behind storage or a gate acknowledgement.
        // It only reduces execution authority and is bound to an immutable run ID.
        if request.operation == TaskOperation::Cancel as i32 {
            let mut canonical = TaskRequest::default();
            canonical.operation = TaskOperation::Cancel as i32;
            canonical.run_id.clone_from(&request.run_id);
            if canonical != *request {
                return response(TaskStatus::Invalid);
            }
            let Ok(bytes) = decode_id(&request.run_id) else {
                return response(TaskStatus::Invalid);
            };
            let id = TaskRunId::from_bytes(bytes);
            match self.control(id) {
                Ok(Some(control)) => {
                    control.cancel();
                    return reply!(run_id: request.run_id.clone());
                }
                Err(status) => return response(status),
                Ok(None) => {}
            }
        }
        let _operation = self.0.operations.lock().await;
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.dispatch(request)).await
        {
            Ok(Ok(result)) => result,
            Ok(Err(status)) => response(status),
            Err(_) => response(TaskStatus::Unavailable),
        }
    }
}
fn upload_status(error: UploadRepositoryError) -> TaskStatus {
    match error {
        UploadRepositoryError::Conflict => TaskStatus::Conflict,
        UploadRepositoryError::Missing => TaskStatus::Missing,
        UploadRepositoryError::Unavailable => TaskStatus::Unavailable,
        UploadRepositoryError::Corrupt => TaskStatus::Corrupt,
    }
}
impl TaskIpcService {
    async fn upload_view(&self, id: TaskRunId) -> Result<TaskResponse, TaskStatus> {
        let (snapshot, rows) = self.0.uploads.get(id).await.map_err(upload_status)?;
        let targets: Vec<_> = snapshot
            .targets
            .targets()
            .iter()
            .zip(&rows)
            .map(|(host, row)| UploadStatusTarget {
                host_id: host.id,
                name: host.name.clone(),
                address: host.address.clone(),
                port: host.port,
                row: row.clone(),
            })
            .collect();
        let (active, recovery) = self.0.uploads.activity(id).map_err(upload_status)?;
        let document = UploadStatusDocument {
            format: "cshell.batch-upload-status".into(),
            version: 1,
            id,
            definition: snapshot.definition.clone(),
            created_at_ms: snapshot.created_at_ms,
            host_revision: snapshot.targets.host_revision(),
            profile_revision: snapshot.targets.profile_revision(),
            active,
            targets,
            recovery,
        };
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        let bytes = serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?;
        if bytes.len() > MAX_TASK_SNAPSHOT_BYTES {
            return Err(TaskStatus::Limit);
        }
        Ok(reply!(run_id: id.as_uuid().as_bytes().to_vec(), document_json: bytes))
    }
    async fn dispatch_upload(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        if operation == TaskOperation::UploadList {
            canonical.run_id.clone_from(&request.run_id);
            canonical.limit = request.limit;
        } else if operation == TaskOperation::UploadCreate {
            canonical
                .definition_json
                .clone_from(&request.definition_json);
            canonical.selection_json.clone_from(&request.selection_json);
            canonical.host_revision = request.host_revision;
            canonical.profile_revision = request.profile_revision;
        } else {
            canonical.run_id.clone_from(&request.run_id);
            if matches!(
                operation,
                TaskOperation::UploadStart
                    | TaskOperation::UploadResume
                    | TaskOperation::UploadCleanup
                    | TaskOperation::UploadAdopt
            ) {
                canonical.confirmed = request.confirmed;
            }
            if matches!(
                operation,
                TaskOperation::UploadResume
                    | TaskOperation::UploadProbe
                    | TaskOperation::UploadCleanup
                    | TaskOperation::UploadAdopt
            ) {
                canonical.host_id.clone_from(&request.host_id);
            }
            if matches!(
                operation,
                TaskOperation::UploadResume
                    | TaskOperation::UploadCleanup
                    | TaskOperation::UploadAdopt
            ) {
                canonical.expected_revision = request.expected_revision;
            }
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        if operation == TaskOperation::UploadList {
            let after = if request.run_id.is_empty() {
                None
            } else {
                Some(TaskRunId::from_bytes(
                    decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?,
                ))
            };
            if request.limit == 0 || request.limit > 64 {
                return Err(TaskStatus::Invalid);
            }
            let rows = self
                .0
                .uploads
                .list(after, request.limit as usize)
                .await
                .map_err(upload_status)?;
            return Ok(
                reply!(document_json: serde_json::to_vec(&rows).map_err(|_| TaskStatus::Corrupt)?),
            );
        }
        if operation == TaskOperation::UploadCreate {
            if request.definition_json.len() > 1024 * 1024 {
                return Err(TaskStatus::Limit);
            }
            let definition: BatchUploadDefinition =
                serde_json::from_slice(&request.definition_json)
                    .map_err(|_| TaskStatus::Invalid)?;
            let selection =
                decode_host_selection(&request.selection_json).map_err(|_| TaskStatus::Invalid)?;
            let _catalog = self.0.profiles.catalog_mutation.lock().await;
            let hosts = self
                .0
                .profiles
                .service
                .repository()
                .load_hosts()
                .await
                .map_err(|_| TaskStatus::Unavailable)?
                .unwrap_or(HostCatalogSnapshot {
                    revision: 0,
                    catalog: HostCatalog::default(),
                });
            let profiles = self
                .0
                .profiles
                .service
                .load()
                .await
                .map_err(|_| TaskStatus::Unavailable)?
                .snapshot();
            if hosts.revision != request.host_revision
                || profiles.revision != request.profile_revision
            {
                return Err(TaskStatus::Conflict);
            }
            let targets = freeze_hosts(&hosts, &profiles, selection, MAX_HOST_TARGETS)
                .map_err(|_| TaskStatus::Invalid)?;
            let snapshot = BatchUploadSnapshot::new(
                definition,
                targets,
                self.0.clock.now_ms().map_err(|_| TaskStatus::Unavailable)?,
            )
            .map_err(|_| TaskStatus::Invalid)?;
            let id = self
                .0
                .uploads
                .create(snapshot)
                .await
                .map_err(upload_status)?;
            return self.upload_view(id).await;
        }
        let id =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        match operation {
            TaskOperation::UploadGet => self.upload_view(id).await,
            TaskOperation::UploadStart => {
                let (_, rows) = self.0.uploads.get(id).await.map_err(upload_status)?;
                if !rows.iter().any(|row| row.phase == UploadPhase::Pending) {
                    return Err(TaskStatus::Conflict);
                }
                self.0
                    .uploads
                    .start(id, request.confirmed)
                    .map_err(upload_status)?;
                Ok(reply!(run_id: request.run_id.clone()))
            }
            TaskOperation::UploadResume
            | TaskOperation::UploadProbe
            | TaskOperation::UploadCleanup
            | TaskOperation::UploadAdopt => {
                let host = HostId::from_bytes(
                    decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?,
                );
                if operation == TaskOperation::UploadProbe {
                    self.0
                        .uploads
                        .start_recovery(id, host, 0, false, UploadRecoveryOperation::Probe)
                        .await
                        .map_err(upload_status)?;
                    return Ok(reply!(run_id: request.run_id.clone()));
                }
                if operation == TaskOperation::UploadResume {
                    self.0
                        .uploads
                        .resume(id, host, request.expected_revision, request.confirmed)
                        .await
                        .map_err(upload_status)?;
                    return Ok(reply!(run_id: request.run_id.clone()));
                }
                self.0
                    .uploads
                    .start_recovery(
                        id,
                        host,
                        request.expected_revision,
                        request.confirmed,
                        if operation == TaskOperation::UploadAdopt {
                            UploadRecoveryOperation::Adopt
                        } else {
                            UploadRecoveryOperation::Cleanup
                        },
                    )
                    .await
                    .map_err(upload_status)?;
                Ok(reply!(run_id: request.run_id.clone()))
            }
            _ => Err(TaskStatus::Invalid),
        }
    }
}
