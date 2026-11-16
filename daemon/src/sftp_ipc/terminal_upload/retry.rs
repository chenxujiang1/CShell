//! Retry the frozen plan using the original verified queue, never a new drop.
use super::*;

pub(super) struct RetryContext {
    pub(super) registry: Arc<SshSessionRegistry>,
    pub(super) plan: Arc<Plan>,
    pub(super) directory: String,
    pub(super) verification: cshell_domain::SftpVerification,
    pub(super) next: usize,
    pub(super) base_bytes: u64,
    pub(super) clear_completed: bool,
    pub(super) queue: Option<SftpQueueSnapshot>,
    saved_at: Instant,
}
impl RetryContext {
    pub(super) fn new(
        plan: Plan,
        directory: String,
        verification: cshell_domain::SftpVerification,
    ) -> Self {
        Self {
            registry: Arc::new(SshSessionRegistry::default()),
            plan: Arc::new(plan),
            directory,
            verification,
            next: 0,
            base_bytes: 0,
            clear_completed: false,
            queue: None,
            saved_at: Instant::now(),
        }
    }
    fn memory(&self) -> usize {
        self.plan
            .files
            .iter()
            .map(|(a, b, _)| a.capacity() + b.capacity() + 64)
            .sum::<usize>()
            + self
                .plan
                .directories
                .iter()
                .map(|s| s.capacity() + 32)
                .sum::<usize>()
    }
    pub(super) fn counts(&self) -> (u32, u32) {
        let succeeded = if self.clear_completed {
            0
        } else {
            self.queue.as_ref().map_or(0, |q| {
                q.items
                    .iter()
                    .filter(|r| r.state == SftpQueueState::Succeeded)
                    .count()
            })
        };
        let complete = (self.next + succeeded).min(self.plan.files.len()) as u32;
        (
            complete,
            (self.plan.files.len() as u32).saturating_sub(complete),
        )
    }
    pub(super) fn safe(&self) -> bool {
        self.saved_at.elapsed() < Duration::from_secs(600)
            && self.queue.as_ref().is_some_and(|q| {
                q.items.iter().all(|r| {
                    matches!(r.state, SftpQueueState::Succeeded | SftpQueueState::Pending)
                        || r.can_retry_verified()
                })
            })
    }
}

pub(super) fn validate_chunk(
    queue: &SftpQueueSnapshot,
    chunk: &[(String, String, u64)],
    directory: &str,
) -> Result<(), String> {
    if queue.items.len() != chunk.len()
        || chunk.iter().any(|(local, relative, _)| {
            let remote = format!("{}/{}", directory.trim_end_matches('/'), relative);
            queue
                .items
                .iter()
                .filter(|r| {
                    r.draft.local_path == *local
                        && r.draft.remote_path == remote
                        && r.draft.direction == SftpQueueDirection::Upload
                        && r.draft.verified
                })
                .count()
                != 1
        })
    {
        return Err("原上传队列与文件清单不一致，请核对远端文件".into());
    }
    Ok(())
}

impl SftpIpcService {
    pub(super) fn retain_upload_retry(&self, job: &Arc<Job>, mut context: RetryContext) {
        // At most eight plans of <= 4 MiB, with no open SSH consumer retained.
        if context.memory() > 4 * 1024 * 1024 {
            return;
        }
        context.saved_at = Instant::now();
        let jobs = self
            .uploads
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut retained = jobs
            .values()
            .filter_map(|j| {
                j.retry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .map(|c| (c.saved_at, j.clone()))
            })
            .collect::<Vec<_>>();
        retained.sort_by_key(|(t, _)| *t);
        let excess = retained.len().saturating_sub(7);
        for (_, old) in retained.into_iter().take(excess) {
            old.retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
        *job.retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context);
    }

