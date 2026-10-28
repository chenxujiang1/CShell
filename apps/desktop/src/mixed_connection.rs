//! Bounded mixed control, local manifest hashing and readonly command output.
use crate::{
    daemon_connection::DesktopConnectionConfig,
    file_task_transport::{self, FileTaskKind},
    task_connection::output_log_page,
};
use cshell_application::*;
use cshell_domain::{HostId, HostSelection, TaskRunId};
use cshell_ipc::{TaskOperation, TaskRequest, TaskResponse};
use cshell_render::LogPage;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
pub(crate) type MixedExpectedTargets = BTreeMap<HostId, (String, String, u16)>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MixedOutputKey {
    pub run: TaskRunId,
    pub host: HostId,
    pub step: u32,
    pub child: TaskRunId,
    pub revision: u64,
    pub stderr: bool,
    pub offset: u64,
}
#[derive(Clone)]
pub(crate) struct MixedOutputPage {
    pub key: MixedOutputKey,
    pub context: MixedOutputContext,
    pub page: Arc<LogPage>,
}
impl std::fmt::Debug for MixedOutputPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MixedOutputPage")
            .field("key", &self.key)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug)]
pub(crate) struct MixedView {
    pub rows: Arc<[MixedRunSummary]>,
    pub after: Option<TaskRunId>,
    pub plan: Option<MixedTaskSnapshot>,
    pub page: Option<Arc<MixedStatusPage>>,
    pub manifests: BTreeMap<u64, BatchUploadDefinition>,
    pub output: Option<MixedOutputPage>,
    pub repair: Option<Arc<MixedRepairDocument>>,
    pub rollout: Option<Arc<MixedRolloutDocument>>,
    pub results: Option<Arc<MixedResultDocument>>,
    pub busy: bool,
    pub cancel_busy: bool,
    pub hashing: Option<u64>,
    pub available: bool,
    pub writes_allowed: bool,
    pub generation: u64,
    pub status: String,
    epoch: u64,
}
impl Default for MixedView {
    fn default() -> Self {
        Self {
            rows: Arc::from([]),
            after: None,
            plan: None,
            page: None,
            manifests: BTreeMap::new(),
            output: None,
            repair: None,
            rollout: None,
            results: None,
            busy: false,
            cancel_busy: false,
            hashing: None,
            available: false,
            writes_allowed: true,
            generation: 0,
            status: String::new(),
            epoch: 0,
        }
    }
}
enum Operation {
    List(Option<TaskRunId>),
    Inspect(TaskRunId),
    Page(MixedTaskSnapshot, Option<HostId>, bool, bool),
    Repair(
        MixedTaskSnapshot,
        HostId,
        usize,
        u64,
        MixedFileOperation,
        Option<HostId>,
    ),
    RepairStatus(MixedTaskSnapshot),
    Results(MixedTaskSnapshot),
    Export(
        Arc<MixedResultDocument>,
        TaskResultExportFormat,
        std::path::PathBuf,
    ),
    Create(
        MixedTaskDefinition,
        HostSelection,
        MixedExpectedTargets,
        u64,
        u64,
    ),
    Start(MixedTaskSnapshot, MixedStartConfirmation, Option<HostId>),
    Retry(MixedTaskSnapshot, MixedRetryConfirmation),
    Approve(MixedTaskSnapshot, Arc<MixedRolloutDocument>),
    Hash(u64, BatchUploadDefinition),
    Output(MixedTaskSnapshot, MixedOutputKey),
}
impl Operation {
    fn mutates(&self) -> bool {
        matches!(
            self,
            Self::Create(..) | Self::Start(..) | Self::Retry(..) | Self::Approve(..)
        ) || matches!(self,Self::Repair(_,_,_,_,operation,_) if *operation!=MixedFileOperation::Probe)
    }
}
struct Queued {
    operation: Operation,
    epoch: u64,
}
pub(crate) struct MixedConnection {
    pub analysis: Arc<crate::output_analysis_connection::OutputAnalysisConnection>,
    pub search: Arc<crate::output_search_connection::OutputSearchConnection>,
    pub merge: Arc<crate::output_merge_connection::OutputMergeConnection>,
    pub archives: Arc<crate::output_archive_connection::OutputArchiveConnection>,
    pub workspace: Arc<crate::output_workspace_connection::OutputWorkspaceConnection>,
    shared: Arc<Mutex<MixedView>>,
    sender: Option<tokio::sync::mpsc::Sender<Queued>>,
    cancels: Option<tokio::sync::mpsc::Sender<TaskRunId>>,
    hash_cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
struct Failure {
    message: String,
    uncertain: bool,
}
impl Failure {
    fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain: false,
        }
    }
    fn wire(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain: true,
        }
    }
}
impl MixedConnection {
    pub fn start(
        config: DesktopConnectionConfig,
        analysis: Arc<crate::output_analysis_connection::OutputAnalysisConnection>,
        search: Arc<crate::output_search_connection::OutputSearchConnection>,
        merge: Arc<crate::output_merge_connection::OutputMergeConnection>,
        archives: Arc<crate::output_archive_connection::OutputArchiveConnection>,
        workspace: Arc<crate::output_workspace_connection::OutputWorkspaceConnection>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(MixedView {
            busy: true,
            ..Default::default()
        }));
        let state = shared.clone();
        let hash_cancel = Arc::new(AtomicBool::new(false));
        let hash_stop = hash_cancel.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Queued>(1);
        let (cancels, mut cancel_rx) = tokio::sync::mpsc::channel::<TaskRunId>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-mixed-control-ui".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let mut view = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.busy = false;
                    view.status = "无法启动混合任务连接".into();
                    return;
                };
                runtime.block_on(async move {
                    let cancel_config = config.clone();
                    let cancel_state = state.clone();
                    let cancellation = tokio::spawn(async move {
                        while let Some(id) = cancel_rx.recv().await {
                            let result = send(
                                &cancel_config,
                                request(TaskOperation::MixedCancel, Some(id)),
                            )
                            .await;
                            let mut view = cancel_state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.cancel_busy = false;
                            view.generation = view.generation.wrapping_add(1);
                            if result.is_err() {
                                block(&mut view);
                            }
                            view.status = if result.is_ok() {
                                "取消已请求；请查询保存状态".into()
                            } else {
                                "取消回复不明；重新载入状态后再修改，未自动重发".into()
                            };
                        }
                    });
                    let mut queued = Queued {
                        operation: Operation::List(None),
                        epoch: 0,
                    };
                    loop {
                        let mutation = queued.operation.mutates();
                        let epoch = queued.epoch;
                        let result = perform(&config, queued.operation, hash_stop.clone()).await;
                        {
                            let mut view = state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = false;
                            view.hashing = None;
                            view.generation = view.generation.wrapping_add(1);
                            match result {
                                Ok(Outcome::List(rows, after)) => {
                                    view.available = true;
                                    view.rows = rows.into();
                                    view.after = after;
                                    view.status = "混合任务列表已载入".into();
                                }
                                Ok(Outcome::Loaded(plan, page, reload, repair)) => {
                                    if view
                                        .plan
                                        .as_ref()
                                        .is_none_or(|saved| saved.id() != plan.id())
                                    {
                                        view.output = None;
                                        view.repair = None;
                                        view.results = None;
                                    }
                                    view.available = true;
                                    view.plan = Some(plan);
                                    view.page = Some(Arc::new(page.status));
                                    view.rollout = page.rollout.map(Arc::new);
                                    if let Some(repair) = repair {
                                        view.repair = Some(Arc::new(repair));
                                    }
                                    if reload
                                        && epoch != u64::MAX
                                        && epoch == view.epoch
                                        && !view.cancel_busy
                                    {
                                        view.writes_allowed = true;
                                    }
                                    view.status = "冻结混合计划与目标分页已载入".into();
                                }
                                Ok(Outcome::Manifest(key, definition)) => {
                                    if view.manifests.len() >= MAX_TASK_STEPS
                                        && !view.manifests.contains_key(&key)
                                    {
                                        view.manifests.pop_first();
                                    }
                                    view.manifests.insert(key, definition);
                                    view.status = "上传源摘要已生成".into();
                                }
                                Ok(Outcome::Output(mut output)) => {
                                    Arc::make_mut(&mut output.page).revision = view.generation;
                                    view.output = Some(output);
                                    view.status = "已载入命令字节输出页".into();
                                }
                                Ok(Outcome::RepairStatus(document)) => {
                                    view.repair = Some(Arc::new(document));
                                    view.status = "文件修复状态已载入".into();
                                }
                                Ok(Outcome::Results(document)) => {
                                    view.results = Some(Arc::new(document));
                                    view.status = "全部目标的固定结果已载入".into();
                                }
                                Ok(Outcome::Exported) => {
                                    view.status = "结果已保存到新文件".into();
                                }
                                Err(error) => {
                                    if error.uncertain {
                                        view.available = false;
                                        if mutation {
                                            block(&mut view);
                                        }
                                    }
                                    view.status = format!("{}；未自动重放请求", error.message);
                                }
                            }
                        }
                        let Some(next) = receiver.recv().await else {
                            break;
                        };
                        queued = next;
                    }
                    let _joined = cancellation.await;
                });
            })?;
        Ok(Self {
            workspace,
            archives,
            merge,
            analysis,
            search,
            shared,
            sender: Some(sender),
            cancels: Some(cancels),
            hash_cancel,
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> MixedView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn enqueue(&self, operation: Operation) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy
            || operation.mutates() && (!view.available || !view.writes_allowed || view.cancel_busy)
        {
            return false;
        }
        let bound_plan = match &operation {
            Operation::Page(plan, ..)
            | Operation::Output(plan, ..)
            | Operation::Repair(plan, ..)
            | Operation::RepairStatus(plan) => Some(plan),
            Operation::Results(plan) | Operation::Retry(plan, _) | Operation::Approve(plan, _) => {
                Some(plan)
            }
            _ => None,
        };
        if let Some(plan) = bound_plan
            && view.plan.as_ref().is_none_or(|current| {
                current.id() != plan.id() || current.fingerprint() != plan.fingerprint()
            })
        {
            return false;
        }
        if let Operation::Start(plan, expected, _) = &operation {
            if view.plan.as_ref().is_none_or(|saved| {
                saved.id() != plan.id() || saved.fingerprint() != plan.fingerprint()
            }) || view
                .page
                .as_ref()
                .and_then(|page| page.confirmation_bound(view.rollout.as_deref()).ok())
                .as_ref()
                != Some(expected)
            {
                return false;
            }
        }
        if let Operation::Retry(plan, confirmation) = &operation {
            if confirmation.validate().is_err()
                || confirmation.run_id != plan.id()
                || confirmation.snapshot_sha256 != *plan.fingerprint()
                || view.page.as_ref().is_none_or(|page| page.active)
            {
                return false;
            }
            if confirmation.targets.iter().any(|expected| {
                view.page
                    .as_ref()
                    .and_then(|page| {
                        page.targets
                            .iter()
                            .find(|target| target.run.host_id == expected.host_id)
                    })
                    .is_some_and(|target| {
                        target.run.revision != expected.revision
                            || mixed_retry_block(plan, &target.run).is_some()
                    })
            }) {
                return false;
            }
        }
        if let Operation::Approve(_, document) = &operation {
            if view
                .rollout
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, document))
                || document.active
                || document.progress.gate.is_none()
            {
                return false;
            }
        }
        if let Operation::Repair(plan, host, step, revision, action, _) = &operation {
            if view.cancel_busy
                || !view.available
                || view.page.as_ref().is_none_or(|page| {
                    page.active
                        || page
                            .targets
                            .iter()
                            .find(|target| target.run.host_id == *host)
                            .is_none_or(|target| {
                                target.run.revision != *revision
                                    || !target.run.file_repair_allowed(*step, *action)
                                    || *action == MixedFileOperation::Adopt
                                        && view.repair.as_ref().is_none_or(|document| {
                                            !document.matching_destination(plan, &target.run, *step)
                                        })
                            })
                })
            {
                return false;
            }
        }
        if let Operation::Results(_) = &operation {
            if view.cancel_busy
                || !view.available
                || view
                    .page
                    .as_ref()
                    .is_none_or(|page| page.active || page.counts[0] != 0 || page.counts[1] != 0)
            {
                return false;
            }
        }
        if let Operation::Export(document, _, _) = &operation {
            if view
                .results
                .as_ref()
                .is_none_or(|saved| !Arc::ptr_eq(saved, document))
            {
                return false;
            }
        }
        let hashing = if let Operation::Hash(key, _) = &operation {
            Some(*key)
        } else {
            None
        };
        if hashing.is_some() {
            self.hash_cancel.store(false, Ordering::Release);
        }
        let queued = Queued {
            operation,
            epoch: view.epoch,
        };
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(queued).is_ok())
        {
            view.hashing = hashing;
            view.busy = true;
            true
        } else {
            false
        }
    }
    pub fn list(&self, after: Option<TaskRunId>) -> bool {
        self.enqueue(Operation::List(after))
    }
    pub fn inspect(&self, id: TaskRunId) -> bool {
        self.enqueue(Operation::Inspect(id))
    }
    pub fn page(&self, plan: MixedTaskSnapshot, after: Option<HostId>, reload: bool) -> bool {
        let track = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .repair
            .as_ref()
            .is_some_and(|document| document.run_id == plan.id());
        self.enqueue(Operation::Page(plan, after, reload, track))
    }
    pub fn repair(
        &self,
        plan: MixedTaskSnapshot,
        host: HostId,
        step: usize,
        revision: u64,
        operation: MixedFileOperation,
        after: Option<HostId>,
    ) -> bool {
        self.enqueue(Operation::Repair(
            plan, host, step, revision, operation, after,
        ))
    }
    pub fn repair_status(&self, plan: MixedTaskSnapshot) -> bool {
        self.enqueue(Operation::RepairStatus(plan))
    }
    pub fn results(&self, plan: MixedTaskSnapshot) -> bool {
        self.enqueue(Operation::Results(plan))
    }
    pub fn export(
        &self,
        document: Arc<MixedResultDocument>,
        format: TaskResultExportFormat,
        path: std::path::PathBuf,
    ) -> bool {
        self.enqueue(Operation::Export(document, format, path))
    }
    pub fn create(
        &self,
        definition: MixedTaskDefinition,
        selection: HostSelection,
        expected: MixedExpectedTargets,
        hosts: u64,
        profiles: u64,
    ) -> bool {
        self.enqueue(Operation::Create(
            definition, selection, expected, hosts, profiles,
        ))
    }
    pub fn start_run(
        &self,
        plan: MixedTaskSnapshot,
        confirmation: MixedStartConfirmation,
        after: Option<HostId>,
    ) -> bool {
        self.enqueue(Operation::Start(plan, confirmation, after))
    }
    pub fn retry(&self, plan: MixedTaskSnapshot, confirmation: MixedRetryConfirmation) -> bool {
        self.enqueue(Operation::Retry(plan, confirmation))
    }
    pub fn approve(&self, plan: MixedTaskSnapshot, document: Arc<MixedRolloutDocument>) -> bool {
        self.enqueue(Operation::Approve(plan, document))
    }
    pub fn hash(&self, key: u64, definition: BatchUploadDefinition) -> bool {
        self.enqueue(Operation::Hash(key, definition))
    }
    pub fn cancel_hash(&self) {
        self.hash_cancel.store(true, Ordering::Release);
    }
    pub fn output(&self, plan: MixedTaskSnapshot, key: MixedOutputKey) -> bool {
        self.enqueue(Operation::Output(plan, key))
    }
    pub fn cancel(&self, id: TaskRunId) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.cancel_busy
            || view
                .page
                .as_ref()
                .is_none_or(|page| page.run_id != id || !(page.active || !page.pending.is_empty()))
        {
            return false;
        }
        if self
            .cancels
            .as_ref()
            .is_some_and(|sender| sender.try_send(id).is_ok())
        {
            view.cancel_busy = true;
            view.generation = view.generation.wrapping_add(1);
            true
        } else {
            false
        }
    }
}
impl Drop for MixedConnection {
    fn drop(&mut self) {
        self.cancel_hash();
        self.sender.take();
        self.cancels.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
fn block(view: &mut MixedView) {
    view.writes_allowed = false;
    view.epoch = view.epoch.saturating_add(1);
}
fn request(operation: TaskOperation, id: Option<TaskRunId>) -> TaskRequest {
    let mut value = TaskRequest::default();
    value.operation = operation as i32;
    if let Some(id) = id {
        value.run_id = id.as_uuid().as_bytes().to_vec();
    }
    value
}
async fn send(
    config: &DesktopConnectionConfig,
    query: TaskRequest,
) -> Result<TaskResponse, Failure> {
    file_task_transport::send(config, query, FileTaskKind::Mixed)
        .await
        .map_err(Failure::wire)
}
struct LoadedPage {
    status: MixedStatusPage,
    rollout: Option<MixedRolloutDocument>,
}
async fn rollout(
    config: &DesktopConnectionConfig,
    plan: &MixedTaskSnapshot,
) -> Result<MixedRolloutDocument, Failure> {
    let reply = send(
        config,
        request(TaskOperation::MixedRolloutGet, Some(plan.id())),
    )
    .await?;
    let document = decode_mixed_rollout(&reply.document_json)
        .map_err(|_| Failure::wire("分批调度状态无效"))?;
    document
        .check_snapshot(plan)
        .map_err(|_| Failure::wire("分批调度快照不一致"))?;
    Ok(document)
}
async fn page(
    config: &DesktopConnectionConfig,
    plan: &MixedTaskSnapshot,
    after: Option<HostId>,
) -> Result<LoadedPage, Failure> {
    let mut query = request(TaskOperation::MixedGet, Some(plan.id()));
    query.limit = MAX_MIXED_STATUS_TARGETS as u32;
    if let Some(after) = after {
        query.host_id = after.as_uuid().as_bytes().to_vec();
    }
    let reply = send(config, query).await?;
    let page = decode_mixed_status(&reply.document_json)
        .map_err(|_| Failure::wire("混合任务状态分页无效"))?;
    page.check_snapshot(plan)
        .map_err(|_| Failure::wire("状态与冻结计划不一致"))?;
    if page.after != after || page.limit != MAX_MIXED_STATUS_TARGETS {
        return Err(Failure::wire("状态游标不一致"));
    }
    let rollout = if plan.definition().policy.rollout.is_some() {
        Some(rollout(config, plan).await?)
    } else {
        None
    };
    Ok(LoadedPage {
        status: page,
        rollout,
    })
}
enum Outcome {
    List(Vec<MixedRunSummary>, Option<TaskRunId>),
    Loaded(
        MixedTaskSnapshot,
        LoadedPage,
        bool,
        Option<MixedRepairDocument>,
    ),
    RepairStatus(MixedRepairDocument),
    Results(MixedResultDocument),
    Exported,
    Manifest(u64, BatchUploadDefinition),
    Output(MixedOutputPage),
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
    stop: Arc<AtomicBool>,
) -> Result<Outcome, Failure> {
    match operation {
        Operation::Results(plan) => {
            let reply = send(
                config,
                request(TaskOperation::MixedResults, Some(plan.id())),
            )
            .await?;
            let manifest = decode_mixed_result_manifest(&reply.document_json)
                .map_err(|_| Failure::wire("混合结果清单无效"))?;
            if manifest.header.run_id != plan.id()
                || manifest.header.snapshot_sha256 != *plan.fingerprint()
            {
                return Err(Failure::wire("混合结果与计划不一致"));
            }
            let mut collector = MixedResultCollector::new(manifest.clone())
                .map_err(|_| Failure::wire("混合结果清单无效"))?;
            loop {
                let mut query = request(TaskOperation::MixedResultPage, Some(plan.id()));
                query.result_id = manifest.result_id.as_uuid().as_bytes().to_vec();
                query.limit = MAX_MIXED_STATUS_TARGETS as u32;
                if let Some(after) = collector.after() {
                    query.host_id = after.as_uuid().as_bytes().to_vec();
                }
                let reply = send(config, query).await?;
                if collector
                    .push(&reply.document_json)
                    .map_err(|_| Failure::wire("混合结果分页无效或超限"))?
                {
                    break;
                }
            }
            let document = collector
                .finish()
                .map_err(|_| Failure::wire("混合结果摘要不一致"))?;
            document
                .check_snapshot(&plan)
                .map_err(|_| Failure::wire("混合结果目标与计划不一致"))?;
            Ok(Outcome::Results(document))
        }
        Operation::Export(document, format, path) => {
            tokio::task::spawn_blocking(move || {
                cshell_ipc::publish_export_file(&path, |file| {
                    write_mixed_result_export(&document, format, file)
                })
            })
            .await
            .map_err(|_| Failure::local("导出未完成，请核对目标文件后再操作"))?
            .map_err(|_| Failure::local("导出失败，请核对目标文件后再操作"))?;
            Ok(Outcome::Exported)
        }
        Operation::List(after) => {
            let mut query = request(TaskOperation::MixedList, after);
            query.limit = 64;
            let reply = send(config, query).await?;
            let rows = decode_mixed_list(&reply.document_json)
                .map_err(|_| Failure::wire("混合列表无效"))?;
            if after.is_some_and(|id| rows.iter().any(|row| row.id <= id)) {
                return Err(Failure::wire("混合列表游标不一致"));
            }
            Ok(Outcome::List(rows, after))
        }
        Operation::Inspect(id) => {
            let reply = send(config, request(TaskOperation::MixedReview, Some(id))).await?;
            let plan = decode_mixed_snapshot(&reply.document_json)
                .map_err(|_| Failure::wire("混合冻结计划无效"))?;
            if plan.id() != id {
                return Err(Failure::wire("混合计划身份不一致"));
            }
            let page = page(config, &plan, None).await?;
            Ok(Outcome::Loaded(plan, page, true, None))
        }
        Operation::Page(plan, after, reload, track) => {
            let page = page(config, &plan, after).await?;
            let repair = if track {
                Some(repair_status(config, &plan).await?)
            } else {
                None
            };
            Ok(Outcome::Loaded(plan, page, reload, repair))
        }
        Operation::Hash(key, mut definition) => {
            definition
                .validate()
                .map_err(|_| Failure::local("上传路径或策略无效"))?;
            let path = std::path::PathBuf::from(&definition.local_source);
            let value = tokio::task::spawn_blocking(move || {
                cshell_ipc::fingerprint_upload_source(&path, MAX_UPLOAD_FILE_BYTES, &stop)
            })
            .await
            .map_err(|_| Failure::local("摘要计算未完成"))?
            .map_err(|error| Failure::local(error.to_string()))?;
            definition.size = value.size;
            definition.sha256 = value.sha256;
            Ok(Outcome::Manifest(key, definition))
        }
        Operation::Create(definition, selection, expected, hosts, profiles) => {
            definition
                .validate()
                .map_err(|error| Failure::local(error.to_string()))?;
            let mut query = request(TaskOperation::MixedCreate, None);
            query.definition_json =
                serde_json::to_vec(&definition).map_err(|_| Failure::local("混合定义无效"))?;
            query.selection_json =
                encode_host_selection(&selection).map_err(|_| Failure::local("目标选择无效"))?;
            query.host_revision = hosts;
            query.profile_revision = profiles;
            let reply = send(config, query).await?;
            let plan = decode_mixed_snapshot(&reply.document_json)
                .map_err(|_| Failure::wire("保存计划回复无效，结果不明"))?;
            let actual: MixedExpectedTargets = plan
                .targets()
                .targets()
                .iter()
                .map(|host| {
                    (
                        host.id,
                        (host.name.clone(), host.address.clone(), host.port),
                    )
                })
                .collect();
            if plan.id().as_uuid().as_bytes().as_slice() != reply.run_id
                || plan.definition() != &definition
                || plan.targets().selection() != &selection
                || plan.targets().host_revision() != hosts
                || plan.targets().profile_revision() != profiles
                || actual != expected
            {
                return Err(Failure::wire("保存计划与提交内容不一致，结果不明"));
            }
            let page = page(config, &plan, None).await?;
            Ok(Outcome::Loaded(plan, page, false, None))
        }
        Operation::Start(plan, confirmation, after) => {
            confirmation
                .validate()
                .map_err(|_| Failure::local("启动确认无效"))?;
            let mut query = request(TaskOperation::MixedStart, Some(plan.id()));
            query.confirmed = true;
            query.expected_targets_json =
                serde_json::to_vec(&confirmation).map_err(|_| Failure::local("启动确认无效"))?;
            send(config, query).await?;
            let page = page(config, &plan, after).await?;
            Ok(Outcome::Loaded(plan, page, false, None))
        }
        Operation::Retry(parent, confirmation) => {
            confirmation
                .validate()
                .map_err(|_| Failure::local("新运行重试确认无效"))?;
            let mut query = request(TaskOperation::MixedRetry, Some(parent.id()));
            query.confirmed = true;
            query.expected_targets_json =
                serde_json::to_vec(&confirmation).map_err(|_| Failure::local("重试确认无效"))?;
            let reply = send(config, query).await?;
            let plan = decode_mixed_snapshot(&reply.document_json)
                .map_err(|_| Failure::wire("新运行回复无效，结果不明"))?;
            if plan.id().as_uuid().as_bytes().as_slice() != reply.run_id
                || plan
                    .check_retry_confirmation(&parent, &confirmation)
                    .is_err()
            {
                return Err(Failure::wire("新运行来源或冻结计划不一致，结果不明"));
            }
            let page = page(config, &plan, None).await?;
            Ok(Outcome::Loaded(plan, page, false, None))
        }
        Operation::Approve(plan, document) => {
            document
                .check_snapshot(&plan)
                .map_err(|_| Failure::local("关卡确认无效"))?;
            let gate = document
                .progress
                .gate
                .as_ref()
                .ok_or_else(|| Failure::local("没有待批准关卡"))?;
            let mut query = request(TaskOperation::MixedRolloutApprove, Some(plan.id()));
            query.expected_revision = document.progress.revision;
            query.gate_id = gate.id.as_uuid().as_bytes().to_vec();
            query.confirmed = true;
            let reply = send(config, query).await?;
            let approved = decode_mixed_rollout(&reply.document_json)
                .map_err(|_| Failure::wire("关卡回复不明"))?;
            approved
                .check_snapshot(&plan)
                .map_err(|_| Failure::wire("关卡回复身份不一致"))?;
            if approved.progress.revision != document.progress.revision + 1
                || approved.progress.gate.is_some()
                || !approved.progress.approved.contains(&(gate.next_batch - 1))
            {
                return Err(Failure::wire("关卡批准回复不一致"));
            }
            let page = page(config, &plan, None).await?;
            Ok(Outcome::Loaded(plan, page, false, None))
        }
        Operation::RepairStatus(plan) => {
            Ok(Outcome::RepairStatus(repair_status(config, &plan).await?))
        }
        Operation::Repair(plan, host, step, revision, operation, after) => {
            let op = match operation {
                MixedFileOperation::Probe => TaskOperation::MixedFileProbe,
                MixedFileOperation::Resume => TaskOperation::MixedFileResume,
                MixedFileOperation::Cleanup => TaskOperation::MixedFileCleanup,
                MixedFileOperation::Adopt => TaskOperation::MixedFileAdopt,
            };
            let mut query = request(op, Some(plan.id()));
            query.host_id = host.as_uuid().as_bytes().to_vec();
            query.step = step as u32;
            query.expected_revision = revision;
            query.confirmed = operation != MixedFileOperation::Probe;
            send(config, query).await?;
            let repair = repair_status(config, &plan).await?;
            let page = page(config, &plan, after).await?;
            Ok(Outcome::Loaded(plan, page, false, Some(repair)))
        }
        Operation::Output(plan, key) => {
            let MixedChildSnapshot::Command(child) = plan
                .child(key.host, key.step as usize)
                .map_err(|_| Failure::local("输出目标或步骤无效"))?
            else {
                return Err(Failure::local("文件步骤没有命令输出"));
            };
            if plan.id() != key.run || child.id() != key.child {
                return Err(Failure::local("输出身份无效"));
            }
            let mut query = request(TaskOperation::MixedOutput, Some(key.run));
            query.host_id = key.host.as_uuid().as_bytes().to_vec();
            query.step = key.step;
            query.expected_revision = key.revision;
            query.stderr = key.stderr;
            query.offset = key.offset;
            query.limit = 64 * 1024;
            let reply = send(config, query).await?;
            let context = decode_mixed_output(&reply.document_json, &reply.output)
                .map_err(|_| Failure::wire("混合输出上下文无效"))?;
            if context.run_id != key.run
                || context.host_id != key.host
                || context.child_id != key.child
                || context.step != key.step
                || context.stderr != key.stderr
                || context.offset != key.offset
                || context.target_revision != reply.revision
                || context.length != reply.output_length
                || key.revision != 0 && context.target_revision != key.revision
            {
                return Err(Failure::wire("混合输出范围不一致"));
            }
            Ok(Outcome::Output(MixedOutputPage {
                key,
                page: output_log_page(&reply.output, key.offset),
                context,
            }))
        }
    }
}
async fn repair_status(
    config: &DesktopConnectionConfig,
    plan: &MixedTaskSnapshot,
) -> Result<MixedRepairDocument, Failure> {
    let reply = send(
        config,
        request(TaskOperation::MixedFileStatus, Some(plan.id())),
    )
    .await?;
    let document =
        decode_mixed_repair(&reply.document_json).map_err(|_| Failure::wire("文件修复状态无效"))?;
    if document.run_id != plan.id() {
        return Err(Failure::wire("文件修复运行身份不一致"));
    }
    if let Some(job) = &document.job
        && plan
            .child(job.host_id, job.step)
            .map_err(|_| Failure::wire("文件修复步骤无效"))?
            .id()
            != job.child_id
    {
        return Err(Failure::wire("文件修复子运行不一致"));
    }
    Ok(document)
}
