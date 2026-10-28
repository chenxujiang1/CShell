//! Bounded task control over authenticated IPC. Execution belongs to the daemon.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::{HostId, HostSelection, TaskDefinition, TaskGateId, TaskRunId};
use cshell_ipc::{
    Envelope, Handshake, TaskOperation, TaskRequest, TaskResponse, TaskStatus, client_handshake,
    envelope, features, read_envelope, transport, write_envelope,
};
use cshell_render::{LogPage, LogRow, LogSourceId};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) const OUTPUT_PAGE_BYTES: u32 = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OutputKey {
    pub(crate) run: TaskRunId,
    pub(crate) host: HostId,
    pub(crate) step: u32,
    pub(crate) stderr: bool,
    pub(crate) offset: u64,
}
#[derive(Clone, Debug)]
pub(crate) struct OutputPage {
    pub(crate) key: OutputKey,
    pub(crate) next: u64,
    pub(crate) length: u64,
    pub(crate) page: Arc<LogPage>,
}
#[derive(Clone)]
pub(crate) struct ComparisonPage {
    pub(crate) comparison: Arc<TaskOutputComparison>,
    pub(crate) page: Arc<LogPage>,
}
impl std::fmt::Debug for ComparisonPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComparisonPage")
            .field("comparison", &self.comparison)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Default)]
