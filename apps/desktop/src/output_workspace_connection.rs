use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_ipc::{
    Envelope, ExportDirectoryStaging, ExportStaging, ExportTransferControl, Handshake,
    TaskOperation, TaskRequest, TaskResponse, TaskStatus, client_handshake, envelope, features,
    read_envelope, transport, write_envelope,
};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceView {
    pub document: Option<Arc<OutputWorkspaceDocument>>,
    pub index_page: Option<OutputIndexPage>,
    pub page: Option<Arc<OutputWorkspacePage>>,
    pub receipt: Option<OutputRetentionReceipt>,
    pub busy: bool,
    pub status: String,
    pub retention_plan: Option<OutputRetentionPlan>,
    pub write_uncertain: bool,
}
pub(crate) enum Operation {
    Index(OutputAnalysisRequest, OutputIndexEntry, usize),
    Launch(OutputWorkspaceRequest, bool),
    Get(Arc<OutputWorkspaceDocument>, bool),
    Page(Arc<OutputWorkspaceDocument>, usize),
    Export(Arc<OutputWorkspaceDocument>, PathBuf),
    Extract(PathBuf, PathBuf),
    Inspect(PathBuf),
    Receipt(OutputRetentionPlan),
}
pub(crate) struct OutputWorkspaceConnection {
    shared: Arc<Mutex<WorkspaceView>>,
    sender: Option<tokio::sync::mpsc::Sender<Operation>>,
    worker: Option<std::thread::JoinHandle<()>>,
    control: Arc<Mutex<Option<Arc<ExportTransferControl>>>>,
}
impl OutputWorkspaceConnection {
    pub fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(WorkspaceView::default()));
        let state = shared.clone();
        let control = Arc::new(Mutex::new(None));
        let active = control.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let worker = std::thread::Builder::new().name("cshell-output-workspace".into()).spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return; };
            runtime.block_on(async move {
                while let Some(operation) = receiver.recv().await {
                    let mutation = matches!(&operation, Operation::Launch(request, _) if request.action == OutputWorkspaceAction::RetentionApply);
                    let result = perform(&config, operation, &active).await;
                    let mut view = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner); view.busy = false;
                    match result {
                        Ok(Outcome::Document(document)) => {
                            if document.action == OutputWorkspaceAction::RetentionApply && matches!(document.state, OutputAnalysisState::Failed | OutputAnalysisState::Cancelled) { view.write_uncertain = true; }
                            if view.document.as_ref().is_none_or(|old| old.job_id != document.job_id) { view.page = None; view.receipt = None; }
                            view.document = Some(Arc::new(document)); view.status = "作业状态已更新".into();
                        }
                        Ok(Outcome::Page(page)) => { view.page = Some(Arc::new(page)); view.status = "结果页已载入".into(); }
                        Ok(Outcome::Index(page)) => { view.index_page = Some(page); view.status = "持久行索引已载入".into(); }
                        Ok(Outcome::Receipt(receipt)) => { if receipt.states.iter().all(|s| *s == OutputRetentionState::Deleted) { view.write_uncertain = false; } view.receipt = Some(receipt); view.status = "持久保留操作回执已载入".into(); }
                        Ok(Outcome::Message(message)) => view.status = message,
                        Err(error) => {
                            if mutation || view.document.as_ref().is_some_and(|d| d.action == OutputWorkspaceAction::RetentionApply && d.state == OutputAnalysisState::Running) { view.write_uncertain = true; }
                            if error == "作业不存在、已过期或 daemon 已重启" { view.document = None; view.page = None; view.index_page = None; }
                            view.status = format!("{error}；未自动重发，保留操作须核对持久回执");
                        }
                    }
                }
            });
        })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            worker: Some(worker),
            control,
        })
    }
    pub fn view(&self) -> WorkspaceView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn enqueue(&self, operation: Operation) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        let plan = if let Operation::Launch(request, _) = &operation {
            if request.action == OutputWorkspaceAction::RetentionApply && view.write_uncertain {
                return false;
            }
            request.retention.clone()
        } else {
            None
        };
        if matches!(&operation, Operation::Launch(..))
            && view
                .document
                .as_ref()
                .is_some_and(|d| d.state == OutputAnalysisState::Running)
        {
            return false;
        }
        if let Operation::Get(document, _)
        | Operation::Page(document, _)
        | Operation::Export(document, _) = &operation
        {
            if view
                .document
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, document))
            {
                return false;
            }
        }
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(operation).is_ok())
        {
            if plan.is_some() {
                view.retention_plan = plan;
            }
            view.busy = true;
            true
        } else {
            false
        }
    }
    pub fn cancel_transfer(&self) {
        if let Some(control) = self
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            control.request_cancel();
        }
    }
}
impl Drop for OutputWorkspaceConnection {
    fn drop(&mut self) {
        self.cancel_transfer();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
enum Outcome {
    Index(OutputIndexPage),
    Document(OutputWorkspaceDocument),
    Page(OutputWorkspacePage),
    Receipt(OutputRetentionReceipt),
    Message(String),
}
async fn send(
    config: &DesktopConnectionConfig,
    query: TaskRequest,
) -> Result<TaskResponse, String> {
    let operation = TaskOperation::try_from(query.operation).map_err(|_| "操作无效")?;
    let run = query.run_id.clone();
    tokio::time::timeout(Duration::from_secs(15), async {
        let resolved = config.resolve().map_err(|_| "无法发现 daemon")?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| "无法连接 daemon")?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        let required = features::TASK_CONTROL | features::OUTPUT_WORKSPACE;
        handshake.feature_bits = required | features::MIXED_TASK_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon 身份核对失败")?;
        if negotiated.feature_bits & required != required {
            return Err("daemon 不支持输出工作区".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TaskRequest(query)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "请求未确认")?;
        let response = read_envelope(&mut stream).await.map_err(|_| "回复未确认")?;
        let Some(envelope::Payload::TaskResponse(reply)) = response.payload else {
            return Err("回复类型无效".into());
        };
        if reply.status == TaskStatus::Missing as i32 {
            return Err("作业不存在、已过期或 daemon 已重启".into());
        }
        if response.request_id != 2
            || reply.status != TaskStatus::Ok as i32
            || reply.run_id != run
            || reply.revision != 0
            || reply.document_json.len() > MAX_WORKSPACE_DOCUMENT
            || reply.output_length != reply.output.len() as u64
            || if operation == TaskOperation::OutputWorkspaceRead {
                reply.output.len() > MAX_OUTPUT_ARCHIVE_CHUNK
            } else {
                !reply.output.is_empty()
            }
        {
            return Err("请求被拒绝、作业已过期或回复不匹配".into());
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "请求超时".to_owned())?
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
    active: &Arc<Mutex<Option<Arc<ExportTransferControl>>>>,
) -> Result<Outcome, String> {
    if let Operation::Extract(source, destination) = operation {
        let control = Arc::new(ExportTransferControl::default());
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(control.clone());
        let result =
            tokio::task::spawn_blocking(move || extract_local(source, destination, &control))
                .await
                .map_err(|_| "提取后台失败".to_owned())?;
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        return result.map(Outcome::Message);
    }
    if let Operation::Inspect(path) = operation {
        let control = Arc::new(ExportTransferControl::default());
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(control.clone());
        let started = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let mut file = open_container(&path)?;
            let artifact =
                inspect_output_container_checked(&mut file, || local_check(&control, started))
                    .map_err(|_| "容器校验失败或已取消".to_owned())?;
            Ok(Outcome::Message(
                serde_json::to_string_pretty(&artifact).map_err(|_| "描述编码失败")?,
            ))
        })
        .await
        .map_err(|_| "检查后台失败".to_owned())?;
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        return result;
    }
    if let Operation::Export(document, path) = operation {
        let control = Arc::new(ExportTransferControl::default());
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(control.clone());
        let result = export_bundle(config, document, path, &control).await;
        *active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        return result.map(|_| Outcome::Message("归档包已校验并保存".into()));
    }
    let mut query = TaskRequest::default();
    let mut previous = None;
    let mut submitted = None;
    let mut requested_index = None;
    let mut requested_plan = None;
    let mut page_offset = None;
    match operation {
        Operation::Index(scope, entry, offset) => {
            requested_index = Some((scope.clone(), entry.clone(), offset));
            query.operation = TaskOperation::OutputIndexRead as i32;
            query.run_id = scope.run_id.as_uuid().as_bytes().to_vec();
            query.definition_json = serde_json::to_vec(&scope).map_err(|_| "范围无效")?;
            query.expected_targets_json = serde_json::to_vec(&entry).map_err(|_| "索引描述无效")?;
            query.offset = offset as u64;
        }
        Operation::Launch(request, confirmed) => {
            request.validate().map_err(|_| "聚合请求无效")?;
            query.operation = TaskOperation::OutputWorkspace as i32;
            query.run_id = request.sources[0].run_id.as_uuid().as_bytes().to_vec();
            query.definition_json = serde_json::to_vec(&request).map_err(|_| "无法编码请求")?;
            query.confirmed = confirmed;
            submitted = Some(request);
        }
        Operation::Get(document, cancel) => {
            query.run_id = document.sources[0].run_id.as_uuid().as_bytes().to_vec();
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            query.operation = if cancel {
                TaskOperation::OutputWorkspaceCancel
            } else {
                TaskOperation::OutputWorkspaceGet
            } as i32;
            previous = Some(document);
        }
        Operation::Page(document, offset) => {
            page_offset = Some(offset);
            query.run_id = document.sources[0].run_id.as_uuid().as_bytes().to_vec();
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            query.operation = TaskOperation::OutputWorkspacePage as i32;
            query.offset = offset as u64;
            query.expected_targets_json =
                serde_json::to_vec(&*document).map_err(|_| "无法编码作业")?;
            previous = Some(document);
        }
        Operation::Receipt(plan) => {
            requested_plan = Some(plan.clone());
            query.operation = TaskOperation::OutputRetentionReceipt as i32;
            query.run_id = plan.sources[0].run_id.as_uuid().as_bytes().to_vec();
            query.result_id = plan.id.as_uuid().as_bytes().to_vec();
        }
        _ => return Err("操作无效".into()),
    }
    let is_page = query.operation == TaskOperation::OutputWorkspacePage as i32;
    let is_index = query.operation == TaskOperation::OutputIndexRead as i32;
    let is_receipt = query.operation == TaskOperation::OutputRetentionReceipt as i32;
    let reply = send(config, query).await?;
    if is_index {
        let page: OutputIndexPage =
            serde_json::from_slice(&reply.document_json).map_err(|_| "索引页无效")?;
        page.validate().map_err(|_| "索引页不匹配")?;
        if requested_index
            .as_ref()
            .is_none_or(|(scope, entry, offset)| {
                page.scope != *scope || page.entry != *entry || page.offset != *offset
            })
        {
            return Err("索引页范围不匹配".into());
        }
        return Ok(Outcome::Index(page));
    }
    if is_page {
        let page: OutputWorkspacePage =
            serde_json::from_slice(&reply.document_json).map_err(|_| "结果页无效")?;
        page.validate(previous.as_ref().ok_or("缺少作业")?)
            .map_err(|_| "结果页不匹配")?;
        if Some(page.offset) != page_offset {
            return Err("结果页位置不匹配".into());
        }
        return Ok(Outcome::Page(page));
    }
    if is_receipt {
        let receipt: OutputRetentionReceipt =
            serde_json::from_slice(&reply.document_json).map_err(|_| "回执无效")?;
        receipt.validate().map_err(|_| "回执无效")?;
        if Some(&receipt.plan) != requested_plan.as_ref() {
            return Err("持久回执计划不匹配".into());
        }
        return Ok(Outcome::Receipt(receipt));
    }
    let document = decode_output_workspace(&reply.document_json).map_err(|_| "作业描述无效")?;
    if submitted.as_ref().is_some_and(|request| {
        document.sources != request.sources || document.action != request.action
    }) {
        return Err("回复不是已提交的作业范围".into());
    }
    if previous.as_ref().is_some_and(|old| {
        document.job_id != old.job_id
            || document.sources != old.sources
            || document.bindings != old.bindings
            || document.action != old.action
            || document.bytes < old.bytes
            || old.state != OutputAnalysisState::Running && document != **old
    }) {
        return Err("作业范围或进度不匹配".into());
    }
    Ok(Outcome::Document(document))
}
pub(crate) fn open_container(path: &std::path::Path) -> Result<std::fs::File, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| "无法读取容器")?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_OUTPUT_BUNDLE_BYTES {
        return Err("容器须为有界普通文件".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("容器不允许重解析点".into());
        }
    }
    std::fs::File::open(path).map_err(|_| "无法打开容器".into())
}
fn extract_local(
    source: PathBuf,
    destination: PathBuf,
    control: &ExportTransferControl,
) -> Result<String, String> {
    let started = std::time::Instant::now();
    let mut file = open_container(&source)?;
    let artifact = inspect_output_container_checked(&mut file, || local_check(control, started))
        .map_err(|_| "容器校验失败或已取消")?;
    if control.cancelled() {
        return Err("提取已取消".into());
    }
    let staging = ExportDirectoryStaging::prepare(&destination).map_err(|e| e.to_string())?;
    extract_output_container_checked(
        &mut file,
        &artifact,
        || local_check(control, started),
        |manifest, kind| {
            if control.cancelled() {
                return Err(std::io::Error::other("cancelled"));
            }
            let stream = match kind {
                OutputArchiveEntryKind::Stdout => "stdout",
                OutputArchiveEntryKind::Stderr => "stderr",
                OutputArchiveEntryKind::Journal => "journal",
            };
            let name = format!(
                "{}-{}-{}-{stream}.bin",
                manifest.scope.run_id, manifest.scope.left, manifest.scope.step
            );
            staging
                .create(&name)
                .map(|file| Box::new(file) as Box<dyn Write>)
                .map_err(|_| std::io::Error::other("extraction staging"))
        },
    )
    .map_err(|_| "提取校验或暂存失败，未发布目录")?;
    let mut manifest = staging.create("manifest.json").map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(&mut manifest, &artifact).map_err(|_| "描述写入失败")?;
    if !control.begin_publication() {
        return Err("提取已取消".into());
    }
    staging.publish().map_err(|e| e.to_string())?;
    control.finish();
    Ok("提取已完成；文件保存到新目录".into())
}
async fn export_bundle(
    config: &DesktopConnectionConfig,
    document: Arc<OutputWorkspaceDocument>,
    path: PathBuf,
    control: &ExportTransferControl,
) -> Result<(), String> {
    let artifact = document.artifact.as_ref().ok_or("缺少归档包")?.clone();
    let mut staging = ExportStaging::prepare(&path).map_err(|e| e.to_string())?;
    let mut offset = 0_u64;
    let started = std::time::Instant::now();
    while offset < artifact.bytes {
        if control.cancelled() || started.elapsed() > Duration::from_secs(1800) {
            return Err("下载已取消或超时".into());
        }
        let mut query = TaskRequest::default();
        query.operation = TaskOperation::OutputWorkspaceRead as i32;
        query.run_id = document.sources[0].run_id.as_uuid().as_bytes().to_vec();
        query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
        query.offset = offset;
        query.expected_targets_json = serde_json::to_vec(&*document).map_err(|_| "作业无效")?;
        let reply = send(config, query).await?;
        if decode_output_workspace(&reply.document_json).map_err(|_| "下载描述无效")? != *document
            || reply.output.is_empty()
            || offset + reply.output.len() as u64 > artifact.bytes
        {
            return Err("下载数据不匹配".into());
        }
        staging
            .file_mut()
            .map_err(|e| e.to_string())?
            .write_all(&reply.output)
            .map_err(|_| "暂存写入失败")?;
        offset += reply.output.len() as u64;
        control.record_bytes(reply.output.len() as u64);
    }
    staging
        .file_mut()
        .map_err(|e| e.to_string())?
        .seek(SeekFrom::Start(0))
        .map_err(|_| "暂存读取失败")?;
    if inspect_output_container_checked(staging.file_mut().map_err(|e| e.to_string())?, || {
        local_check(control, started)
    })
    .map_err(|_| "下载整包校验失败")?
        != OutputContainerArtifact::Bundle(artifact)
    {
        return Err("归档包不匹配".into());
    }
    if !control.begin_publication() {
        return Err("下载已取消".into());
    }
    staging.publish().map_err(|e| e.to_string())?;
    control.finish();
    Ok(())
}
fn local_check(
    control: &ExportTransferControl,
    started: std::time::Instant,
) -> std::io::Result<()> {
    if control.cancelled() || started.elapsed() > Duration::from_secs(1800) {
        Err(std::io::Error::other(
            "local output work cancelled/deadline",
        ))
    } else {
        Ok(())
    }
}
