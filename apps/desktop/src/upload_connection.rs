//! Independent bounded upload control and background local manifest preparation.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::{HostId, HostSelection, TaskRunId};
use cshell_ipc::{TaskOperation, TaskRequest, TaskResponse};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug)]
pub(crate) struct UploadView {
    pub(crate) rows: Arc<[UploadRunSummary]>,
    pub(crate) after: Option<TaskRunId>,
    pub(crate) document: Option<Arc<UploadStatusDocument>>,
    pub(crate) manifest: Option<BatchUploadDefinition>,
    pub(crate) busy: bool,
    pub(crate) cancel_busy: bool,
    pub(crate) hashing: bool,
    pub(crate) writes_allowed: bool,
    pub(crate) generation: u64,
    pub(crate) status: String,
    authority_epoch: u64,
}
impl Default for UploadView {
    fn default() -> Self {
        Self {
            rows: Arc::from([]),
            after: None,
            document: None,
            manifest: None,
            busy: false,
            cancel_busy: false,
            hashing: false,
            writes_allowed: true,
            generation: 0,
            status: String::new(),
            authority_epoch: 0,
        }
    }
}
enum Operation {
    List(Option<TaskRunId>),
    Describe(BatchUploadDefinition),
    Get(TaskRunId, bool),
    Create(
        BatchUploadDefinition,
        HostSelection,
        BTreeSet<HostId>,
        u64,
        u64,
    ),
    Act(TaskOperation, TaskRunId, Option<HostId>, u64),
}
struct QueuedOperation {
    operation: Operation,
    authority_epoch: u64,
}
pub(crate) struct UploadConnection {
    shared: Arc<Mutex<UploadView>>,
    sender: Option<tokio::sync::mpsc::Sender<QueuedOperation>>,
    cancels: Option<tokio::sync::mpsc::Sender<TaskRunId>>,
    hash_cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl UploadConnection {
    pub(crate) fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(UploadView {
            busy: true,
            ..Default::default()
        }));
        let state = shared.clone();
        let hash_cancel = Arc::new(AtomicBool::new(false));
        let hash_stop = hash_cancel.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<QueuedOperation>(1);
        let (cancels, mut cancel_receiver) = tokio::sync::mpsc::channel::<TaskRunId>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-batch-upload-ui".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let mut view = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.busy = false;
                    view.status = "无法启动上传连接".into();
                    return;
                };
                runtime.block_on(async move {
                    let cancel_config = config.clone();
                    let cancel_state = state.clone();
                    let cancel_worker = tokio::spawn(async move {
                        while let Some(id) = cancel_receiver.recv().await {
                            let result = send(
                                &cancel_config,
                                request(TaskOperation::UploadCancel, Some(id)),
                            )
                            .await;
                            let mut view = cancel_state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.generation = view.generation.wrapping_add(1);
                            view.cancel_busy = false;
                            if result.is_err() {
                                view.writes_allowed = false;
                                view.authority_epoch = view.authority_epoch.saturating_add(1);
                            }
                            view.status = if result.is_ok() {
                                "取消已请求；请查询持久状态".into()
                            } else {
                                "取消回复不明；请查询状态，未自动重发".into()
                            };
                        }
                    });
                    let mut queued = QueuedOperation {
                        operation: Operation::List(None),
                        authority_epoch: 0,
                    };
                    loop {
                        let mutation =
                            matches!(queued.operation, Operation::Create(..) | Operation::Act(..));
                        let epoch = queued.authority_epoch;
                        let result = perform(&config, queued.operation, hash_stop.clone()).await;
                        {
                            let mut view = state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = false;
                            view.hashing = false;
                            view.generation = view.generation.wrapping_add(1);
                            match result {
                                Ok(Outcome::List(rows, after)) => {
                                    view.rows = rows.into();
                                    view.after = after;
                                    view.status = "上传运行列表已载入".into();
                                }
                                Ok(Outcome::Manifest(definition)) => {
                                    view.manifest = Some(definition);
                                    view.status = "内容摘要已生成；保存计划前核对源与目标".into();
                                }
                                Ok(Outcome::Document(document, reload)) => {
                                    view.document = Some(Arc::new(document));
                                    if reload
                                        && epoch != u64::MAX
                                        && epoch == view.authority_epoch
                                        && !view.cancel_busy
                                    {
                                        view.writes_allowed = true;
                                    }
                                    view.status = "上传计划与逐目标状态已载入".into();
                                }
                                Err(error) => {
                                    if mutation {
                                        view.writes_allowed = false;
                                        view.authority_epoch =
                                            view.authority_epoch.saturating_add(1);
                                    }
                                    view.status =
                                        format!("{error}；请重新载入保存状态，未自动重放请求");
                                }
                            }
                        }
                        let Some(next) = receiver.recv().await else {
                            break;
                        };
                        queued = next;
                    }
                    let _joined = cancel_worker.await;
                });
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            cancels: Some(cancels),
            hash_cancel,
            worker: Some(worker),
        })
    }
    pub(crate) fn view(&self) -> UploadView {
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
        if matches!(operation, Operation::Create(..) | Operation::Act(..))
            && (!view.writes_allowed || view.cancel_busy)
        {
            return false;
        }
        if let Operation::Act(kind, id, host, revision) = &operation {
            if view.document.as_ref().is_none_or(|document| {
                document.id != *id
                    || document.active
                    || host.is_some_and(|host| {
                        document
                            .target(host)
                            .is_none_or(|target| *revision != 0 && target.row.revision != *revision)
                    })
            }) || !matches!(
                kind,
                TaskOperation::UploadStart
                    | TaskOperation::UploadProbe
                    | TaskOperation::UploadResume
                    | TaskOperation::UploadCleanup
                    | TaskOperation::UploadAdopt
            ) {
                return false;
            }
        }
        let hashing = matches!(operation, Operation::Describe(..));
        if hashing {
            self.hash_cancel.store(false, Ordering::Release);
            view.manifest = None;
        }
        if self.sender.as_ref().is_some_and(|sender| {
            sender
                .try_send(QueuedOperation {
                    operation,
                    authority_epoch: view.authority_epoch,
                })
                .is_ok()
        }) {
            view.busy = true;
            view.hashing = hashing;
            true
        } else {
            false
        }
    }
    pub(crate) fn list(&self, after: Option<TaskRunId>) -> bool {
        self.enqueue(Operation::List(after))
    }
    pub(crate) fn get(&self, id: TaskRunId, reload: bool) -> bool {
        self.enqueue(Operation::Get(id, reload))
    }
    pub(crate) fn describe(&self, definition: BatchUploadDefinition) -> bool {
        self.enqueue(Operation::Describe(definition))
    }
    pub(crate) fn cancel_hash(&self) {
        self.hash_cancel.store(true, Ordering::Release);
    }
    pub(crate) fn create(
        &self,
        definition: BatchUploadDefinition,
        selection: HostSelection,
        expected: BTreeSet<HostId>,
        hosts: u64,
        profiles: u64,
    ) -> bool {
        self.enqueue(Operation::Create(
            definition, selection, expected, hosts, profiles,
        ))
    }
    pub(crate) fn act(
        &self,
        operation: TaskOperation,
        id: TaskRunId,
        host: Option<HostId>,
        revision: u64,
    ) -> bool {
        self.enqueue(Operation::Act(operation, id, host, revision))
    }
    pub(crate) fn cancel(&self, id: TaskRunId) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.cancel_busy
            || view.document.as_ref().is_none_or(|document| {
                document.id != id
                    || !(document.active
                        || document
                            .targets
                            .iter()
                            .any(|target| target.row.phase == UploadPhase::Pending))
            })
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
impl Drop for UploadConnection {
    fn drop(&mut self) {
        self.cancel_hash();
        self.sender.take();
        self.cancels.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
enum Outcome {
    List(Vec<UploadRunSummary>, Option<TaskRunId>),
    Manifest(BatchUploadDefinition),
    Document(UploadStatusDocument, bool),
}
fn request(operation: TaskOperation, id: Option<TaskRunId>) -> TaskRequest {
    let mut request = TaskRequest::default();
    request.operation = operation as i32;
    if let Some(id) = id {
        request.run_id = id.as_uuid().as_bytes().to_vec();
    }
    request
}
async fn get(
    config: &DesktopConnectionConfig,
    id: TaskRunId,
) -> Result<UploadStatusDocument, String> {
    let reply = send(config, request(TaskOperation::UploadGet, Some(id))).await?;
    let document = decode_upload_status(&reply.document_json).map_err(|_| "上传状态文档无效")?;
    if document.id != id {
        return Err("上传状态身份不一致".into());
    }
    Ok(document)
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
    stop: Arc<AtomicBool>,
) -> Result<Outcome, String> {
    match operation {
        Operation::List(after) => {
            let mut query = request(TaskOperation::UploadList, after);
            query.limit = 64;
            let reply = send(config, query).await?;
            let rows = decode_upload_list(&reply.document_json).map_err(|_| "上传列表无效")?;
            if after.is_some_and(|id| rows.iter().any(|row| row.id <= id)) {
                return Err("上传列表游标不一致".into());
            }
            Ok(Outcome::List(rows, after))
        }
        Operation::Describe(mut definition) => {
            definition
                .validate()
                .map_err(|_| "源路径、目标路径或策略无效")?;
            let path = std::path::PathBuf::from(&definition.local_source);
            let value = tokio::task::spawn_blocking(move || {
                cshell_ipc::fingerprint_upload_source(&path, MAX_UPLOAD_FILE_BYTES, &stop)
            })
            .await
            .map_err(|_| "摘要 worker 失败")?
            .map_err(|error| error.to_string())?;
            definition.size = value.size;
            definition.sha256 = value.sha256;
            Ok(Outcome::Manifest(definition))
        }
        Operation::Get(id, reload) => get(config, id)
            .await
            .map(|document| Outcome::Document(document, reload)),
        Operation::Create(definition, selection, expected, hosts, profiles) => {
            definition.validate().map_err(|_| "上传清单无效")?;
            let mut query = request(TaskOperation::UploadCreate, None);
            query.definition_json = serde_json::to_vec(&definition).map_err(|_| "上传清单无效")?;
            query.selection_json = encode_host_selection(&selection).map_err(|_| "目标选择无效")?;
            query.host_revision = hosts;
            query.profile_revision = profiles;
            let reply = send(config, query).await?;
            let document =
                decode_upload_status(&reply.document_json).map_err(|_| "保存回复无效，结果不明")?;
            let actual: BTreeSet<_> = document
                .targets
                .iter()
                .map(|target| target.host_id)
                .collect();
            if document.id.as_uuid().as_bytes().as_slice() != reply.run_id
                || document.definition != definition
                || document.host_revision != hosts
                || document.profile_revision != profiles
                || actual != expected
                || document.active
                || document.targets.iter().any(|target| {
                    target.row.phase != UploadPhase::Pending || target.row.revision != 1
                })
            {
                return Err("保存的计划与提交内容不一致，结果不明".into());
            }
            Ok(Outcome::Document(document, false))
        }
        Operation::Act(operation, id, host, revision) => {
            let mut query = request(operation, Some(id));
            if let Some(host) = host {
                query.host_id = host.as_uuid().as_bytes().to_vec();
            }
            if operation != TaskOperation::UploadProbe {
                query.confirmed = true;
                query.expected_revision = revision;
            }
            send(config, query).await?;
            get(config, id)
                .await
                .map(|document| Outcome::Document(document, false))
        }
    }
}
async fn send(
    config: &DesktopConnectionConfig,
    query: TaskRequest,
) -> Result<TaskResponse, String> {
    crate::file_task_transport::send(
        config,
        query,
        crate::file_task_transport::FileTaskKind::Upload,
    )
    .await
}
