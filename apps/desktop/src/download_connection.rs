//! Independent bounded download control; no file I/O on the UI thread.
use crate::{
    daemon_connection::DesktopConnectionConfig,
    file_task_transport::{self, FileTaskKind},
};
use cshell_application::*;
use cshell_domain::{HostId, HostSelection, TaskRunId};
use cshell_ipc::{TaskOperation, TaskRequest};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(crate) type DownloadExpectedTargets = BTreeMap<HostId, (String, String, u16)>;
#[derive(Clone, Debug)]
pub(crate) struct DownloadView {
    pub(crate) rows: Arc<[DownloadRunSummary]>,
    pub(crate) after: Option<TaskRunId>,
    pub(crate) document: Option<Arc<DownloadStatusDocument>>,
    pub(crate) busy: bool,
    pub(crate) cancel_busy: bool,
    pub(crate) available: bool,
    pub(crate) writes_allowed: bool,
    pub(crate) generation: u64,
    pub(crate) status: String,
    authority_epoch: u64,
}
impl Default for DownloadView {
    fn default() -> Self {
        Self {
            rows: Arc::from([]),
            after: None,
            document: None,
            busy: false,
            cancel_busy: false,
            available: false,
            writes_allowed: true,
            generation: 0,
            status: String::new(),
            authority_epoch: 0,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DownloadAction {
    Start,
    Probe(HostId, u64),
    Resume(HostId, u64),
    Cleanup(HostId, u64),
    Adopt(HostId, u64),
}
impl DownloadAction {
    fn mutates(self) -> bool {
        !matches!(self, Self::Probe(..))
    }
    fn allowed(self, document: &DownloadStatusDocument) -> bool {
        if document.active {
            return false;
        }
        let (host, revision) = match self {
            Self::Start => {
                return document
                    .targets
                    .iter()
                    .any(|target| target.row.phase == DownloadPhase::Pending);
            }
            Self::Probe(host, revision)
            | Self::Resume(host, revision)
            | Self::Cleanup(host, revision)
            | Self::Adopt(host, revision) => (host, revision),
        };
        let Some(target) = document.target(host) else {
            return false;
        };
        if revision == 0 || target.row.revision != revision {
            return false;
        }
        match self {
            Self::Probe(..) => true,
            Self::Resume(..) => target.row.can_resume(),
            Self::Cleanup(..) => target.row.can_cleanup(),
            Self::Adopt(..) => {
                target.row.can_adopt() && document.has_matching_destination_probe(host)
            }
            Self::Start => false,
        }
    }
    fn request(self, id: TaskRunId) -> TaskRequest {
        let mut query = request(
            match self {
                Self::Start => TaskOperation::DownloadStart,
                Self::Probe(..) => TaskOperation::DownloadProbe,
                Self::Resume(..) => TaskOperation::DownloadResume,
                Self::Cleanup(..) => TaskOperation::DownloadCleanup,
                Self::Adopt(..) => TaskOperation::DownloadAdopt,
            },
            Some(id),
        );
        match self {
            Self::Start => {}
            Self::Probe(host, revision)
            | Self::Resume(host, revision)
            | Self::Cleanup(host, revision)
            | Self::Adopt(host, revision) => {
                query.host_id = host.as_uuid().as_bytes().to_vec();
                query.expected_revision = revision;
            }
        }
        query.confirmed = self.mutates();
        query
    }
}
enum Operation {
    List(Option<TaskRunId>),
    Get(TaskRunId, bool),
    Create(
        BatchDownloadDefinition,
        HostSelection,
        DownloadExpectedTargets,
        u64,
        u64,
    ),
    Act(TaskRunId, DownloadAction),
}
impl Operation {
    fn mutates(&self) -> bool {
        match self {
            Self::Create(..) => true,
            Self::Act(_, action) => action.mutates(),
            _ => false,
        }
    }
}
struct QueuedOperation {
    operation: Operation,
    authority_epoch: u64,
}
pub(crate) struct DownloadConnection {
    shared: Arc<Mutex<DownloadView>>,
    sender: Option<tokio::sync::mpsc::Sender<QueuedOperation>>,
    cancels: Option<tokio::sync::mpsc::Sender<TaskRunId>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl DownloadConnection {
    pub(crate) fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(DownloadView {
            busy: true,
            ..Default::default()
        }));
        let state = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<QueuedOperation>(1);
        let (cancels, mut cancel_receiver) = tokio::sync::mpsc::channel::<TaskRunId>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-batch-download-ui".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let mut view = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.busy = false;
                    view.status = "无法启动下载连接".into();
                    return;
                };
                runtime.block_on(async move {
                    let cancel_config = config.clone();
                    let cancel_state = state.clone();
                    let cancel_worker = tokio::spawn(async move {
                        while let Some(id) = cancel_receiver.recv().await {
                            let result = file_task_transport::send(
                                &cancel_config,
                                request(TaskOperation::DownloadCancel, Some(id)),
                                FileTaskKind::Download,
                            )
                            .await;
                            let mut view = cancel_state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.cancel_busy = false;
                            view.generation = view.generation.wrapping_add(1);
                            if result.is_err() {
                                block_writes(&mut view);
                            }
                            view.status = if result.is_ok() {
                                "取消已请求；查询保存状态确认结果".into()
                            } else {
                                "取消回复不明；请重新载入状态，未自动重发".into()
                            };
                        }
                    });
                    let mut queued = QueuedOperation {
                        operation: Operation::List(None),
                        authority_epoch: 0,
                    };
                    loop {
                        let mutation = queued.operation.mutates();
                        let epoch = queued.authority_epoch;
                        let result = perform(&config, queued.operation).await;
                        {
                            let mut view = state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = false;
                            view.generation = view.generation.wrapping_add(1);
                            match result {
                                Ok(Outcome::List(rows, after)) => {
                                    view.available = true;
                                    view.rows = rows.into();
                                    view.after = after;
                                    view.status = "下载运行列表已载入".into();
                                }
                                Ok(Outcome::Document(document, reload)) => {
                                    view.available = true;
                                    view.document = Some(Arc::new(document));
                                    if reload
                                        && epoch != u64::MAX
                                        && epoch == view.authority_epoch
                                        && !view.cancel_busy
                                    {
                                        view.writes_allowed = true;
                                    }
                                    view.status = "下载计划与逐目标保存状态已载入".into();
                                }
                                Err(error) => {
                                    view.available = false;
                                    if mutation {
                                        block_writes(&mut view);
                                    }
                                    view.status =
                                        format!("{error}；请重新载入状态，未自动重放请求");
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
            worker: Some(worker),
        })
    }
    pub(crate) fn view(&self) -> DownloadView {
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
            || operation.mutates() && (!view.writes_allowed || !view.available || view.cancel_busy)
        {
            return false;
        }
        if let Operation::Act(id, action) = &operation {
            if view.cancel_busy
                || !view.available
                || view
                    .document
                    .as_ref()
                    .is_none_or(|document| document.id != *id || !action.allowed(document))
            {
                return false;
            }
        }
        let queued = QueuedOperation {
            operation,
            authority_epoch: view.authority_epoch,
        };
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(queued).is_ok())
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
    pub(crate) fn get(&self, id: TaskRunId, reload: bool) -> bool {
        self.enqueue(Operation::Get(id, reload))
    }
    pub(crate) fn create(
        &self,
        definition: BatchDownloadDefinition,
        selection: HostSelection,
        expected: DownloadExpectedTargets,
        hosts: u64,
        profiles: u64,
    ) -> bool {
        self.enqueue(Operation::Create(
            definition, selection, expected, hosts, profiles,
        ))
    }
    pub(crate) fn act(&self, id: TaskRunId, action: DownloadAction) -> bool {
        self.enqueue(Operation::Act(id, action))
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
                            .any(|target| target.row.phase == DownloadPhase::Pending))
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
            view.status = "正在请求取消下载".into();
            true
        } else {
            false
        }
    }
}
impl Drop for DownloadConnection {
    fn drop(&mut self) {
        self.sender.take();
        self.cancels.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
fn block_writes(view: &mut DownloadView) {
    view.writes_allowed = false;
    view.authority_epoch = view.authority_epoch.saturating_add(1);
}
enum Outcome {
    List(Vec<DownloadRunSummary>, Option<TaskRunId>),
    Document(DownloadStatusDocument, bool),
}
fn request(operation: TaskOperation, id: Option<TaskRunId>) -> TaskRequest {
    let mut query = TaskRequest::default();
    query.operation = operation as i32;
    if let Some(id) = id {
        query.run_id = id.as_uuid().as_bytes().to_vec();
    }
    query
}
async fn get(
    config: &DesktopConnectionConfig,
    id: TaskRunId,
) -> Result<DownloadStatusDocument, String> {
    let reply = file_task_transport::send(
        config,
        request(TaskOperation::DownloadGet, Some(id)),
        FileTaskKind::Download,
    )
    .await?;
    let document = decode_download_status(&reply.document_json).map_err(|_| "下载状态文档无效")?;
    if document.id != id {
        return Err("下载状态身份不一致".into());
    }
    Ok(document)
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
) -> Result<Outcome, String> {
    match operation {
        Operation::List(after) => {
            let mut query = request(TaskOperation::DownloadList, after);
            query.limit = 64;
            let reply = file_task_transport::send(config, query, FileTaskKind::Download).await?;
            let rows = decode_download_list(&reply.document_json).map_err(|_| "下载列表无效")?;
            if after.is_some_and(|id| rows.iter().any(|row| row.id <= id)) {
                return Err("下载列表游标不一致".into());
            }
            Ok(Outcome::List(rows, after))
        }
        Operation::Get(id, reload) => get(config, id)
            .await
            .map(|document| Outcome::Document(document, reload)),
        Operation::Create(definition, selection, expected, hosts, profiles) => {
            definition.validate().map_err(|_| "下载计划无效")?;
            let mut query = request(TaskOperation::DownloadCreate, None);
            query.definition_json = serde_json::to_vec(&definition).map_err(|_| "下载计划无效")?;
            query.selection_json = encode_host_selection(&selection).map_err(|_| "目标选择无效")?;
            query.host_revision = hosts;
            query.profile_revision = profiles;
            let reply = file_task_transport::send(config, query, FileTaskKind::Download).await?;
            let document = decode_download_status(&reply.document_json)
                .map_err(|_| "下载保存回复无效，结果不明")?;
            let actual: DownloadExpectedTargets = document
                .targets
                .iter()
                .map(|target| {
                    (
                        target.host_id,
                        (target.name.clone(), target.address.clone(), target.port),
                    )
                })
                .collect();
            if document.id.as_uuid().as_bytes().as_slice() != reply.run_id
                || document.definition != definition
                || document.host_revision != hosts
                || document.profile_revision != profiles
                || actual != expected
                || document.active
                || document.targets.iter().any(|target| {
                    target.row.phase != DownloadPhase::Pending || target.row.revision != 1
                })
            {
                return Err("保存的下载计划与提交内容不一致，结果不明".into());
            }
            Ok(Outcome::Document(document, false))
        }
        Operation::Act(id, action) => {
            file_task_transport::send(config, action.request(id), FileTaskKind::Download).await?;
            get(config, id)
                .await
                .map(|document| Outcome::Document(document, false))
        }
    }
}
