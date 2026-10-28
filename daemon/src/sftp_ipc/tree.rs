//! Recursive SFTP batches use private durable file queues; the source channel stays open.
use super::*;
use cshell_application::*;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Default)]
pub(super) struct Trees(Arc<Mutex<BTreeMap<SessionId, Arc<Job>>>>);
struct Job {
    source: Arc<SshSession>,
    epoch: u64,
    options: SftpTreeStart,
    snapshot: Mutex<SftpTreeSnapshot>,
    control: Mutex<TransferControl>,
    active: AtomicBool,
    registry: Arc<SshSessionRegistry>,
    plan: tokio::sync::Mutex<Option<Plan>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_directory_validation_rejects_parent_traversal_and_files(){
        let root=tempfile::tempdir().unwrap();let path=root.path().join("file");std::fs::write(&path,b"keep").unwrap();
        assert!(local_directory(&path,false).is_err());assert!(local_directory(&root.path().join("../outside"),true).is_err());assert_eq!(std::fs::read(path).unwrap(),b"keep");
        assert!(!name(".."));assert!(!name("a/b"));if cfg!(windows){assert!(!name("con.txt"));assert!(!name("C:escape"));}
    }
    #[tokio::test]
    async fn invalid_scope_or_unknown_source_never_creates_a_tree_job(){
        let service=SftpIpcService::default();let registry=SshSessionRegistry::default();let session=SessionId::new();
        for id in [session,SessionId::new()] {
            let request=SftpRequest{operation:SftpOperation::TreeStart as i32,session_id:session.as_uuid().as_bytes().to_vec(),transfer_id:id.as_uuid().as_bytes().to_vec(),confirmed:true,document_json:serde_json::to_vec(&SftpTreeStart{direction:SftpQueueDirection::Upload,paths:vec!["C:/sample".into()],destination:"/srv".into(),verification:Default::default()}).unwrap(),..Default::default()};
            let response=service.handle(request,&registry).await;assert_ne!(response.status,SftpStatus::Ok as i32);
        }assert!(service.trees.0.lock().unwrap().is_empty());
    }
}
impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeJob").finish_non_exhaustive()
    }
}
#[derive(Default)]
struct Plan {
    files: Vec<SftpQueueDraft>,
    directories: Vec<String>,
    next: usize,
    base_bytes: u64,
    base_files: u32,
    initialized: bool,
}
impl Job {
    fn view(&self) -> SftpTreeSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn update(&self, f: impl FnOnce(&mut SftpTreeSnapshot)) {
        f(&mut self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
    }
    fn control(&self) -> TransferControl {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn check(&self) -> Result<(), String> {
        if self.control().is_cancelled() {
            return Err("任务已取消；已完成文件保留".into());
        }
        if !self.source.running() || self.source.generation() != self.epoch {
            return Err("原 SFTP 连接已断开；未转移到其他连接".into());
        }
        Ok(())
    }
    async fn ready(&self) -> Result<(), String> {
        loop {
            self.check()?;
            if !self.control().is_paused() {
                return Ok(());
            }
            self.update(|v| v.state = TerminalUploadState::Paused);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
pub(super) fn name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 255
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
        && (!cfg!(windows)
            || (!value.contains(['<', '>', ':', '"', '|', '?', '*'])
                && !value.ends_with(['.', ' '])
                && !matches!(
                    value
                        .split('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )))
}
pub(super) fn local_directory(path: &Path, create: bool) -> Result<(), String> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("本地目录路径无效".into());
    }
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for part in ancestors {
        if create && !part.exists() {
            std::fs::create_dir(part).map_err(|_| "无法创建本地目录，请检查权限")?;
        }
        let m = std::fs::symlink_metadata(part).map_err(|_| "本地目录不存在或无法读取")?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err("不向符号链接目录写入，请选择实际目录".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if m.file_attributes() & 0x400 != 0 {
                return Err("不向目录联接写入，请选择实际目录".into());
            }
        }
    }
    Ok(())
}
impl SftpIpcService {
    pub(super) fn start_sync_tree(&self,source:Arc<SshSession>,session:SessionId,id:SessionId,options:SftpTreeStart,files:Vec<SftpQueueDraft>,directories:Vec<String>,total_bytes:u64,epoch:u64)->Result<SftpTreeSnapshot,String> {
        if !source.running()||source.generation()!=epoch{return Err("原连接已变化，请重新比较".into());}
        for file in &files {file.validate().map_err(str::to_owned)?;}
        let registry=Arc::new(SshSessionRegistry::default());
        registry.sftp_targets.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id,source.clone());
        let job=Arc::new(Job {epoch,source,options:options.clone(),
            snapshot:Mutex::new(SftpTreeSnapshot{id,session,direction:options.direction,state:TerminalUploadState::Preparing,completed:0,total_files:files.len() as u32,
                bytes:0,total_bytes,bytes_per_second:0,detail:"正在提交已审阅的目录差异".into(),retryable:false,label:"目录同步".into(),destination:options.destination.clone()}),
            control:Mutex::new(TransferControl::default()),active:AtomicBool::new(true),registry,
            plan:tokio::sync::Mutex::new(Some(Plan{files,directories,..Default::default()}))});
        {
            let mut jobs=self.trees.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.contains_key(&id) || super::queue::Queues::root().is_ok_and(|root|root.join(format!("{id}.json")).exists()){return Err("同步标识已被使用，请查询原任务，勿重复提交".into());}
            if jobs.values().filter(|j|j.active.load(Ordering::Acquire)).count()>=2{return Err("目录任务已满，请稍后重新比较".into());}
            if jobs.len()>=64{jobs.retain(|_,j|!j.view().state.finished()||j.view().retryable);}
            if jobs.len()>=64{return Err("请先处理未完成的目录任务".into());}
            jobs.insert(id,job.clone());
        }
        self.spawn_tree(job.clone());Ok(job.view())
    }
    pub(super) async fn tree(
        &self,
        request: SftpRequest,
        session: SessionId,
        op: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        let Ok(bytes) = <[u8; 16]>::try_from(request.transfer_id.as_slice()) else {
            return SftpResponse::with_status(SftpStatus::InvalidRequest, "目录任务标识无效");
        };
        let id = SessionId::from_bytes(bytes);
        let expected = SftpRequest {
            operation: op as i32,
            session_id: request.session_id.clone(),
            transfer_id: request.transfer_id.clone(),
            document_json: if op == SftpOperation::TreeStart {
                request.document_json.clone()
            } else {
                Vec::new()
            },
            confirmed: op != SftpOperation::TreeGet,
            ..Default::default()
        };
        if request != expected
            || id.as_uuid().is_nil()
            || id == session
            || request.document_json.len() > 64 * 1024
        {
            return SftpResponse::with_status(SftpStatus::InvalidRequest, "目录任务请求无效");
        }
        let job = if op == SftpOperation::TreeStart {
            let Ok(options) = serde_json::from_slice::<SftpTreeStart>(&request.document_json)
            else {
                return SftpResponse::with_status(SftpStatus::InvalidRequest, "目录任务内容无效");
            };
            if options.paths.is_empty()
                || options.paths.len() > 128
                || options
                    .paths
                    .iter()
                    .any(|p| p.len() > 4096 || p.chars().any(char::is_control))
                || options.destination.len() > 4096
                || options.destination.chars().any(char::is_control)
            {
                return SftpResponse::with_status(
                    SftpStatus::InvalidRequest,
                    "请选择 1–128 个有效文件或文件夹",
                );
            }
            let Some(source) = sessions.get_sftp(session).ok().filter(|s| s.running()) else {
                return SftpResponse::with_status(
                    SftpStatus::Unavailable,
                    "SFTP 已断开，请重新连接",
                );
            };
            let registry = Arc::new(SshSessionRegistry::default());
            if sessions.get_sftp(id).is_ok()||super::queue::Queues::root().is_ok_and(|root|root.join(format!("{id}.json")).exists()){return SftpResponse::with_status(SftpStatus::Conflict,"任务标识已被使用，请查询原任务状态");}
            registry
                .sftp_targets
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id, source.clone());
            let label = options
                .paths
                .first()
                .and_then(|p| p.rsplit(['/', '\\']).next())
                .unwrap_or("目录任务")
                .to_owned();
            let job = Arc::new(Job {
                epoch: source.generation(),
                source,
                options: options.clone(),
                snapshot: Mutex::new(SftpTreeSnapshot {
                    id,
                    session,
                    direction: options.direction,
                    state: TerminalUploadState::Preparing,
                    completed: 0,
                    total_files: 0,
                    bytes: 0,
                    total_bytes: 0,
                    bytes_per_second: 0,
                    detail: "正在整理目录；同名文件不会覆盖".into(),
                    retryable: false,
                    label: if options.paths.len() > 1 {
                        format!("{label} 等 {} 项", options.paths.len())
                    } else {
                        label
                    },
                    destination:options.destination.clone(),
                }),
                control: Mutex::new(TransferControl::default()),
                active: AtomicBool::new(true),
                registry,
                plan: tokio::sync::Mutex::new(None),
            });
            {
                let mut jobs = self
                    .trees
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if jobs.contains_key(&id) {
                    return SftpResponse::with_status(
                        SftpStatus::Conflict,
                        "任务已提交，请刷新进度，不要重复提交",
                    );
                }
                if jobs
                    .values()
                    .filter(|j| j.active.load(Ordering::Acquire))
                    .count()
                    >= 2
                {
                    return SftpResponse::with_status(
                        SftpStatus::CapacityReached,
                        "目录任务已满，请等待已有任务完成",
                    );
                }
                if jobs.len() >= 64 {
                    jobs.retain(|_, j| !j.view().state.finished() || j.view().retryable);
                }
                if jobs.len() >= 64 {
                    return SftpResponse::with_status(
                        SftpStatus::CapacityReached,
                        "请先处理未完成的目录任务",
                    );
                }
                jobs.insert(id, job.clone());
            }
            self.spawn_tree(job.clone());
            job
        } else {
            let Some(job) = self
                .trees
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .filter(|j| j.view().session == session)
                .cloned()
            else {
                return SftpResponse::with_status(
                    SftpStatus::Unavailable,
                    "未找到此会话的目录任务；不会重新提交",
                );
            };
            match op {
                SftpOperation::TreePause if !job.view().state.finished() => job.control().pause(),
                SftpOperation::TreeContinue if !job.view().state.finished() => {
                    job.control().resume()
                }
                SftpOperation::TreeCancel if !job.view().state.finished() => {
                    job.control().cancel();
                    job.update(|v| v.detail = "正在取消；已完成文件将保留".into());
                }
                SftpOperation::TreeRetry if job.view().retryable => {
                    if job
                        .active
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        return SftpResponse::with_status(
                            SftpStatus::Conflict,
                            "原任务正在收尾，请稍后重试",
                        );
                    }
                    *job.control
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        TransferControl::default();
                    job.update(|v| {
                        v.state = TerminalUploadState::Preparing;
                        v.retryable = false;
                        v.detail = "正在核对未完成文件".into();
                    });
                    self.spawn_tree(job.clone());
                }
                SftpOperation::TreeGet => {}
                _ => {
                    return SftpResponse::with_status(
                        SftpStatus::Conflict,
                        "任务状态已变化，请刷新进度",
                    );
                }
            }
            job
        };
        SftpResponse {
            document_json: serde_json::to_vec(&job.view()).unwrap_or_default(),
            ..Default::default()
        }
    }
    fn spawn_tree(&self, job: Arc<Job>) {
        let service = self.clone();
        tokio::spawn(async move {
            let result = service.run_tree(&job).await;
            if result.is_err() {
                let _ = service
                    .upload_mutate(
                        &job.registry,
                        job.view().id,
                        SftpOperation::QueuePause,
                        Vec::new(),
                    )
                    .await;
                service.stop_session_transfers(job.view().id);
            }
            let mut retryable = false;
            if result.is_err() && job.source.running() {
                for _ in 0..30 {
                    if let Ok(queue) = service.upload_queue(&job.registry, job.view().id).await {
                        if !queue
                            .items
                            .iter()
                            .any(|r| r.state == SftpQueueState::Running)
                        {
                            retryable = !queue.items.is_empty()
                                && queue.items.iter().all(|r| {
                                    matches!(
                                        r.state,
                                        SftpQueueState::Succeeded | SftpQueueState::Pending
                                    ) || r.can_retry_verified()
                                });
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            job.update(|v| {
                v.bytes_per_second = 0;
                v.retryable = retryable;
                match result {
                    Ok(()) => {
                        v.state = TerminalUploadState::Succeeded;
                        v.bytes = v.total_bytes;
                        v.completed = v.total_files;
                        v.detail = "全部传输完成，目录结构已保留".into();
                    }
                    Err(error) => {
                        v.state = if job.control().is_cancelled() {
                            TerminalUploadState::Cancelled
                        } else {
                            TerminalUploadState::Failed
                        };
                        v.detail = error;
                    }
                }
            });
            job.active.store(false, Ordering::Release);
        });
    }
    async fn tree_plan(&self, job: &Arc<Job>) -> Result<Plan, String> {
        job.ready().await?;
        let mut plan = Plan::default();
        let mut bytes = 0u64;
        match job.options.direction {
            SftpQueueDirection::Upload => {
                if !sftp_remote_path(&job.options.destination, true) {
                    return Err("远端目标目录无效".into());
                }
                let paths = job.options.paths.clone();
                let control = job.control();
                let input = tokio::task::spawn_blocking(move || {
                    super::terminal_upload::plan(&paths, &control)
                })
                .await
                .map_err(|_| "目录整理失败")??;
                plan.directories = input
                    .directories
                    .into_iter()
                    .map(|r| format!("{}/{}", job.options.destination.trim_end_matches('/'), r))
                    .collect();
                bytes = input.bytes;
                for (local, relative, _) in input.files {
                    plan.files.push(SftpQueueDraft {
                        direction: SftpQueueDirection::Upload,
                        local_path: local,
                        remote_path: format!(
                            "{}/{}",
                            job.options.destination.trim_end_matches('/'),
                            relative
                        ),
                        verified: true,
                        verification: Some(job.options.verification),
                        ..Default::default()
                    });
                }
            }
            SftpQueueDirection::Download => {
                let destination = job.options.destination.clone();
                tokio::task::spawn_blocking(move || {
                    local_directory(Path::new(&destination), false)
                })
                .await
                .map_err(|_| "检查本地目录失败")??;
                let mut pending = Vec::new();
                let mut destinations = BTreeSet::new();
                for path in &job.options.paths {
                    if !sftp_remote_path(path, false) {
                        return Err("远端源路径无效".into());
                    }
                    let relative = path
                        .rsplit('/')
                        .next()
                        .filter(|v| name(v))
                        .ok_or("文件名无法保存在本地")?
                        .to_owned();
                    pending.push((path.clone(), relative, 0usize));
                }
                while let Some((remote, relative, depth)) = pending.pop() {
                    job.ready().await?;
                    let key = if cfg!(windows) {
                        relative.to_lowercase()
                    } else {
                        relative.clone()
                    };
                    if depth > 64
                        || relative.len() > 3500
                        || pending.len() + plan.files.len() + plan.directories.len() >= 10_000
                        || !destinations.insert(key)
                    {
                        return Err("目录超过 10000 项、64 层或包含重复目标，请拆分下载".into());
                    }
                    let client = job
                        .source
                        .open_sftp()
                        .await
                        .map_err(|_| "无法读取远端目录")?;
                    let metadata = client
                        .metadata(remote.clone(), cshell_sftp::SymlinkPolicy::NoFollow)
                        .await
                        .map_err(|_| "远端文件已变化或无法读取")?;
                    let local = Path::new(&job.options.destination)
                        .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
                    match metadata.file_type {
                        RemoteFileType::Directory => {
                            plan.directories
                                .push(local.to_str().ok_or("本地路径编码无效")?.into());
                            let mut cursor = client
                                .directory_cursor(remote)
                                .await
                                .map_err(|_| "无法打开远端目录")?;
                            loop {
                                job.ready().await?;
                                let page = cursor
                                    .next_page(128)
                                    .await
                                    .map_err(|_| "读取远端目录失败")?;
                                for entry in page.entries {
                                    if !name(&entry.name) {
                                        return Err("目录中含有无法保存在本地的文件名".into());
                                    }
                                    pending.push((
                                        entry.path,
                                        format!("{relative}/{}", entry.name),
                                        depth + 1,
                                    ));
                                    if pending.len() + plan.files.len() + plan.directories.len()
                                        > 10_000
                                    {
                                        return Err("目录超过 10000 项，请拆分下载".into());
                                    }
                                }
                                if !page.truncated {
                                    break;
                                }
                            }
                            let _ = cursor.close().await;
                        }
                        RemoteFileType::File => {
                            let size = metadata.size.ok_or("无法确认远端文件大小")?;
                            if size > MAX_SFTP_VERIFIED_BYTES {
                                return Err("单个文件超过 64 GiB，请拆分下载".into());
                            }
                            bytes = bytes.checked_add(size).ok_or("下载总大小超限")?;
                            plan.files.push(SftpQueueDraft {
                                direction: SftpQueueDirection::Download,
                                remote_path: remote,
                                local_path: local.to_str().ok_or("本地路径编码无效")?.into(),
                                verified: true,
                                verification: Some(job.options.verification),
                                ..Default::default()
                            });
                            let _ = client.close().await;
                        }
                        _ => {
                            return Err(
                                "目录包含符号链接或特殊文件，请选择普通文件或实际目录".into()
                            );
                        }
                    }
                }
            }
        }
        for file in &plan.files {
            file.validate().map_err(str::to_owned)?;
        }
        job.update(|v| {
            v.total_files = plan.files.len() as u32;
            v.total_bytes = bytes;
        });
        Ok(plan)
    }
    async fn run_tree(&self, job: &Arc<Job>) -> Result<(), String> {
        let id = job.view().id;
        let mut guard = job.plan.lock().await;
        if guard.is_none() {
            *guard = Some(self.tree_plan(job).await?);
        }
        let plan = guard.as_mut().ok_or("目录传输计划不可用")?;
        if !plan.initialized {
            for directory in &plan.directories {
                job.ready().await?;
                match job.options.direction {
                    SftpQueueDirection::Upload => {
                        let client = job
                            .source
                            .open_sftp()
                            .await
                            .map_err(|_| "无法创建远端目录")?;
                        match client
                            .metadata(directory.clone(), cshell_sftp::SymlinkPolicy::NoFollow)
                            .await
                        {
                            Ok(m) if m.file_type == RemoteFileType::Directory => {}
                            Ok(_) => return Err("目标包含同名文件或链接，未覆盖".into()),
                            Err(e) if e.is_not_found() => client
                                .create_directory(directory, 0o755)
                                .await
                                .map_err(|_| "无法创建远端目录，请检查权限")?,
                            Err(_) => return Err("无法核对目标目录，请检查权限".into()),
                        }
                        let _ = client.close().await;
                    }
                    SftpQueueDirection::Download => {
                        let path = directory.clone();
                        tokio::task::spawn_blocking(move || {
                            local_directory(Path::new(&path), true)
                        })
                        .await
                        .map_err(|_| "创建本地目录失败")??;
                    }
                }
            }
            plan.initialized = true;
        }
        #[cfg(test)]
        const BATCH: usize = 3;
        #[cfg(not(test))]
        const BATCH: usize = 96;
        let mut last = (Instant::now(), job.view().bytes);
        while plan.next < plan.files.len() {
            job.ready().await?;
            let end = (plan.next + BATCH).min(plan.files.len());
            let chunk = &plan.files[plan.next..end];
            let queue = self.upload_queue(&job.registry, id).await?;
            if queue.items.is_empty() {
                let request = SftpRequest {
                    operation: SftpOperation::QueueEnqueueAndStart as i32,
                    session_id: id.as_uuid().as_bytes().to_vec(),
                    expected_revision: queue.revision,
                    confirmed: true,
                    document_json: serde_json::to_vec(chunk).map_err(|_| "无法创建传输队列")?,
                    ..Default::default()
                };
                let reply = self
                    .workspace_request(
                        request,
                        id,
                        SftpOperation::QueueEnqueueAndStart,
                        &job.registry,
                    )
                    .await;
                if reply.status != SftpStatus::Ok as i32 {
                    return Err(reply.detail);
                }
            } else if queue
                .items
                .iter()
                .any(|r| r.state != SftpQueueState::Succeeded)
            {
                let retry = queue
                    .items
                    .iter()
                    .filter(|r| {
                        r.state != SftpQueueState::Succeeded && r.state != SftpQueueState::Pending
                    })
                    .map(|r| {
                        if !r.can_retry_verified() {
                            return Err("原任务结果需要核对，不能自动重试");
                        }
                        Ok(r.id)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if !retry.is_empty() {
                    let request = SftpRequest {
                        operation: SftpOperation::QueueRetryMany as i32,
                        session_id: id.as_uuid().as_bytes().to_vec(),
                        expected_revision: queue.revision,
                        confirmed: true,
                        document_json: serde_json::to_vec(&SftpQueueRetry {
                            items: retry,
                            start: true,
                        })
                        .map_err(|_| "重试请求无效")?,
                        ..Default::default()
                    };
                    let reply = self
                        .queue(request, id, SftpOperation::QueueRetryMany, &job.registry)
                        .await;
                    if reply.status != SftpStatus::Ok as i32 {
                        return Err(reply.detail);
                    }
                } else {
                    self.upload_mutate(
                        &job.registry,
                        id,
                        SftpOperation::QueueStartManaged,
                        Vec::new(),
                    )
                    .await?;
                }
            }
            let mut paused = false;
            loop {
                job.check()?;
                let queue = self.upload_queue(&job.registry, id).await?;
                let want = job.control().is_paused();
                if want != paused {
                    for row in queue
                        .items
                        .iter()
                        .filter(|r| r.state == SftpQueueState::Running)
                    {
                        if let Some(transfer) = row.transfer_id {
                            let op = if want {
                                SftpOperation::PauseTransfer
                            } else {
                                SftpOperation::ResumeTransfer
                            };
                            let reply = self.transfer_status(
                                SftpRequest {
                                    operation: op as i32,
                                    session_id: id.as_uuid().as_bytes().to_vec(),
                                    transfer_id: transfer.as_uuid().as_bytes().to_vec(),
                                    ..Default::default()
                                },
                                op,
                                id,
                            );
                            if reply.status != SftpStatus::Ok as i32 {
                                return Err("暂停或继续未确认，请刷新状态".into());
                            }
                        }
                    }
                    self.upload_mutate(
                        &job.registry,
                        id,
                        if want {
                            SftpOperation::QueuePause
                        } else {
                            SftpOperation::QueueStartManaged
                        },
                        Vec::new(),
                    )
                    .await?;
                    paused = want;
                    continue;
                }
                let completed = queue
                    .items
                    .iter()
                    .filter(|r| r.state == SftpQueueState::Succeeded)
                    .count() as u32;
                let bytes = queue
                    .items
                    .iter()
                    .map(|r| {
                        r.receipt
                            .as_ref()
                            .map(|r| r.bytes)
                            .or_else(|| {
                                r.transfer_id.and_then(|transfer| {
                                    self.jobs
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .get(&transfer.as_uuid().as_bytes().to_vec())
                                        .map(|j| j.snapshot.bytes_transferred)
                                })
                            })
                            .unwrap_or(0)
                    })
                    .sum::<u64>()
                    + plan.base_bytes;
                let now = Instant::now();
                let speed = if paused {
                    0
                } else {
                    ((bytes.saturating_sub(last.1)) as f64
                        / now.duration_since(last.0).as_secs_f64().max(0.1))
                        as u64
                };
                last = (now, bytes);
                job.update(|v| {
                    v.state = if paused {
                        TerminalUploadState::Paused
                    } else {
                        TerminalUploadState::Running
                    };
                    v.completed = plan.base_files + completed;
                    v.bytes = bytes.min(v.total_bytes);
                    v.bytes_per_second = speed;
                    v.detail = if paused {
                        "传输已暂停"
                    } else {
                        "正在传输；同名文件不会覆盖"
                    }
                    .into();
                });
                if let Some(row) = queue.items.iter().find(|r| {
                    !queue.retry_items.contains(&r.id) && matches!(
                        r.state,
                        SftpQueueState::Unknown
                            | SftpQueueState::CleanupRequired
                            | SftpQueueState::Cancelled
                    )
                }) {
                    return Err(row
                        .receipt
                        .as_ref()
                        .and_then(|r| r.detail.clone())
                        .unwrap_or_else(|| "部分文件未完成，请检查冲突或权限后重试".into()));
                }
                if completed as usize == chunk.len() {
                    plan.base_bytes = bytes;
                    plan.base_files += completed;
                    plan.next = end;
                    for row in queue.items {
                        self.upload_mutate(
                            &job.registry,
                            id,
                            SftpOperation::QueueRemove,
                            row.id.as_uuid().as_bytes().to_vec(),
                        )
                        .await?;
                    }
                    break;
                }
                if queue.paused
                    && !paused
                    && !queue
                        .items
                        .iter()
                        .any(|r| r.state == SftpQueueState::Running)
                {
                    return Err("传输容量不足，请等待其他任务结束后重试".into());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        Ok(())
    }
}
