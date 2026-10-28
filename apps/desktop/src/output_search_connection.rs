//! Search query bytes live only in the queued launch, not in cached job replies.
use crate::{daemon_connection::DesktopConnectionConfig, output_analysis_connection::send};
use cshell_application::*;
use cshell_domain::TaskRunId;
use cshell_ipc::{TaskOperation, TaskRequest};
use std::sync::{Arc, Mutex};
#[derive(Clone, Debug, Default)]
pub(crate) struct SearchView {
    pub document: Option<Arc<OutputSearchDocument>>,
    pub busy: bool,
    pub status: String,
}
enum Operation {
    Launch(OutputSearchRequest, [u8; 32], TaskRunId),
    Get(Arc<OutputSearchDocument>, bool),
}
pub(crate) struct OutputSearchConnection {
    shared: Arc<Mutex<SearchView>>,
    sender: Option<tokio::sync::mpsc::Sender<Operation>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl OutputSearchConnection {
    pub fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(SearchView::default()));
        let state = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Operation>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-output-search-ui".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .status = "无法启动输出检索连接".into();
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
                                view.status = "输出检索状态已载入".into();
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
    pub fn view(&self) -> SearchView {
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
                .is_none_or(|saved| !Arc::ptr_eq(saved, document))
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
    pub fn launch(&self, request: OutputSearchRequest, hash: [u8; 32], child: TaskRunId) -> bool {
        self.enqueue(Operation::Launch(request, hash, child))
    }
    pub fn get(&self, document: Arc<OutputSearchDocument>, cancel: bool) -> bool {
        self.enqueue(Operation::Get(document, cancel))
    }
}
impl Drop for OutputSearchConnection {
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
) -> Result<OutputSearchDocument, String> {
    let mut query = TaskRequest::default();
    let (expected, hash, child, previous) = match operation {
        Operation::Launch(request, hash, child) => {
            let expected = request.descriptor().map_err(|_| "检索范围或查询字节无效")?;
            query.operation = TaskOperation::OutputSearch as i32;
            query.run_id = request.scope.run_id.as_uuid().as_bytes().to_vec();
            query.definition_json = serde_json::to_vec(&request).map_err(|_| "检索请求无效")?;
            (expected, hash, child, None)
        }
        Operation::Get(document, cancel) => {
            query.operation = if cancel {
                TaskOperation::OutputSearchCancel
            } else {
                TaskOperation::OutputSearchGet
            } as i32;
            query.run_id = document.request.scope.run_id.as_uuid().as_bytes().to_vec();
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            (
                document.request.clone(),
                document.binding.snapshot_sha256,
                document.binding.child_id,
                Some(document),
            )
        }
    };
    let reply = send(config, query, expected.scope.kind).await?;
    let document = decode_output_search(&reply.document_json).map_err(|_| "检索作业状态无效")?;
    if document.request != expected
        || document.binding.snapshot_sha256 != hash
        || document.binding.child_id != child
        || previous.as_ref().is_some_and(|previous| {
            document.job_id != previous.job_id
                || document.binding != previous.binding
                || document.bytes < previous.bytes
                || previous.state != OutputAnalysisState::Running && *previous.as_ref() != document
        })
    {
        return Err("检索作业身份或进度不一致".into());
    }
    Ok(document)
}