    pub(super) async fn terminal_retry(
        &self,
        job: &Arc<Job>,
        revision: u64,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        let snapshot = job.view();
        let source = sessions
            .get(snapshot.source)
            .ok()
            .filter(|s| s.running() && s.transport_generation() == job.source_epoch);
        let review = {
            let retained = job
                .retry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let context = retained.as_ref();
            let (completed_files, retry_files) =
                context.map_or((snapshot.completed, 0), RetryContext::counts);
            let can_retry = snapshot.state == State::Failed
                && !job.active.load(Ordering::Acquire)
                && source.is_some()
                && context.is_some_and(RetryContext::safe)
                && retry_files > 0;
            TerminalUploadRetryPlan {
                id: snapshot.id,
                source: snapshot.source,
                revision: job.revision.load(Ordering::Acquire),
                host: snapshot.host,
                directory: snapshot.directory,
                completed_files,
                retry_files,
                can_retry,
                detail: if can_retry {
                    "只重试原清单中未完成的文件，保留已成功项；同名文件不覆盖。"
                } else if source.is_none() {
                    "原 SSH 连接已失效；请重新连接并核对远端文件后重新选择上传。"
                } else if snapshot.protocol == Protocol::Zmodem {
                    "ZMODEM 无法确认逐文件发布结果，请核对远端后使用 SFTP 上传。"
                } else if job.active.load(Ordering::Acquire) {
                    "原任务仍在收尾，请稍后刷新任务状态。"
                } else if context.is_none()
                    || context.is_some_and(|c| c.saved_at.elapsed() >= Duration::from_secs(600))
                {
                    "重试清单已过期或释放，请核对远端文件后重新选择上传。"
                } else {
                    "部分文件发布结果不确定或需要清理，请先在 SFTP 核对远端文件；不能直接重试。"
                }
                .into(),
            }
        };
        if operation == SftpOperation::TerminalUploadRetryInspect {
            return SftpResponse {
                document_json: serde_json::to_vec(&review).unwrap_or_default(),
                ..Default::default()
            };
        }
        if !review.can_retry || revision == 0 || revision != review.revision {
            return SftpResponse::with_status(
                SftpStatus::Conflict,
                "重试条件或确认版本已变化，请重新检查失败项",
            );
        }
        let Some(source) = source else {
            return SftpResponse::with_status(SftpStatus::Unavailable, "原 SSH 连接已断开");
        };
        {
            // Serialize capacity checks with starts and other retries.
            let jobs = self
                .uploads
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !jobs
                .get(&snapshot.id)
                .is_some_and(|registered| Arc::ptr_eq(registered, job))
            {
                return SftpResponse::with_status(
                    SftpStatus::Unavailable,
                    "原上传记录已释放，请核对远端文件",
                );
            }
            if jobs
                .values()
                .filter(|j| j.active.load(Ordering::Acquire))
                .count()
                >= 4
            {
                return SftpResponse::with_status(
                    SftpStatus::CapacityReached,
                    "请等待其他上传任务完成",
                );
            }
            if job.revision.load(Ordering::Acquire) != revision
                || job.active.swap(true, Ordering::AcqRel)
            {
                return SftpResponse::with_status(
                    SftpStatus::Conflict,
                    "此重试已提交，请刷新任务状态",
                );
            }
            job.revision.fetch_add(1, Ordering::AcqRel);
        }
        let context = job
            .retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(mut context) = context.filter(RetryContext::safe) else {
            job.active.store(false, Ordering::Release);
            return SftpResponse::with_status(SftpStatus::Conflict, "重试清单已释放，请核对原任务");
        };
        *job.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = TransferControl::default();
        job.update(|s| {
            s.state = State::Preparing;
            s.bytes_per_second = 0;
            s.detail = "正在重试原目录中的未完成文件".into();
        });
        let service = self.clone();
        let task = job.clone();
        tokio::spawn(async move {
            let result = async {
                task.check(&source)?;
                let lease = source.fork_transport().map_err(|_| "原 SSH 连接已失效")?;
                context.queue = None;
                let result = service
                    .upload_sftp(&task, &source, &lease, &mut context)
                    .await;
                lease.release().await;
                result
            }
            .await;
            if result.is_err() && !task.control().is_cancelled() {
                service.retain_upload_retry(&task, context);
            }
            task.finish(result);
        });
        SftpResponse {
            document_json: serde_json::to_vec(&job.view()).unwrap_or_default(),
            ..Default::default()
        }
    }

    pub(super) async fn resume_upload_queue(
        &self,
        registry: &SshSessionRegistry,
        session: SessionId,
        queue: &SftpQueueSnapshot,
    ) -> Result<(), String> {
        let items = queue
            .items
            .iter()
            .filter(|r| !matches!(r.state, SftpQueueState::Succeeded | SftpQueueState::Pending))
            .map(|r| {
                if r.can_retry_verified() {
                    Ok(r.id)
                } else {
                    Err("原文件发布结果不确定，请先核对远端")
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if items.is_empty() {
            if queue
                .items
                .iter()
                .any(|r| r.state == SftpQueueState::Pending)
            {
                self.upload_mutate(
                    registry,
                    session,
                    SftpOperation::QueueStartManaged,
                    Vec::new(),
                )
                .await?;
            }
            return Ok(());
        }
        let reply = self
            .queue(
                SftpRequest {
                    operation: SftpOperation::QueueRetryMany as i32,
                    session_id: session.as_uuid().as_bytes().to_vec(),
                    expected_revision: queue.revision,
                    confirmed: true,
                    document_json: serde_json::to_vec(&SftpQueueRetry { items, start: true })
                        .map_err(|_| "重试清单编码失败")?,
                    ..Default::default()
                },
                session,
                SftpOperation::QueueRetryMany,
                registry,
            )
            .await;
        if reply.status != SftpStatus::Ok as i32 {
            return Err(reply.detail);
        }
        Ok(())
    }
}