pub(crate) struct TaskView {
    pub(crate) rows: Arc<[TaskRunSummary]>,
    pub(crate) after: Option<TaskRunId>,
    pub(crate) detail: Option<Arc<TaskRunView>>,
    pub(crate) plan: Option<TaskSnapshot>,
    pub(crate) output: Option<OutputPage>,
    pub(crate) results: Option<Arc<TaskResultDocument>>,
    pub(crate) comparison: Option<ComparisonPage>,
    pub(crate) writable: bool,
    pub(crate) busy: bool,
    pub(crate) generation: u64,
    pub(crate) status: String,
}
enum Operation {
    List(Option<TaskRunId>),
    Inspect(TaskRunId),
    Refresh(TaskRunId),
    Create(TaskDefinition, HostSelection, u64, u64),
    Start(TaskRunId, u64),
    Approve(TaskRunId, u64, TaskGateId),
    Retry(TaskRunId, u64, BTreeSet<HostId>, bool),
    Output(OutputKey),
    Compare(OutputKey, HostId, u64),
    Export(
        Arc<TaskResultDocument>,
        TaskResultExportFormat,
        std::path::PathBuf,
    ),
}
impl Operation {
    fn mutation(&self) -> bool {
        matches!(
            self,
            Self::Create(..) | Self::Start(..) | Self::Approve(..) | Self::Retry(..)
        )
    }
}
pub(crate) struct TaskConnection {
    pub(crate) uploads: Arc<crate::upload_connection::UploadConnection>,
    pub(crate) downloads: Arc<crate::download_connection::DownloadConnection>,
    pub(crate) mixed: Arc<crate::mixed_connection::MixedConnection>,
    pub(crate) analysis: Arc<crate::output_analysis_connection::OutputAnalysisConnection>,
    pub(crate) search: Arc<crate::output_search_connection::OutputSearchConnection>,
    pub(crate) merge: Arc<crate::output_merge_connection::OutputMergeConnection>,
    pub(crate) archives: Arc<crate::output_archive_connection::OutputArchiveConnection>,
    pub(crate) workspace: Arc<crate::output_workspace_connection::OutputWorkspaceConnection>,
    shared: Arc<Mutex<TaskView>>,
    sender: Option<tokio::sync::mpsc::Sender<Operation>>,
    cancel_sender: Option<tokio::sync::mpsc::Sender<TaskRunId>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl TaskConnection {
    pub(crate) fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let workspace = Arc::new(
            crate::output_workspace_connection::OutputWorkspaceConnection::start(config.clone())?,
        );
        let archives = Arc::new(
            crate::output_archive_connection::OutputArchiveConnection::start(config.clone())?,
        );
        let merge =
            Arc::new(crate::output_merge_connection::OutputMergeConnection::start(config.clone())?);
        let search = Arc::new(
            crate::output_search_connection::OutputSearchConnection::start(config.clone())?,
        );
        let analysis = Arc::new(
            crate::output_analysis_connection::OutputAnalysisConnection::start(config.clone())?,
        );
        let mixed = Arc::new(crate::mixed_connection::MixedConnection::start(
            config.clone(),
            analysis.clone(),
            search.clone(),
            merge.clone(),
            archives.clone(),
            workspace.clone(),
        )?);
        let downloads = Arc::new(crate::download_connection::DownloadConnection::start(
            config.clone(),
        )?);
        let uploads = Arc::new(crate::upload_connection::UploadConnection::start(
            config.clone(),
        )?);
        let shared = Arc::new(Mutex::new(TaskView {
            busy: true,
            ..Default::default()
        }));
        let state = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let (cancel_sender, mut cancels) = tokio::sync::mpsc::channel::<TaskRunId>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-tasks".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let mut view = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.busy = false;
                    view.status = "无法启动任务连接".into();
                    return;
                };
                runtime.block_on(async move {
                    let cancel_config = config.clone();
                    let cancel_state = state.clone();
                    let cancellation = tokio::spawn(async move {
                        while let Some(id) = cancels.recv().await {
                            let mut request = task_request(TaskOperation::Cancel, Some(id));
                            request.confirmed = false;
                            let result = send(&cancel_config, request).await;
                            let mut view = cancel_state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.generation = view.generation.wrapping_add(1);
                            view.status = match result {
                                Ok(_) => "取消请求已发送；刷新查看最终状态".into(),
                                Err(error) => format!("{error}；请刷新状态，未重发取消"),
                            };
                        }
                    });
                    let mut operation = Operation::List(None);
                    loop {
                        let mutation = operation.mutation();
                        let result = perform(&config, operation).await;
                        {
                            let mut view = state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = false;
                            view.generation = view.generation.wrapping_add(1);
                            match result {
                                Ok(Outcome::List(rows, after)) => {
                                    view.rows = rows.into();
                                    view.after = after;
                                    view.status = "任务列表已加载；选择任务查看计划与结果".into();
                                }
                                Ok(Outcome::Inspected(document, plan)) => {
                                    view.detail = Some(Arc::new(document.view()));
                                    view.results = Some(Arc::new(document));
                                    view.plan = Some(plan);
                                    view.output = None;
                                    view.comparison = None;
                                    view.writable = true;
                                    view.status =
                                        "已加载冻结计划；执行前核对目标、命令和策略".into();
                                }
                                Ok(Outcome::Results(document)) => {
                                    if view
                                        .detail
                                        .as_ref()
                                        .is_some_and(|old| old.id == document.run_id)
                                    {
                                        view.detail = Some(Arc::new(document.view()));
                                        view.results = Some(Arc::new(document));
                                    }
                                    view.status = "任务状态已刷新".into();
                                }
                                Ok(Outcome::Output(mut page)) => {
                                    Arc::make_mut(&mut page.page).revision = view.generation;
                                    view.output = Some(page);
                                    view.status = "已读取输出页；分页内容不代表执行完成".into();
                                }
                                Ok(Outcome::Comparison(mut page)) => {
                                    Arc::make_mut(&mut page.page).revision = view.generation;
                                    view.comparison = Some(page);
                                    view.status = "已比较输出页；此结果只覆盖显示范围".into();
                                }
                                Ok(Outcome::Exported) => {
                                    view.status = "结果快照已保存到新文件，包含全部目标".into();
                                }
                                Err(error) => {
                                    if mutation {
                                        view.writable = false;
                                        view.plan = None;
                                    }
                                    view.status =
                                        format!("{error}；请重新加载任务，未自动重放请求");
                                }
                            }
                        }
                        let Some(next) = receiver.recv().await else {
                            break;
                        };
                        operation = next;
                    }
                    let _result = cancellation.await;
                });
            })?;
        Ok(Self {
            workspace,
            archives,
            merge,
            analysis,
            search,
            uploads,
            downloads,
            mixed,
            shared,
            sender: Some(sender),
            cancel_sender: Some(cancel_sender),
            worker: Some(worker),
        })
    }
    pub(crate) fn view(&self) -> TaskView {
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
        if view.busy {
            return false;
        }
        match &operation {
            Operation::Start(id, revision)
            | Operation::Approve(id, revision, _)
            | Operation::Retry(id, revision, _, _) => {
                if !view.writable
                    || view.plan.as_ref().is_none_or(|plan| plan.id() != *id)
                    || view
                        .detail
                        .as_ref()
                        .is_none_or(|detail| detail.id != *id || detail.meta.revision != *revision)
                {
                    return false;
                }
            }
            Operation::Compare(key, _, revision) => {
                if view.detail.as_ref().is_none_or(|detail| {
                    detail.id != key.run
                        || detail.meta.revision != *revision
                        || !task_terminal(detail.meta.state)
                        || detail.runner_active
                }) {
                    return false;
                }
            }
            _ => {}
        }
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(operation).is_ok())
        {
            view.busy = true;
            true
        } else {
            false
        }
    }
    pub(crate) fn list(&self, after: Option<TaskRunId>) -> bool {
        self.enqueue(Operation::List(after))
    }
    pub(crate) fn inspect(&self, id: TaskRunId) -> bool {
        self.enqueue(Operation::Inspect(id))
    }
    pub(crate) fn refresh(&self, id: TaskRunId) -> bool {
        self.enqueue(Operation::Refresh(id))
    }
    pub(crate) fn create(
        &self,
        definition: TaskDefinition,
        selection: HostSelection,
        hosts: u64,
        profiles: u64,
    ) -> bool {
        self.enqueue(Operation::Create(definition, selection, hosts, profiles))
    }
    pub(crate) fn start_run(&self, id: TaskRunId, revision: u64) -> bool {
        self.enqueue(Operation::Start(id, revision))
    }
    pub(crate) fn approve(&self, id: TaskRunId, revision: u64, gate: TaskGateId) -> bool {
        self.enqueue(Operation::Approve(id, revision, gate))
    }
    pub(crate) fn retry(
        &self,
        id: TaskRunId,
        revision: u64,
        hosts: BTreeSet<HostId>,
        unknown: bool,
    ) -> bool {
        self.enqueue(Operation::Retry(id, revision, hosts, unknown))
    }
    pub(crate) fn output(&self, key: OutputKey) -> bool {
        self.enqueue(Operation::Output(key))
    }
    pub(crate) fn compare(&self, key: OutputKey, right: HostId, revision: u64) -> bool {
        self.enqueue(Operation::Compare(key, right, revision))
    }
    pub(crate) fn export(
        &self,
        document: Arc<TaskResultDocument>,
        format: TaskResultExportFormat,
        path: std::path::PathBuf,
    ) -> bool {
        self.enqueue(Operation::Export(document, format, path))
    }
    pub(crate) fn cancel(&self, id: TaskRunId) -> bool {
        self.cancel_sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(id).is_ok())
    }
}
impl Drop for TaskConnection {
    fn drop(&mut self) {
        self.sender.take();
        self.cancel_sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
fn task_request(operation: TaskOperation, id: Option<TaskRunId>) -> TaskRequest {
    let mut request = TaskRequest::default();
    request.operation = operation as i32;
    if let Some(id) = id {
        request.run_id = id.as_uuid().as_bytes().to_vec();
    }
    request
}
enum Outcome {
    List(Vec<TaskRunSummary>, Option<TaskRunId>),
    Inspected(TaskResultDocument, TaskSnapshot),
    Results(TaskResultDocument),
    Output(OutputPage),
    Comparison(ComparisonPage),
    Exported,
}
async fn results(
    config: &DesktopConnectionConfig,
    id: TaskRunId,
) -> Result<TaskResultDocument, String> {
    let reply = send(config, task_request(TaskOperation::Results, Some(id))).await?;
    let view = decode_task_results(&reply.document_json).map_err(|_| "任务结果快照无效")?;
    if view.run_id != id || view.meta.revision != reply.revision {
        return Err("任务状态版本不一致".into());
    }
    Ok(view)
}
fn check_plan(plan: &TaskSnapshot, document: &TaskResultDocument) -> Result<(), String> {
    let view = document.view();
    let planned: BTreeSet<_> = plan
        .targets()
        .targets()
        .iter()
        .map(|host| host.id)
        .collect();
    let actual: BTreeSet<_> = view.targets.iter().map(|target| target.host_id).collect();
    if plan.id() != view.id
        || plan.definition().name != view.name
        || planned != actual
        || view
            .targets
            .iter()
            .any(|target| target.steps.len() != plan.definition().steps.len())
        || document.host_revision != plan.targets().host_revision()
        || document.profile_revision != plan.targets().profile_revision()
        || document.retry_of != plan.retry_of()
        || plan.targets().targets().iter().any(|host| {
            document.target(host.id).is_none_or(|target| {
                target.name != host.name
                    || target.address != host.address
                    || target.port != host.port
                    || target.profile_id != host.profile_id
            })
        })
        || document
            .steps
            .iter()
            .zip(&plan.definition().steps)
            .any(|(reported, step)| {
                reported.name != step.name
                    || reported.success_codes != step.success_codes
                    || reported.idempotent != step.idempotent
            })
    {
        return Err("任务计划与状态不一致".into());
    }
    Ok(())
}
async fn inspect(config: &DesktopConnectionConfig, id: TaskRunId) -> Result<Outcome, String> {
    let reply = send(config, task_request(TaskOperation::Review, Some(id))).await?;
    let plan = decode_task_snapshot(&reply.document_json).map_err(|_| "任务计划无效")?;
    let view = results(config, id).await?;
    if reply.revision == 0 || reply.revision > view.meta.revision {
        return Err("任务计划版本不一致".into());
    }
    check_plan(&plan, &view)?;
    Ok(Outcome::Inspected(view, plan))
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
) -> Result<Outcome, String> {
    let mut request = TaskRequest::default();
    match operation {
        Operation::List(after) => {
            request = task_request(TaskOperation::List, after);
            request.limit = MAX_TASK_LIST_RESULTS as u32;
            let reply = send(config, request).await?;
            let rows = decode_task_list(&reply.document_json).map_err(|_| "任务列表无效")?;
            if after.is_some_and(|id| rows.iter().any(|row| row.id <= id)) {
                return Err("任务列表游标无效".into());
            }
            return Ok(Outcome::List(rows, after));
        }
        Operation::Inspect(id) => return inspect(config, id).await,
        Operation::Refresh(id) => return results(config, id).await.map(Outcome::Results),
        Operation::Create(definition, selection, hosts, profiles) => {
            validate_task_definition(&definition).map_err(|error| error.to_string())?;
            request.operation = TaskOperation::Create as i32;
            request.definition_json =
                serde_json::to_vec(&definition).map_err(|_| "任务定义无效")?;
            request.selection_json =
                encode_host_selection(&selection).map_err(|_| "主机选择无效")?;
            request.host_revision = hosts;
            request.profile_revision = profiles;
            let reply = send(config, request).await?;
            let plan = decode_task_snapshot(&reply.document_json).map_err(|_| "保存的计划无效")?;
            if plan.definition() != &definition
                || plan.targets().selection() != &selection
                || plan.targets().host_revision() != hosts
                || plan.targets().profile_revision() != profiles
                || reply.revision != 1
                || plan.id().as_uuid().as_bytes().as_slice() != reply.run_id
            {
                return Err("保存计划与提交内容不一致，结果不明".into());
            }
            let view = results(config, plan.id()).await?;
            check_plan(&plan, &view)?;
            return Ok(Outcome::Inspected(view, plan));
        }
        Operation::Start(id, revision) => {
            request = task_request(TaskOperation::Start, Some(id));
            request.expected_revision = revision;
            request.confirmed = true;
        }
        Operation::Approve(id, revision, gate) => {
            request = task_request(TaskOperation::Approve, Some(id));
            request.expected_revision = revision;
            request.gate_id = gate.as_uuid().as_bytes().to_vec();
            request.confirmed = true;
        }
        Operation::Retry(id, revision, hosts, unknown) => {
            request = task_request(TaskOperation::Retry, Some(id));
            request.expected_revision = revision;
            request.retry_hosts = hosts
                .iter()
                .map(|host| host.as_uuid().as_bytes().to_vec())
                .collect();
            request.confirmed = true;
            request.confirm_unknown = unknown;
            let reply = send(config, request).await?;
            let plan = decode_task_snapshot(&reply.document_json).map_err(|_| "重试计划无效")?;
            let returned: BTreeSet<_> = plan
                .targets()
                .targets()
                .iter()
                .map(|host| host.id)
                .collect();
            if plan.id() == id
                || plan.retry_of() != Some(id)
                || returned != hosts
                || reply.revision != 1
                || plan.id().as_uuid().as_bytes().as_slice() != reply.run_id
            {
                return Err("重试计划身份不一致，结果不明".into());
            }
            let view = results(config, plan.id()).await?;
            check_plan(&plan, &view)?;
            return Ok(Outcome::Inspected(view, plan));
        }
        Operation::Output(key) => {
            request = task_request(TaskOperation::Output, Some(key.run));
            request.host_id = key.host.as_uuid().as_bytes().to_vec();
            request.step = key.step;
            request.stderr = key.stderr;
            request.offset = key.offset;
            request.limit = OUTPUT_PAGE_BYTES;
            let reply = send(config, request).await?;
            let next = key
                .offset
                .checked_add(reply.output.len() as u64)
                .ok_or("输出偏移溢出")?;
            if reply.output.len() > OUTPUT_PAGE_BYTES as usize || next > reply.output_length {
                return Err("输出分页边界无效".into());
            }
            let page = output_log_page(&reply.output, key.offset);
            return Ok(Outcome::Output(OutputPage {
                key,
                next,
                length: reply.output_length,
                page,
            }));
        }
        Operation::Export(document, format, path) => {
            tokio::task::spawn_blocking(move || {
                cshell_ipc::publish_export_file(&path, |file| {
                    write_task_result_export(&document, format, file)
                })
            })
            .await
            .map_err(|_| "导出 worker 失败，核对目标文件后再操作")?
            .map_err(|error| error.to_string())?;
            return Ok(Outcome::Exported);
        }
        Operation::Compare(key, right, revision) => {
            request = task_request(TaskOperation::Compare, Some(key.run));
            request.host_id = key.host.as_uuid().as_bytes().to_vec();
            request.compare_host_id = right.as_uuid().as_bytes().to_vec();
            request.expected_revision = revision;
            request.step = key.step;
            request.stderr = key.stderr;
            request.offset = key.offset;
            request.limit = OUTPUT_PAGE_BYTES;
            let reply = send(config, request).await?;
            let comparison =
                decode_task_comparison(&reply.document_json).map_err(|_| "输出比较回复无效")?;
            if comparison.run_id != key.run
                || comparison.revision != revision
                || reply.revision != revision
                || comparison.left_host != key.host
                || comparison.right_host != right
                || comparison.step != key.step
                || comparison.stderr != key.stderr
                || comparison.offset != key.offset
            {
                return Err("输出比较上下文不一致".into());
            }
            let page = comparison_log_page(&comparison);
            return Ok(Outcome::Comparison(ComparisonPage {
                comparison: Arc::new(comparison),
                page,
            }));
        }
    }
    let id =
        TaskRunId::from_bytes(cshell_ipc::decode_id(&request.run_id).map_err(|_| "任务编号无效")?);
    send(config, request).await?;
    results(config, id).await.map(Outcome::Results)
}
// Byte inspection is deliberately separate from terminal parsing. Each row has at
// most 32 source bytes. Controls, Unicode bytes and backslashes are unambiguous.
pub(crate) fn output_log_page(bytes: &[u8], offset: u64) -> Arc<LogPage> {
    use std::fmt::Write;
    let mut rows = Vec::with_capacity(bytes.len().div_ceil(32).max(1));
    for (index, chunk) in bytes.chunks(32).enumerate() {
        let mut text = format!("{:012x}  ", offset + (index * 32) as u64);
        for byte in chunk {
            match byte {
                b' '..=b'~' if *byte != b'\\' => text.push(char::from(*byte)),
                b'\\' => text.push_str("\\\\"),
                _ => {
                    let _written = write!(text, "\\x{byte:02x}");
                }
            }
        }
        rows.push(LogRow {
            soft_wrap: false,
            line_id: index as u64 + 1,
            text: text.into(),
            style_spans: Arc::from([]),
            truncated: false,
        });
    }
    if rows.is_empty() {
        rows.push(LogRow {
            soft_wrap: false,
            line_id: 1,
            text: "当前偏移没有输出".into(),
            style_spans: Arc::from([]),
            truncated: false,
        });
    }
    Arc::new(LogPage { first_line_id: 1,
        source_id: LogSourceId(0),
        revision: 0,
        anchor_line_id: 1,
        total_line_count: rows.len() as u64,
        rows: rows.into(),
        has_before: false,
        has_after: false,
    })
}
fn comparison_log_page(comparison: &TaskOutputComparison) -> Arc<LogPage> {
    use std::fmt::Write;
    let count = comparison
        .left
        .len()
        .max(comparison.right.len())
        .div_ceil(32);
    let mut rows = Vec::with_capacity((count * 2).max(1));
    for row in 0..count {
        let start = row * 32;
        let end = start + 32;
        let left = comparison
            .left
            .get(start..end.min(comparison.left.len()))
            .unwrap_or_default();
        let right = comparison
            .right
            .get(start..end.min(comparison.right.len()))
            .unwrap_or_default();
        let marker = if left == right { '=' } else { '!' };
        for (side, bytes) in [('L', left), ('R', right)] {
            let mut text = format!("{marker}{side} {:012x}  ", comparison.offset + start as u64);
            if bytes.is_empty() {
                text.push_str("<EOF>");
            }
            for byte in bytes {
                match byte {
                    b' '..=b'~' if *byte != b'\\' => text.push(char::from(*byte)),
                    b'\\' => text.push_str("\\\\"),
                    _ => {
                        let _written = write!(text, "\\x{byte:02x}");
                    }
                }
            }
            rows.push(LogRow {
            soft_wrap: false,
                line_id: rows.len() as u64 + 1,
                text: text.into(),
                style_spans: Arc::from([]),
                truncated: false,
            });
        }
    }
    if rows.is_empty() {
        rows.push(LogRow {
            soft_wrap: false,
            line_id: 1,
            text: "所选范围两侧均无字节；请核对覆盖范围".into(),
            style_spans: Arc::from([]),
            truncated: false,
        });
    }
    Arc::new(LogPage { first_line_id: 1,
        source_id: LogSourceId(0),
        revision: 0,
        anchor_line_id: 1,
        total_line_count: rows.len() as u64,
        rows: rows.into(),
        has_before: false,
        has_after: false,
    })
}
async fn send(
    config: &DesktopConnectionConfig,
    request: TaskRequest,
) -> Result<TaskResponse, String> {
    let operation = TaskOperation::try_from(request.operation).map_err(|_| "任务操作无效")?;
    let expected = request.run_id.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        let endpoint = config.resolve().map_err(|_| "无法发现 daemon")?;
        let mut stream = transport::connect(&endpoint.endpoint)
            .await
            .map_err(|_| "无法连接 daemon")?;
        let mut handshake = Handshake::new(
            endpoint.daemon_instance_id.to_vec(),
            endpoint.instance_token.to_vec(),
        );
        handshake.feature_bits =
            features::TASK_CONTROL | features::TASK_PLAN_CONTROL | features::TASK_RESULTS;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon 身份或协议验证失败")?;
        if negotiated.feature_bits & features::TASK_CONTROL == 0
            || matches!(operation, TaskOperation::Review | TaskOperation::Retry)
                && negotiated.feature_bits & features::TASK_PLAN_CONTROL == 0
            || matches!(operation, TaskOperation::Results | TaskOperation::Compare)
                && negotiated.feature_bits & features::TASK_RESULTS == 0
        {
            return Err("当前 daemon 不支持此任务操作".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TaskRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "任务发送中断，结果可能不明")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "任务回复中断，结果可能不明")?;
        if reply.request_id != 2 {
            return Err("任务回复编号不匹配".into());
        }
        let Some(envelope::Payload::TaskResponse(reply)) = reply.payload else {
            return Err("任务回复类型不匹配".into());
        };
        if TaskStatus::try_from(reply.status) != Ok(TaskStatus::Ok) {
            return Err(match TaskStatus::try_from(reply.status) {
                Ok(TaskStatus::Conflict) => "任务或主机版本已变化，请重新加载",
                Ok(TaskStatus::Busy) => "任务已有执行者",
                Ok(TaskStatus::Invalid) => "任务请求无效或重试条件不满足",
                Ok(TaskStatus::Missing) => "任务不存在",
                Ok(TaskStatus::Unsupported) => "daemon 不支持此任务操作",
                Ok(TaskStatus::Limit) => "任务超过资源限制",
                Ok(TaskStatus::Corrupt) => "任务存储损坏",
                _ => "任务服务不可用，结果可能不明",
            }
            .into());
        }
        if !matches!(
            operation,
            TaskOperation::List | TaskOperation::Create | TaskOperation::Retry
        ) && reply.run_id != expected
        {
            return Err("任务回复身份不匹配".into());
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "任务请求超时，结果可能不明".to_owned())?
}
