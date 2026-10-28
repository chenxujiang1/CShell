use super::*;
mod wait;
pub(super) use wait::WaitCursor;
use wait::LiteralWaitMatcher;
pub(super) struct Broker {
    pub local: Arc<LocalSessionRegistry>,
    pub ssh: Arc<SshSessionRegistry>,
    pub profiles: Arc<ProfileIpcService>,
    pub tasks: Option<TaskIpcService>,
    pub tools: crate::terminal_tools_ipc::TerminalToolsService,
    pub session: Option<BoundSession>,
    pub receipt: ScriptLiveReview,
    pub script_id: String,
    pub secrets: BTreeSet<String>,
    pub consumed_tasks: Mutex<BTreeSet<String>>,
    pub cursor: tokio::sync::Mutex<WaitCursor>,
    pub pending_effect: AtomicBool,
    pub runtime: tokio::runtime::Handle,
}
impl ScriptHostBroker for Broker {
    fn authorized(&self) -> bool {
        self.profiles
            .vault
            .commit_auth(self.receipt.authentication_generation, || ())
            .is_ok()
            && match (&self.session, self.receipt.scope.session) {
                (Some(session), Some(id)) => session.current(id, &self.local, &self.ssh),
                (None, None) => true,
                _ => false,
            }
            && self.receipt.scope.trigger_event.is_none_or(|event| {
                self.receipt
                    .scope
                    .session
                    .is_some_and(|id| self.tools.script_event_current(event, id, &self.script_id))
            })
            && (!self.receipt.scope.trigger_on_next_match
                || self.receipt.scope.session.is_some_and(|id| {
                    self.tools.script_arm_current(id, &self.script_id) == self.receipt.trigger_run
                }))
    }
    fn effects_confirmed(&self) -> bool {
        !self.pending_effect.load(Ordering::Acquire)
    }
    fn invoke(&self, operation: ScriptHostOperation) -> ScriptHostFuture<'_> {
        Box::pin(async move {
            if !self.authorized() {
                return Ok(HostResult::Denied);
            }
            let _catalog_gate = self.profiles.catalog_mutation.lock().await;
            let catalog = self
                .profiles
                .service
                .load()
                .await
                .map_err(|_| ScriptWorkerError::Unavailable)?
                .snapshot();
            if catalog.revision != self.receipt.catalog_revision || !self.authorized() {
                return Ok(HostResult::Denied);
            }
            drop(_catalog_gate);
            match operation {
                ScriptHostOperation::Send {
                    text,
                    append_newline,
                } => self.send(text, append_newline),
                ScriptHostOperation::SendParameter {
                    name,
                    value,
                    append_newline,
                } => {
                    if self.secrets.contains(&name)
                        && !self.receipt.scope.secret_parameters.contains(&name)
                    {
                        return Ok(HostResult::Denied);
                    }
                    self.send(value, append_newline)
                }
                ScriptHostOperation::Wait { text, timeout_ms } => {
                    self.wait_for(text, timeout_ms).await
                }
                ScriptHostOperation::ProfileRead => {
                    let Some(profile) = &self.receipt.profile else {
                        return Ok(HostResult::Denied);
                    };
                    Ok(HostResult::Value {
                        value: serde_json::to_string(profile)
                            .map_err(|_| ScriptWorkerError::Protocol)?,
                    })
                }
                ScriptHostOperation::Notify { message: _ } => {
                    // Bodies are deliberately not returned through polling or
                    // tracing; UI shows the acknowledged notification counter.
                    Ok(HostResult::Ok)
                }
                ScriptHostOperation::TaskSubmit { binding } => {
                    let Some(task) = self.receipt.scope.tasks.iter().find(|t| t.name == binding)
                    else {
                        return Ok(HostResult::Denied);
                    };
                    if !self
                        .consumed_tasks
                        .lock()
                        .map_err(|_| ScriptWorkerError::Protocol)?
                        .insert(binding)
                    {
                        return Ok(HostResult::Denied);
                    }
                    check_task(self.tasks.as_ref(), task)
                        .await
                        .map_err(|_| ScriptWorkerError::Capability)?;
                    if !self.authorized() {
                        return Ok(HostResult::Denied);
                    }
                    let tasks = self.tasks.clone().ok_or(ScriptWorkerError::Capability)?;
                    let mut request = TaskRequest::default();
                    request.operation = TaskOperation::Start as i32;
                    request.run_id = task.run.as_uuid().as_bytes().to_vec();
                    request.expected_revision = task.revision;
                    request.confirmed = true;
                    self.pending_effect.store(true, Ordering::Release);
                    // The daemon runtime owns accepted jobs; finishing the JS
                    // supervisor's private runtime must not abort the scheduler.
                    let job = self
                        .profiles
                        .vault
                        .commit_auth(self.receipt.authentication_generation, || {
                            self.runtime
                                .spawn(async move { tasks.handle(&request).await })
                        })
                        .map_err(|_| ScriptWorkerError::Capability)?;
                    let reply = job.await.map_err(|_| ScriptWorkerError::Protocol)?;
                    if reply.status != TaskStatus::Ok as i32
                        || reply.run_id != task.run.as_uuid().as_bytes().to_vec()
                    {
                        return Err(ScriptWorkerError::Protocol);
                    }
                    self.pending_effect.store(false, Ordering::Release);
                    Ok(HostResult::Value {
                        value: task.run.to_string(),
                    })
                }
            }
        })
    }
}
impl Broker {
    fn send(
        &self,
        mut text: Zeroizing<String>,
        append_newline: bool,
    ) -> Result<HostResult, ScriptWorkerError> {
        if text.is_empty() || text.len() > 8192 || text.contains('\0') {
            return Ok(HostResult::Denied);
        }
        let Some(session) = &self.session else {
            return Ok(HostResult::Denied);
        };
        if append_newline {
            text.push('\r');
        }
        let accepted = self
            .profiles
            .vault
            .commit_auth(self.receipt.authentication_generation, || {
                // Check the pinned instance again while committing the intent. A
                // replacement with the same public SessionId never inherits it.
                if !self
                    .receipt
                    .scope
                    .session
                    .is_some_and(|id| session.current(id, &self.local, &self.ssh))
                {
                    return false;
                }
                session.send(text.to_string())
            })
            .map_err(|_| ScriptWorkerError::Capability)?;
        Ok(if accepted {
            HostResult::Ok
        } else {
            HostResult::Denied
        })
    }
    async fn wait_for(
        &self,
        text: Zeroizing<String>,
        timeout_ms: u32,
    ) -> Result<HostResult, ScriptWorkerError> {
        let session = self.session.as_ref().ok_or(ScriptWorkerError::Capability)?;
        let index = session
            .journal(&self.local)
            .map_err(|_| ScriptWorkerError::Capability)?;
        if !index.recording_enabled() {
            return Ok(HostResult::Denied);
        }
        let mut matcher = LiteralWaitMatcher::new(text)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        let mut cursor = self.cursor.lock().await;
        let mut scanned = 0usize;
        loop {
            if !self.authorized() {
                return Ok(HostResult::Denied);
            }
            if cursor.match_pending(&mut matcher) {
                return Ok(HostResult::Boolean { value: true });
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(HostResult::Boolean { value: false });
            }
            let reader = index.clone();
            let (sequence, offset) = cursor.position();
            static READERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
                std::sync::OnceLock::new();
            let permit = READERS
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
                .clone()
                .try_acquire_owned()
                .map_err(|_| ScriptWorkerError::Busy)?;
            let raw = tokio::time::timeout_at(
                deadline,
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    reader.read_raw_slice(sequence, offset)
                }),
            )
            .await
            .map_err(|_| ScriptWorkerError::Protocol)?
            .map_err(|_| ScriptWorkerError::Protocol)?
            .map_err(|_| ScriptWorkerError::Protocol)?;
            if raw.sequence >= raw.total_records {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            scanned = scanned
                .checked_add(raw.data.len())
                .ok_or(ScriptWorkerError::Protocol)?;
            if scanned > 256 * 1024 {
                return Err(ScriptWorkerError::Protocol);
            }
            if cursor.consume(&raw, &mut matcher)? {
                return Ok(HostResult::Boolean { value: true });
            }
            tokio::task::yield_now().await;
        }
    }
}
