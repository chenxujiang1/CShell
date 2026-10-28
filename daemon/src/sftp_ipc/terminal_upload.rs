//! Uploads initiated by dropping local paths on an authenticated SSH terminal.
use super::*;
use cshell_application::{
    TerminalUploadProtocol as Protocol, TerminalUploadSnapshot as Snapshot,
    TerminalUploadStart as Start, TerminalUploadState as State, *,
};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub(super) struct Uploads(Arc<Mutex<BTreeMap<SessionId, Arc<Job>>>>);
struct Job {
    source_epoch: u64,
    snapshot: Mutex<Snapshot>,
    control: TransferControl,
    confirmed: AtomicBool,
}
impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalUploadJob").finish_non_exhaustive()
    }
}
impl Job {
    fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
    }
    fn view(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn check(&self, source: &crate::SshSession) -> Result<(), String> {
        if !source.running() || source.transport_generation() != self.source_epoch {
            self.control.cancel();
        }
        if self.control.is_cancelled() || !source.running() {
            Err("上传已取消或原 SSH 会话已断开".into())
        } else {
            Ok(())
        }
    }
}
#[derive(Debug)]
pub(super) struct Plan {
    pub(super) files: Vec<(String, String, u64)>,
    pub(super) directories: Vec<String>,
    pub(super) bytes: u64,
    has_directories: bool,
}
fn component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}
pub(super) fn plan(paths: &[String], control: &TransferControl) -> Result<Plan, String> {
    let mut result = Plan {
        files: Vec::new(),
        directories: Vec::new(),
        bytes: 0,
        has_directories: false,
    };
    let mut seen = BTreeSet::new();
    let mut pending = Vec::new();
    for path in paths {
        let native = Path::new(path);
        if path.len() > 4096
            || !native.is_absolute()
            || native
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("请拖入有效的本地绝对路径".into());
        }
        let name = native
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| component(s))
            .ok_or("文件或文件夹名称不能用于远端路径")?;
        if !seen.insert(path.clone()) {
            continue;
        }
        pending.push((native.to_path_buf(), name.to_owned(), 0usize));
    }
    let mut destinations = BTreeSet::new();
    while let Some((path, relative, depth)) = pending.pop() {
        if control.is_cancelled() {
            return Err("上传已取消".into());
        }
        if depth > 64
            || result.files.len() + result.directories.len() >= 10_000
            || relative.len() > 3500
        {
            return Err("文件夹超出单次 10000 项、64 层或路径长度限制，请拆分上传".into());
        }
        if !destinations.insert(relative.clone()) {
            return Err("拖入内容包含同名或重复的目标路径，请分开上传".into());
        }
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|_| "无法读取拖入的文件或文件夹")?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("暂不跟随符号链接或目录联接，请拖入实际文件夹".into());
            }
        }
        if metadata.file_type().is_symlink() {
            return Err("暂不跟随符号链接，请拖入实际文件或文件夹".into());
        }
        if metadata.is_dir() {
            result.has_directories = true;
            result.directories.push(relative.clone());
            for entry in std::fs::read_dir(&path).map_err(|_| "无法遍历本地文件夹")? {
                let entry = entry.map_err(|_| "读取本地目录项失败")?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "文件名称不是有效 Unicode")?;
                if !component(&name) {
                    return Err("文件名称不能用于远端路径".into());
                }
                if pending.len() + result.files.len() + result.directories.len() >= 10_000 {
                    return Err("单次文件夹上传最多 10000 项，请拆分上传".into());
                }
                pending.push((entry.path(), format!("{relative}/{name}"), depth + 1));
            }
        } else if metadata.is_file() {
            if metadata.len() > MAX_SFTP_VERIFIED_BYTES {
                return Err("单个文件超过 64 GiB 上传限制".into());
            }
            result.bytes = result
                .bytes
                .checked_add(metadata.len())
                .ok_or("上传总大小超限")?;
            result.files.push((
                path.to_str().ok_or("路径不是有效 Unicode")?.to_owned(),
                relative,
                metadata.len(),
            ));
        } else {
            return Err("只支持普通文件和文件夹，不能上传设备或特殊文件".into());
        }
    }
    result
        .directories
        .sort_by_key(|s| (s.matches('/').count(), s.clone()));
    result.files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(result)
}
impl SftpIpcService {
    pub(super) async fn terminal_upload(
        &self,
        request: SftpRequest,
        source_id: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        let Ok(bytes) = <[u8; 16]>::try_from(request.transfer_id.as_slice()) else {
            return SftpResponse::with_status(SftpStatus::InvalidRequest, "上传任务标识无效");
        };
        let id = SessionId::from_bytes(bytes);
        let canonical = SftpRequest {
            operation: operation as i32,
            session_id: request.session_id.clone(),
            transfer_id: request.transfer_id.clone(),
            confirmed: operation == SftpOperation::TerminalUploadStart,
            document_json: if operation == SftpOperation::TerminalUploadStart {
                request.document_json.clone()
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        if canonical != request {
            return SftpResponse::with_status(SftpStatus::InvalidRequest, "上传请求字段不匹配");
        }
        if id.as_uuid().is_nil() || request.document_json.len() > 1024 * 1024 {
            return SftpResponse::with_status(SftpStatus::InvalidRequest, "上传请求过大或标识无效");
        }
        let job = if operation == SftpOperation::TerminalUploadStart {
            let options: Start = match serde_json::from_slice(&request.document_json) {
                Ok(v) => v,
                Err(_) => {
                    return SftpResponse::with_status(SftpStatus::InvalidRequest, "上传参数无效");
                }
            };
            if !request.confirmed || options.paths.is_empty() || options.paths.len() > 128 {
                return SftpResponse::with_status(
                    SftpStatus::InvalidRequest,
                    "一次可拖入 1～128 个文件或文件夹",
                );
            }
            let source = match sessions.get(source_id) {
                Ok(s) if s.running() => s,
                _ => {
                    return SftpResponse::with_status(
                        SftpStatus::Unavailable,
                        "请先连接目标 SSH 会话",
                    );
                }
            };
            let peer = match source.clipboard_trust() {
                Some(p) => p,
                None => {
                    return SftpResponse::with_status(
                        SftpStatus::Unavailable,
                        "目标 SSH 缺少已验证的主机身份",
                    );
                }
            };
            let mut jobs = self
                .uploads
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.contains_key(&id) {
                return SftpResponse::with_status(
                    SftpStatus::Conflict,
                    "此上传请求已经提交，请查询进度，不要重复提交",
                );
            }
            if jobs.values().filter(|j| !j.view().state.finished()).count() >= 4 {
                return SftpResponse::with_status(
                    SftpStatus::CapacityReached,
                    "同时上传的批次已满，请等待现有传输结束",
                );
            }
            if jobs.len() >= 64 {
                jobs.retain(|_, j| !j.view().state.finished());
            }
            let job = Arc::new(Job {
                source_epoch: source.transport_generation(),
                snapshot: Mutex::new(Snapshot {
                    id,
                    source: source_id,
                    host: format!("{}@{}:{}", peer.username, peer.host.host, peer.host.port),
                    directory: String::new(),
                    protocol: options.protocol,
                    state: State::Preparing,
                    bytes: 0,
                    total_bytes: 0,
                    completed: 0,
                    total_files: 0,
                    current_name: String::new(),
                    bytes_per_second: 0,
                    detail: "正在确认 SSH 当前目录并整理文件…".into(),
                }),
                control: TransferControl::default(),
                confirmed: AtomicBool::new(!options.confirm_before_start),
            });
            jobs.insert(id, job.clone());
            let service = self.clone();
            let task = job.clone();
            tokio::spawn(async move {
                let outcome = service
                    .run_terminal_upload(task.clone(), source, options)
                    .await;
                task.update(|s| {
                    s.bytes_per_second = 0;
                    match outcome {
                        Ok(()) => {
                            s.state = State::Succeeded;
                            s.detail = "上传完成".into();
                        }
                        Err(error) => {
                            s.state = if task.control.is_cancelled() {
                                State::Cancelled
                            } else {
                                State::Failed
                            };
                            s.detail = error;
                        }
                    }
                });
            });
            job
        } else {
            let job = self
                .uploads
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .cloned();
            let Some(job) = job.filter(|j| j.view().source == source_id) else {
                return SftpResponse::with_status(
                    SftpStatus::Unavailable,
                    "上传任务不存在或不属于此 SSH 会话",
                );
            };
            if !job.view().state.finished() {
                match operation {
                    SftpOperation::TerminalUploadPause => {
                        if job.view().protocol == Protocol::Zmodem {
                            return SftpResponse::with_status(
                                SftpStatus::Unsupported,
                                "ZMODEM 不支持可靠暂停，可取消后重新上传",
                            );
                        }
                        job.control.pause();
                    }
                    SftpOperation::TerminalUploadContinue => {
                        job.confirmed.store(true, Ordering::Release);
                        job.control.resume();
                    }
                    SftpOperation::TerminalUploadCancel => job.control.cancel(),
                    _ => {}
                }
            }
            job
        };
        SftpResponse {
            document_json: serde_json::to_vec(&job.view()).unwrap_or_default(),
            ..SftpResponse::with_status(SftpStatus::Ok, "")
        }
    }
    async fn run_terminal_upload(
        &self,
        job: Arc<Job>,
        source: Arc<crate::SshSession>,
        options: Start,
    ) -> Result<(), String> {
        job.check(&source)?;
        let lease = source.fork_transport().map_err(|_| "原 SSH 连接已断开")?;
        let directory = source.current_remote_directory().await.filter(|p| sftp_remote_path(p,true)).ok_or("无法确认此 SSH 的当前目录，未上传任何内容。请在 shell 提示符下重新尝试，或打开 SFTP 明确选择目录。")?;
        job.update(|s| s.directory = directory.clone());
        let paths = options.paths.clone();
        let control = job.control.clone();
        let plan = tokio::task::spawn_blocking(move || plan(&paths, &control))
            .await
            .map_err(|_| "整理文件任务失败")??;
        job.check(&source)?;
        let protocol = if plan.has_directories {
            Protocol::Sftp
        } else {
            options.protocol
        };
        if protocol == Protocol::Zmodem && plan.files.iter().any(|f| f.2 > u64::from(u32::MAX)) {
            return Err("ZMODEM 仅支持小于 4 GiB 的普通文件，请选择 SFTP".into());
        }
        job.update(|s| {
            s.directory = directory.clone();
            s.protocol = protocol;
            s.total_files = plan.files.len() as u32;
            s.total_bytes = plan.bytes;
            s.state = State::Ready;
            s.detail = if plan.has_directories && options.protocol == Protocol::Zmodem {
                "文件夹使用 SFTP，保留目录结构"
            } else {
                "目标目录已确认"
            }
            .into();
        });
        while !job.confirmed.load(Ordering::Acquire) {
            job.check(&source)?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        job.check(&source)?;
        job.update(|s| s.state = State::Running);
        let result = if protocol == Protocol::Sftp {
            self.upload_sftp(
                &job,
                &source,
                &lease,
                &plan,
                &directory,
                options.verification,
            )
            .await
        } else {
            self.upload_zmodem(&job, &source, &lease, &plan, &directory)
                .await
        };
        lease.release().await;
        result
    }
    async fn upload_sftp(
        &self,
        job: &Arc<Job>,
        source: &crate::SshSession,
        lease: &crate::shared_ssh::TransportLease,
        plan: &Plan,
        directory: &str,
        verification: cshell_domain::SftpVerification,
    ) -> Result<(), String> {
        let profile = source.profile_id().ok_or("SSH 会话缺少配置身份")?;
        let client = lease
            .client()
            .open_sftp()
            .await
            .map_err(|_| "当前 SSH 服务器无法打开 SFTP 子系统")?;
        client
            .directory_path(directory, false)
            .await
            .map_err(|_| "SSH 当前目录在 SFTP 中不可访问，未改用其他目录")?;
        for relative in &plan.directories {
            job.check(source)?;
            let path = format!("{}/{}", directory.trim_end_matches('/'), relative);
            match client
                .metadata(path.clone(), cshell_sftp::SymlinkPolicy::NoFollow)
                .await
            {
                Ok(m) if m.file_type == RemoteFileType::Directory => {}
                Ok(_) => return Err(format!("目标存在同名文件或链接，未覆盖：{relative}")),
                Err(error) if error.is_not_found() => {
                    client
                        .create_directory(&path, 0o755)
                        .await
                        .map_err(|_| format!("无法创建目标文件夹：{relative}"))?
                }
                Err(_) => {
                    return Err(format!(
                        "无法确认目标目录状态或权限，未继续上传：{relative}"
                    ));
                }
            }
        }
        let _ = client.close().await;
        // A private registry isolates this drop's queue from SFTP browser tabs.
        let session = job.view().id;
        let registry = SshSessionRegistry::default();
        let target = Arc::new(crate::sftp_lifecycle::SftpTarget::Shared {
            info: SftpConnectionInfo {
                session,
                profile,
                title: source.title().to_owned(),
                peer: source.clipboard_trust().cloned().ok_or("缺少主机身份")?,
                running: true,
                shared_from: Some(source.id()),
                initial_directory: Some(directory.into()),
            },
            lease: lease.clone(),
            stopped: AtomicBool::new(false),
        });
        registry
            .sftp_targets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session, target);
        let started = Instant::now();
        let mut base_bytes = 0u64;
        let mut base_files = 0u32;
        let result = async {
            // A smaller test batch exercises rollover without 97 durable file journals.
            #[cfg(test)] const BATCH:usize=3;
            #[cfg(not(test))] const BATCH:usize=96;
            for chunk in plan.files.chunks(BATCH) {
                job.check(source)?;
                let before = self.upload_queue(&registry, session).await?;
                for row in before.items {
                    self.upload_mutate(
                        &registry,
                        session,
                        SftpOperation::QueueRemove,
                        row.id.as_uuid().as_bytes().to_vec(),
                    )
                    .await?;
                }
                let before = self.upload_queue(&registry, session).await?;
                let drafts: Vec<_> = chunk
                    .iter()
                    .map(|(local, relative, _)| SftpQueueDraft {
                        local_path: local.clone(),
                        remote_path: format!("{}/{}", directory.trim_end_matches('/'), relative),
                        direction: SftpQueueDirection::Upload,
                        verified: true,
                        verification: Some(verification),
                        ..Default::default()
                    })
                    .collect();
                let request = SftpRequest {
                    operation: SftpOperation::QueueEnqueueAndStart as i32,
                    session_id: session.as_uuid().as_bytes().to_vec(),
                    expected_revision: before.revision,
                    confirmed: true,
                    document_json: serde_json::to_vec(&drafts).map_err(|_| "上传队列编码失败")?,
                    ..Default::default()
                };
                let reply = self
                    .workspace_request(
                        request,
                        session,
                        SftpOperation::QueueEnqueueAndStart,
                        &registry,
                    )
                    .await;
                if reply.status != SftpStatus::Ok as i32 {
                    return Err(reply.detail);
                }
                let mut paused = false;
                loop {
                    job.check(source)?;
                    let queue = self.upload_queue(&registry, session).await?;
                    let want_pause = job.control.is_paused();
                    if want_pause != paused {
                        for row in queue
                            .items
                            .iter()
                            .filter(|r| r.state == SftpQueueState::Running)
                        {
                            if let Some(id) = row.transfer_id {
                                let operation = if want_pause {
                                    SftpOperation::PauseTransfer
                                } else {
                                    SftpOperation::ResumeTransfer
                                };
                                let response=self.transfer_status(
                                    SftpRequest {
                                        operation: operation as i32,
                                        session_id: session.as_uuid().as_bytes().to_vec(),
                                        transfer_id: id.as_uuid().as_bytes().to_vec(),
                                        ..Default::default()
                                    },
                                    operation,
                                    session,
                                );
                                if response.status!=SftpStatus::Ok as i32 {return Err("部分文件暂停或继续的回执未确认，请检查进度".into());}
                            }
                        }
                        if want_pause {
                            self.upload_mutate(
                                &registry,
                                session,
                                SftpOperation::QueuePause,
                                Vec::new(),
                            )
                            .await?;
                        }
                        if !want_pause {
                            self.upload_mutate(
                                &registry,
                                session,
                                SftpOperation::QueueStartManaged,
                                Vec::new(),
                            )
                            .await?;
                        }
                        paused = want_pause;
                        continue; // Controls changed the durable queue; inspect a fresh snapshot.
                    }
                    let bytes = queue
                        .items
                        .iter()
                        .map(|r| {
                            r.receipt.as_ref().map_or_else(
                                || r.verified.as_ref().map_or(0, |v| v.bytes),
                                |r| r.bytes,
                            )
                        })
                        .sum::<u64>();
                    let complete = queue
                        .items
                        .iter()
                        .filter(|r| r.state == SftpQueueState::Succeeded)
                        .count() as u32;
                    let current = queue
                        .items
                        .iter()
                        .find(|r| r.state == SftpQueueState::Running)
                        .and_then(|r| Path::new(&r.draft.local_path).file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_owned();
                    job.update(|s| {
                        s.bytes = base_bytes + bytes;
                        s.completed = base_files + complete;
                        s.current_name = current;
                        s.state = if paused {
                            State::Paused
                        } else {
                            State::Running
                        };
                        s.bytes_per_second = if paused {
                            0
                        } else {
                            (s.bytes as f64 / started.elapsed().as_secs_f64().max(0.1)) as u64
                        };
                        s.detail = if paused {
                            "上传已暂停"
                        } else {
                            "正在上传，已有同名文件不会被覆盖"
                        }
                        .into();
                    });
                    if queue.paused && !want_pause && queue.items.iter().any(|r|r.state==SftpQueueState::Pending) && !queue.items.iter().any(|r|r.state==SftpQueueState::Running) {
                        return Err("上传队列已停止，可能达到传输容量限制；请等待其他传输结束后重新拖入未完成的文件".into());
                    }
                    if let Some(row) = queue.items.iter().find(|r| {
                        matches!(
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
                            .unwrap_or_else(|| {
                                "部分文件未完成；请在 SFTP 中检查同名冲突、权限或剩余临时文件"
                                    .into()
                            }));
                    }
                    if complete as usize == chunk.len() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                base_bytes += chunk.iter().map(|r| r.2).sum::<u64>();
                base_files += chunk.len() as u32;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            // Stop dispatch before cancelling active workers and pending rows.
            let _ = self
                .upload_mutate(&registry, session, SftpOperation::QueuePause, Vec::new())
                .await;
            self.stop_session_transfers(session);
            let _ = self
                .upload_mutate(
                    &registry,
                    session,
                    SftpOperation::QueueCancelPending,
                    Vec::new(),
                )
                .await;
        }
        // Closing only the child consumer never closes the source terminal.
        if let Ok(target) = registry.get_sftp(session) {
            target.close().await;
        }
        result
    }
    pub(super) async fn upload_queue(
        &self,
        registry: &SshSessionRegistry,
        session: SessionId,
    ) -> Result<SftpQueueSnapshot, String> {
        let reply = self
            .queue(
                SftpRequest {
                    operation: SftpOperation::QueueGet as i32,
                    session_id: session.as_uuid().as_bytes().to_vec(),
                    ..Default::default()
                },
                session,
                SftpOperation::QueueGet,
                registry,
            )
            .await;
        if reply.status != SftpStatus::Ok as i32 {
            return Err(reply.detail);
        }
        decode_sftp_queue(&reply.document_json).map_err(str::to_owned)
    }
    pub(super) async fn upload_mutate(
        &self,
        registry: &SshSessionRegistry,
        session: SessionId,
        operation: SftpOperation,
        item: Vec<u8>,
    ) -> Result<(), String> {
        self.mutate_upload_queue(registry, session, operation, item)
            .await
            .map_err(|_| "上传队列控制未完成，请检查连接和当前传输状态".to_owned())
    }
    async fn upload_zmodem(
        &self,
        job: &Arc<Job>,
        source: &crate::SshSession,
        lease: &crate::shared_ssh::TransportLease,
        plan: &Plan,
        directory: &str,
    ) -> Result<(), String> {
        let command = format!("cd -- '{}' && exec rz -E", directory.replace('\'', "'\\''"));
        let mut channel = lease
            .client()
            .open_exec()
            .await
            .map_err(|_| "无法打开独立 ZMODEM 渠道")?;
        channel
            .request_bidirectional(command.as_bytes())
            .await
            .map_err(|_| "服务器拒绝启动独立 ZMODEM 上传")?;
        let transfer = Arc::new(Mutex::new(crate::zmodem::Transfer::default()));
        let began = Instant::now();
        let mut selected = false;
        let result=async {
            loop {
                job.check(source)?;
                if !selected && began.elapsed()>Duration::from_secs(15){return Err("服务器没有启动 rz，请安装 lrzsz 或将全局上传协议改为 SFTP".into());}
                tokio::select! {
                    event=channel.next_event()=>match event {
                        Some(cshell_ssh::TerminalEvent::Data{data,..})=>{let t=transfer.clone();tokio::task::spawn_blocking(move||{t.lock().unwrap_or_else(std::sync::PoisonError::into_inner).incoming(&data);}).await.map_err(|_|"ZMODEM 接收任务失败")?;},
                        Some(cshell_ssh::TerminalEvent::ExitStatus(code)) if code!=0=>return Err(format!("远端 rz 退出（{code}），请检查目录权限和 lrzsz 安装")),
                        Some(cshell_ssh::TerminalEvent::Closed)|None=>return Err("ZMODEM 渠道提前关闭，请检查远端文件状态".into()),
                        _=>{},
                    },
                    _=tokio::time::sleep(Duration::from_millis(10))=>{},
                }
                let t=transfer.clone();let paths:Vec<_>=plan.files.iter().map(|f|f.0.clone()).collect();
                let (wire,status,did_select)=tokio::task::spawn_blocking(move||{
                    let mut t=t.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let mut did=false;
                    if !selected && let Some(s)=t.status.clone() { if s.state==1 && s.direction==1 {t.control(&cshell_ipc::ZmodemRequest {operation:1,transfer_id:s.transfer_id,paths,..Default::default()})?;did=true;} }
                    Ok::<_,String>((t.pump(),t.status.clone(),did))
                }).await.map_err(|_|"ZMODEM 发送任务失败")??;
                selected|=did_select;
                if !wire.is_empty(){channel.send_input(wire).await.map_err(|_|"ZMODEM 数据发送失败")?;}
                if let Some(status)=status {
                    let complete=status.files_completed as usize;
                    let done=plan.files.iter().take(complete).map(|f|f.2).sum::<u64>();
                    job.update(|s|{s.completed=status.files_completed;s.current_name=status.name;s.bytes=(done+status.bytes).min(s.total_bytes);s.bytes_per_second=(s.bytes as f64/began.elapsed().as_secs_f64().max(0.1)) as u64;s.detail="正在通过独立 ZMODEM 渠道上传".into();});
                    if status.state==3 {job.update(|s|{s.bytes=s.total_bytes;s.completed=s.total_files;});return Ok(());}
                    if matches!(status.state,4|5) {return Err(status.detail);}
                }
            }
        }.await;
        let _ = channel.close().await;
        result
    }
}

#[cfg(test)]
mod tests;
