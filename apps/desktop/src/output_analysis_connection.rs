//! Independent background control for readonly stored-output jobs.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::TaskRunId;
use cshell_ipc::{
    Envelope, Handshake, TaskOperation, TaskRequest, TaskResponse, TaskStatus, client_handshake,
    envelope, features, read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Debug, Default)]
pub(crate) struct AnalysisView {
    pub document: Option<Arc<OutputAnalysisDocument>>,
    pub busy: bool,
    pub status: String,
}
enum Operation {
    Launch(
        OutputAnalysisRequest,
        [u8; 32],
        TaskRunId,
        Option<TaskRunId>,
    ),
    Get(Arc<OutputAnalysisDocument>, bool),
}
pub(crate) struct OutputAnalysisConnection {
    shared: Arc<Mutex<AnalysisView>>,
    sender: Option<tokio::sync::mpsc::Sender<Operation>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl OutputAnalysisConnection {
    pub fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(AnalysisView::default()));
        let state = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Operation>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-output-analysis-ui".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .status = "无法启动输出分析连接".into();
                    return;
                };
                runtime.block_on(async move {
                    while let Some(operation) = receiver.recv().await {
                        let result = perform(&config, operation).await;
                        let mut view = state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        view.busy = false;
                        match result {
                            Ok(document) => {
                                view.status = "输出分析状态已载入".into();
                                view.document = Some(Arc::new(document));
                            }
                            Err(error) => {
                                if error == "输出分析作业已过期或 daemon 已重启" {
                                    view.document = None;
                                }
                                view.status = format!("{error}；未自动重发请求");
                            }
                        }
                    }
                });
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> AnalysisView {
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
        if let Operation::Launch(..) = &operation
            && view
                .document
                .as_ref()
                .is_some_and(|document| document.state == OutputAnalysisState::Running)
        {
            return false;
        }
        if let Operation::Get(document, _) = &operation
            && view
                .document
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, document))
        {
            return false;
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
    pub fn launch(
        &self,
        request: OutputAnalysisRequest,
        hash: [u8; 32],
        left: TaskRunId,
        right: Option<TaskRunId>,
    ) -> bool {
        self.enqueue(Operation::Launch(request, hash, left, right))
    }
    pub fn get(&self, document: Arc<OutputAnalysisDocument>, cancel: bool) -> bool {
        self.enqueue(Operation::Get(document, cancel))
    }
}
impl Drop for OutputAnalysisConnection {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
) -> Result<OutputAnalysisDocument, String> {
    let mut query = TaskRequest::default();
    let (expected, hash, left, right, previous) = match operation {
        Operation::Launch(request, hash, left, right) => {
            request.validate().map_err(|_| "输出分析范围无效")?;
            query.operation = TaskOperation::OutputAnalyze as i32;
            query.run_id = request.run_id.as_uuid().as_bytes().to_vec();
            query.definition_json = serde_json::to_vec(&request).map_err(|_| "输出分析范围无效")?;
            (request, hash, left, right, None)
        }
        Operation::Get(document, cancel) => {
            query.operation = if cancel {
                TaskOperation::OutputAnalysisCancel
            } else {
                TaskOperation::OutputAnalysisGet
            } as i32;
            query.run_id = document.request.run_id.as_uuid().as_bytes().to_vec();
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            (
                document.request.clone(),
                document.left.snapshot_sha256,
                document.left.child_id,
                document.right.as_ref().map(|binding| binding.child_id),
                Some(document),
            )
        }
    };
    let reply = send(config, query, expected.kind).await?;
    let document = decode_output_analysis(&reply.document_json).map_err(|_| "输出分析状态无效")?;
    if document.request != expected
        || document.left.snapshot_sha256 != hash
        || document.left.child_id != left
        || document.right.as_ref().map(|binding| binding.child_id) != right
        || previous.as_ref().is_some_and(|previous| {
            document.job_id != previous.job_id
                || document.left != previous.left
                || document.right != previous.right
                || document.left_bytes < previous.left_bytes
                || document
                    .right_bytes
                    .zip(previous.right_bytes)
                    .is_some_and(|(now, before)| now < before)
                || previous.state != OutputAnalysisState::Running && *previous.as_ref() != document
        })
    {
        return Err("输出分析身份或进度不一致".into());
    }
    Ok(document)
}
pub(crate) async fn send(
    config: &DesktopConnectionConfig,
    query: TaskRequest,
    kind: OutputAnalysisKind,
) -> Result<TaskResponse, String> {
    let expected = query.run_id.clone();
    let operation = TaskOperation::try_from(query.operation).map_err(|_| "输出作业操作无效")?;
    if !operation.is_output_analysis()
        && !operation.is_output_search()
        && !operation.is_output_merge()
        && !operation.is_output_archive()
    {
        return Err("输出作业类型不一致".into());
    }
    let observation_feature = if operation.is_output_archive() {
        features::TASK_OUTPUT_ARCHIVE
    } else if operation.is_output_merge() {
        features::TASK_OUTPUT_MERGE
    } else if operation.is_output_search() {
        features::TASK_OUTPUT_SEARCH
    } else {
        features::TASK_OUTPUT_ANALYSIS
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let endpoint = config.resolve().map_err(|_| "无法发现 daemon")?;
        let mut stream = transport::connect(&endpoint.endpoint)
            .await
            .map_err(|_| "无法连接 daemon")?;
        let mut handshake = Handshake::new(
            endpoint.daemon_instance_id.to_vec(),
            endpoint.instance_token.to_vec(),
        );
        let required = features::TASK_CONTROL
            | observation_feature
            | if kind == OutputAnalysisKind::Mixed {
                features::MIXED_TASK_CONTROL
            } else {
                0
            };
        handshake.feature_bits = required;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon 身份验证失败")?;
        if negotiated.feature_bits & required != required {
            return Err("daemon 不支持完整输出分析".into());
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
        .map_err(|_| "输出分析请求发送中断")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "输出分析回复中断")?;
        if reply.request_id != 2 {
            return Err("输出分析回复编号不一致".into());
        }
        let Some(envelope::Payload::TaskResponse(reply)) = reply.payload else {
            return Err("输出分析回复类型不一致".into());
        };
        if reply.status != TaskStatus::Ok as i32 {
            return Err(match TaskStatus::try_from(reply.status) {
                Ok(TaskStatus::Busy) => "输出分析 worker 已满",
                Ok(TaskStatus::Conflict) => "范围版本变化或命令尚未停止",
                Ok(TaskStatus::Missing) if operation.is_output_merge() => {
                    "此输出没有可用的时序日志（旧运行或输出文件缺失）"
                }
                Ok(TaskStatus::Missing) => "输出分析作业已过期或 daemon 已重启",
                _ => "输出分析请求被拒绝",
            }
            .into());
        }
        if reply.run_id != expected
            || reply.revision != 0
            || if operation == TaskOperation::OutputArchiveRead {
                reply.output.len() > cshell_application::MAX_OUTPUT_ARCHIVE_CHUNK
                    || reply.output_length != reply.output.len() as u64
            } else if operation.is_output_merge() {
                reply.output.len() > cshell_application::MAX_OUTPUT_MERGE_BYTES
                    || reply.output_length != reply.output.len() as u64
            } else {
                !reply.output.is_empty() || reply.output_length != 0
            }
        {
            return Err("输出分析回复字段不一致".into());
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "输出分析请求超时".to_owned())?
}
