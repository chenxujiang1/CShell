use crate::{daemon_connection::DesktopConnectionConfig, output_analysis_connection::send};
use cshell_application::*;
use cshell_domain::TaskRunId;
use cshell_ipc::{ExportStaging, ExportTransferControl, TaskOperation, TaskRequest};
use std::{
    io::{self, Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Default)]
pub(crate) struct ArchiveView {
    pub export_path: Option<PathBuf>,
    pub document: Option<Arc<OutputArchiveDocument>>,
    pub transfer: Option<Arc<ExportTransferControl>>,
    pub busy: bool,
    pub status: String,
}
enum Operation {
    Launch(OutputAnalysisRequest, [u8; 32], TaskRunId),
    Get(Arc<OutputArchiveDocument>, bool),
    Export(
        Arc<OutputArchiveDocument>,
        PathBuf,
        Arc<ExportTransferControl>,
    ),
}
enum Outcome {
    Document(OutputArchiveDocument),
    Exported,
}
pub(crate) struct OutputArchiveConnection {
    shared: Arc<Mutex<ArchiveView>>,
    sender: Option<tokio::sync::mpsc::Sender<Operation>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl OutputArchiveConnection {
    pub fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(ArchiveView::default()));
        let state = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Operation>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-output-archive-ui".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .status = "无法启动归档连接".into();
                    return;
                };
                runtime.block_on(async move {
                    while let Some(operation) = receiver.recv().await {
                        let transfer = if let Operation::Export(_, _, control) = &operation {
                            Some(control.clone())
                        } else {
                            None
                        };
                        let result = perform(&config, operation).await;
                        if let Some(transfer) = transfer {
                            transfer.finish();
                        }
                        let mut view = state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        view.busy = false;
                        match result {
                            Ok(Outcome::Document(document)) => {
                                view.document = Some(Arc::new(document));
                                view.status = "归档捕获状态已载入".into();
                            }
                            Ok(Outcome::Exported) => {
                                view.status = "归档已校验并保存到新文件".into()
                            }
                            Err(error) => {
                                if error == "输出分析作业已过期或 daemon 已重启"
                                    && view.document.as_ref().is_some_and(|document| {
                                        document.state == OutputAnalysisState::Running
                                    })
                                {
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
    pub fn view(&self) -> ArchiveView {
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
        if let Operation::Get(document, _) | Operation::Export(document, ..) = &operation
            && view
                .document
                .as_ref()
                .is_none_or(|saved| !Arc::ptr_eq(saved, document))
        {
            return false;
        }
        let transfer = if let Operation::Export(_, _, control) = &operation {
            Some(control.clone())
        } else {
            None
        };
        let export_path = if let Operation::Export(_, path, _) = &operation {
            Some(path.clone())
        } else {
            None
        };
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(operation).is_ok())
        {
            view.transfer = transfer;
            view.export_path = export_path;
            view.busy = true;
            true
        } else {
            false
        }
    }
    pub fn launch(&self, scope: OutputAnalysisRequest, hash: [u8; 32], child: TaskRunId) -> bool {
        self.enqueue(Operation::Launch(scope, hash, child))
    }
    pub fn get(&self, document: Arc<OutputArchiveDocument>, cancel: bool) -> bool {
        self.enqueue(Operation::Get(document, cancel))
    }
    pub fn export(&self, document: Arc<OutputArchiveDocument>, path: PathBuf) -> bool {
        self.enqueue(Operation::Export(
            document,
            path,
            Arc::new(ExportTransferControl::default()),
        ))
    }
    pub fn cancel_export(&self) -> bool {
        self.view()
            .transfer
            .as_ref()
            .is_some_and(|control| control.request_cancel())
    }
}
impl Drop for OutputArchiveConnection {
    fn drop(&mut self) {
        let _cancelled = self.cancel_export();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
) -> Result<Outcome, String> {
    let mut query = TaskRequest::default();
    let (scope, hash, child, previous) = match operation {
        Operation::Export(document, path, control) => {
            let artifact = document
                .artifact
                .as_ref()
                .ok_or("归档尚未捕获完成")?
                .clone();
            download(config, artifact, path, control).await?;
            return Ok(Outcome::Exported);
        }
        Operation::Launch(scope, hash, child) => {
            scope.validate().map_err(|_| "归档范围无效")?;
            if scope.right.is_some() || scope.stderr {
                return Err("归档需要单目标 stdout 范围".into());
            }
            query.operation = TaskOperation::OutputArchive as i32;
            query.run_id = scope.run_id.as_uuid().as_bytes().to_vec();
            query.definition_json = serde_json::to_vec(&scope).map_err(|_| "归档范围无效")?;
            (scope, hash, child, None)
        }
        Operation::Get(document, cancel) => {
            query.operation = if cancel {
                TaskOperation::OutputArchiveCancel
            } else {
                TaskOperation::OutputArchiveGet
            } as i32;
            query.run_id = document.scope.run_id.as_uuid().as_bytes().to_vec();
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            (
                document.scope.clone(),
                document.binding.snapshot_sha256,
                document.binding.child_id,
                Some(document),
            )
        }
    };
    let reply = send(config, query, scope.kind).await?;
    let document = decode_output_archive(&reply.document_json).map_err(|_| "归档作业无效")?;
    if document.scope != scope
        || document.binding.snapshot_sha256 != hash
        || document.binding.child_id != child
        || previous.as_ref().is_some_and(|previous| {
            document.job_id != previous.job_id
                || document.binding != previous.binding
                || document.bytes < previous.bytes
                || document.source_bytes < previous.source_bytes
                || previous.state != OutputAnalysisState::Running && *previous.as_ref() != document
        })
    {
        return Err("归档身份或进度不一致".into());
    }
    Ok(Outcome::Document(document))
}
fn check(control: &ExportTransferControl, started: Instant) -> io::Result<()> {
    if control.cancelled() {
        Err(io::Error::other("archive export cancelled"))
    } else if started.elapsed() > Duration::from_secs(1800) {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "archive export deadline",
        ))
    } else {
        Ok(())
    }
}
async fn download(
    config: &DesktopConnectionConfig,
    artifact: OutputArchiveArtifact,
    path: PathBuf,
    control: Arc<ExportTransferControl>,
) -> Result<(), String> {
    artifact.validate().map_err(|_| "归档描述无效")?;
    let mut staging = tokio::task::spawn_blocking(move || ExportStaging::prepare(&path))
        .await
        .map_err(|_| "无法准备归档暂存文件")?
        .map_err(|error| error.to_string())?;
    let file = staging
        .file_mut()
        .map_err(|error| error.to_string())?
        .try_clone()
        .map_err(|_| "无法打开归档暂存文件")?;
    let mut output = tokio::fs::File::from_std(file);
    let started = Instant::now();
    let run_id = artifact.manifest.scope.run_id;
    let archive_id = artifact.manifest.archive_id;
    let kind = artifact.manifest.scope.kind;
    copy_output_archive(
        &artifact,
        &mut output,
        |offset, limit| async move {
            let mut query = TaskRequest::default();
            query.operation = TaskOperation::OutputArchiveRead as i32;
            query.run_id = run_id.as_uuid().as_bytes().to_vec();
            query.result_id = archive_id.as_uuid().as_bytes().to_vec();
            query.offset = offset;
            query.limit = limit as u32;
            let mut reply = send(config, query, kind).await.map_err(io::Error::other)?;
            Ok((
                std::mem::take(&mut reply.document_json),
                OutputArchiveBytes(std::mem::take(&mut reply.output)),
            ))
        },
        || check(&control, started),
        |bytes| control.record_bytes(bytes),
    )
    .await
    .map_err(|_| "归档下载未完成；未发布目标文件")?;
    output
        .sync_all()
        .await
        .map_err(|_| "归档暂存同步失败；未发布目标文件")?;
    drop(output);
    tokio::task::spawn_blocking(move || {
        let file = staging.file_mut().map_err(|error| error.to_string())?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| "归档暂存无法读取")?;
        let actual = inspect_output_archive_checked(file, || check(&control, started))
            .map_err(|_| "归档校验未完成；未发布目标文件")?;
        if actual != artifact {
            return Err("归档内容与捕获描述不一致；未发布目标文件".into());
        }
        if !control.begin_publication() {
            return Err("归档下载已取消；未发布目标文件".into());
        }
        staging.publish().map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| "归档发布未完成，请核对目标文件")?
}
