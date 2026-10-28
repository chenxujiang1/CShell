use super::containment::ParentContainment;
#[cfg(windows)]
use super::containment::WindowsChild as Child;
use super::*;
#[cfg(windows)]
type ChildStdin = tokio::fs::File;
#[cfg(windows)]
type ChildStdout = tokio::fs::File;
use cshell_application::{ScriptPermissionPlan, plan_script_permissions};
#[cfg(not(windows))]
use std::process::Stdio;
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
#[cfg(not(windows))]
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, watch},
    time::Instant,
};

#[derive(Clone, Debug)]
pub struct ScriptCancellation {
    sender: watch::Sender<bool>,
}
impl Default for ScriptCancellation {
    fn default() -> Self {
        let (sender, _) = watch::channel(false);
        Self { sender }
    }
}
impl ScriptCancellation {
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }
}
struct CancelOnDrop(ScriptCancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
#[derive(Debug)]
pub struct OfflineScriptRun {
    pub(crate) start: WorkerStart,
    pub(crate) parameters: ScriptParameterValues,
    pub(crate) live: Option<crate::broker::LiveAuthority>,
}
impl OfflineScriptRun {
    /// Consumes the reviewed document and ephemeral values. A caller must obtain
    /// explicit offline-run confirmation; an inert plan alone never starts it.
    pub fn review(
        document: ScriptDocument,
        plan: ScriptPermissionPlan,
        parameters: ScriptParameterValues,
    ) -> Result<Self, ScriptWorkerError> {
        if plan_script_permissions(&document, &plan.selected)
            .map_err(|_| ScriptWorkerError::Invalid)?
            != plan
            || !plan.missing.is_empty()
        {
            return Err(ScriptWorkerError::Invalid);
        }
        parameters.validate(&document)?;
        let start = WorkerStart {
            version: WORKER_VERSION,
            run_id: ScriptRunId::new(),
            document,
            document_sha256: plan.document_sha256,
            selected: plan.selected,
            live: false,
        };
        start.validate()?;
        Ok(Self {
            start,
            parameters,
            live: None,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptRunStatus {
    Completed,
    ScriptFailed,
    BudgetExceeded,
    Denied,
    Cancelled,
    TimedOut,
    ProtocolFailed,
    WorkerFailed,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptRunReport {
    pub format: String,
    pub version: u32,
    pub run_id: ScriptRunId,
    pub document_sha256: [u8; 32],
    pub status: ScriptRunStatus,
    pub operations: u16,
    pub notifications: u16,
    pub exit_confirmed: bool,
    pub containment: Option<WorkerContainment>,
    pub sandbox: Option<WorkerSandbox>,
    pub sandbox_cleanup_confirmed: bool,
    pub effects_confirmed: bool,
}
#[derive(Clone, Copy)]
enum Phase {
    Preparing,
    Running,
    Stopping,
    Cleaning,
}
type Progress = watch::Sender<(Phase, ScriptRunReport)>;
// A dropped supervisor leaves its durable slot occupied until explicit recovery
// proves exit. The in-process permit only bounds owner threads still running.
struct Lease {
    _permit: OwnedSemaphorePermit,
    scratch: Option<tempfile::TempDir>,
    proven: bool,
    containment: Option<ParentContainment>,
    slot: crate::recovery::Slot,
    cleanup_result: Option<bool>,
}
impl Lease {
    fn cleanup(&mut self) -> bool {
        if let Some(result) = self.cleanup_result {
            return result;
        }
        if !self.proven {
            return false;
        }
        let mut result = self.slot.exited().is_ok()
            && self
                .containment
                .as_mut()
                .is_none_or(ParentContainment::cleanup);
        if result && let Some(scratch) = self.scratch.take() {
            result = scratch.close().is_ok();
        }
        if result {
            result = self.slot.clear();
        }
        self.cleanup_result = Some(result);
        result
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.cleanup() {
            if let Some(scratch) = self.scratch.take() {
                let _ = scratch.keep();
            }
        }
    }
}
type SharedInput = Arc<tokio::sync::Mutex<ChildStdin>>;
async fn send<T: Serialize>(stdin: &SharedInput, value: &T) -> Result<(), ScriptWorkerError> {
    let bytes = encode_frame(value)?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|_| ScriptWorkerError::Protocol)?;
    stdin
        .write_all(&bytes)
        .await
        .map_err(|_| ScriptWorkerError::Protocol)?;
    stdin.flush().await.map_err(|_| ScriptWorkerError::Protocol)
}
async fn receive(stdout: &mut ChildStdout) -> Result<WorkerEvent, ScriptWorkerError> {
    let mut prefix = [0u8; 4];
    stdout
        .read_exact(&mut prefix)
        .await
        .map_err(|_| ScriptWorkerError::Protocol)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_WORKER_FRAME {
        return Err(ScriptWorkerError::Protocol);
    }
    let mut bytes = Zeroizing::new(vec![0u8; length]);
    stdout
        .read_exact(&mut bytes)
        .await
        .map_err(|_| ScriptWorkerError::Protocol)?;
    serde_json::from_slice(&bytes).map_err(|_| ScriptWorkerError::Protocol)
}
fn worker_path() -> Result<PathBuf, ScriptWorkerError> {
    let executable = std::env::current_exe().map_err(|_| ScriptWorkerError::Unavailable)?;
    let path = executable.with_file_name(if cfg!(windows) {
        "cshell-script-worker.exe"
    } else {
        "cshell-script-worker"
    });
    if !path
        .metadata()
        .map_err(|_| ScriptWorkerError::Unavailable)?
        .is_file()
    {
        return Err(ScriptWorkerError::Unavailable);
    }
    Ok(path)
}
async fn exchange(
    run: &OfflineScriptRun,
    stdin: &SharedInput,
    stdout: &mut ChildStdout,
    report: &mut ScriptRunReport,
) -> Result<ScriptRunStatus, ScriptWorkerError> {
    send(stdin, &HostFrameRef::Start { start: &run.start }).await?;
    let mut sequence = 1u32;
    let mut ready = false;
    let mut result_bytes = 0usize;
    loop {
        match receive(stdout).await? {
            WorkerEvent::Ready {
                run_id,
                sequence,
                containment,
                sandbox,
            } => {
                if ready
                    || sequence != 0
                    || run_id != run.start.run_id
                    || containment != WorkerContainment::current_platform()?
                    || sandbox != WorkerSandbox::current_platform()?
                {
                    return Err(ScriptWorkerError::Protocol);
                }
                ready = true;
                report.containment = Some(containment);
                report.sandbox = Some(sandbox);
            }
            WorkerEvent::Finished {
                run_id,
                sequence: received,
                status,
            } => {
                if !ready || run_id != run.start.run_id || received != sequence {
                    return Err(ScriptWorkerError::Protocol);
                }
                // A final frame is not exit proof. Require EOF and then wait().
                let mut extra = [0u8; 1];
                if stdout
                    .read(&mut extra)
                    .await
                    .map_err(|_| ScriptWorkerError::Protocol)?
                    != 0
                {
                    return Err(ScriptWorkerError::Protocol);
                }
                return Ok(match status {
                    WorkerFinish::Completed => ScriptRunStatus::Completed,
                    WorkerFinish::ScriptFailed => ScriptRunStatus::ScriptFailed,
                    WorkerFinish::BudgetExceeded => ScriptRunStatus::BudgetExceeded,
                    WorkerFinish::ProtocolFailed => ScriptRunStatus::ProtocolFailed,
                });
            }
            WorkerEvent::Call {
                run_id,
                sequence: received,
                request,
            } => {
                if !ready || run_id != run.start.run_id || received != sequence {
                    return Err(ScriptWorkerError::Protocol);
                }
                if report.operations >= run.start.document.limits.max_operations {
                    return Ok(ScriptRunStatus::BudgetExceeded);
                }
                report.operations += 1;
                if run
                    .live
                    .as_ref()
                    .is_some_and(|a| !a.grant.valid(&run.start) || !a.broker.authorized())
                {
                    return Ok(ScriptRunStatus::Denied);
                }
                let result = match &request {
                    WorkerCall::Parameter { name } => {
                        if !run.start.document.parameters.contains_key(name)
                            || (run.live.is_some()
                                && run.start.document.parameters.get(name)
                                    == Some(&cshell_domain::ScriptParameterKind::Secret))
                        {
                            HostResult::Denied
                        } else {
                            run.parameters
                                .values
                                .get(name)
                                .cloned()
                                .map(|value| HostResult::Value { value })
                                .unwrap_or(HostResult::Denied)
                        }
                    }
                    WorkerCall::Sleep { milliseconds } if (1..=30_000).contains(milliseconds) => {
                        tokio::time::sleep(Duration::from_millis(u64::from(*milliseconds))).await;
                        HostResult::Ok
                    }
                    WorkerCall::Notify { message }
                        if run.start.selected.contains(&ScriptCapability::UiNotify)
                            && !message.is_empty()
                            && message.len() <= 512
                            && !message.chars().any(char::is_control) =>
                    {
                        // Notification bodies may contain parameters. Offline
                        // mode acknowledges/counts only, never displays or logs.
                        report.notifications += 1;
                        if let Some(live) = &run.live {
                            live.broker
                                .invoke(ScriptHostOperation::Notify {
                                    message: Zeroizing::new(message.clone()),
                                })
                                .await?
                        } else {
                            HostResult::Ok
                        }
                    }
                    _ => live_call(run, &request).await?,
                };
                let encoded = encode_frame(&result)?;
                result_bytes = result_bytes
                    .checked_add(encoded.len())
                    .ok_or(ScriptWorkerError::Protocol)?;
                if result_bytes > run.start.document.limits.max_result_bytes as usize {
                    return Ok(ScriptRunStatus::BudgetExceeded);
                }
                let denied = matches!(result, HostResult::Denied);
                send(
                    stdin,
                    &HostFrameRef::Reply {
                        reply: &HostReply {
                            run_id,
                            sequence,
                            result,
                        },
                    },
                )
                .await?;
                if denied {
                    return Ok(ScriptRunStatus::Denied);
                }
                sequence = sequence.checked_add(1).ok_or(ScriptWorkerError::Protocol)?;
            }
        }
    }
}
async fn live_call(
    run: &OfflineScriptRun,
    call: &WorkerCall,
) -> Result<HostResult, ScriptWorkerError> {
    let Some(authority) = &run.live else {
        return Ok(HostResult::Denied);
    };
    if !authority.grant.valid(&run.start) || !authority.broker.authorized() {
        return Ok(HostResult::Denied);
    }
    let has = |cap| run.start.selected.contains(&cap);
    let operation = match call {
        WorkerCall::TerminalSend {
            text,
            append_newline,
        } if has(ScriptCapability::TerminalWrite)
            && !text.is_empty()
            && text.len() <= 8192
            && !text.contains('\0') =>
        {
            ScriptHostOperation::Send {
                text: Zeroizing::new(text.clone()),
                append_newline: *append_newline,
            }
        }
        WorkerCall::TerminalSendParameter {
            name,
            append_newline,
        } if has(ScriptCapability::TerminalWrite)
            && run.start.document.parameters.contains_key(name) =>
        {
            let Some(value) = run.parameters.values.get(name) else {
                return Ok(HostResult::Denied);
            };
            ScriptHostOperation::SendParameter {
                name: name.clone(),
                value: Zeroizing::new(value.clone()),
                append_newline: *append_newline,
            }
        }
        WorkerCall::TerminalWait { text, timeout_ms }
            if has(ScriptCapability::TerminalRead)
                && cshell_domain::valid_script_wait_pattern(text)
                && (1..=30_000).contains(timeout_ms) =>
        {
            ScriptHostOperation::Wait {
                text: Zeroizing::new(text.clone()),
                timeout_ms: *timeout_ms,
            }
        }
        WorkerCall::TerminalWaitParameter { name, timeout_ms }
            if has(ScriptCapability::TerminalRead)
                && run.start.document.parameters.get(name)
                    == Some(&cshell_domain::ScriptParameterKind::LiteralPattern)
                && (1..=30_000).contains(timeout_ms) =>
        {
            let Some(value) = run.parameters.values.get(name) else {
                return Ok(HostResult::Denied);
            };
            if !cshell_domain::valid_script_wait_pattern(value) {
                return Ok(HostResult::Denied);
            }
            ScriptHostOperation::Wait {
                text: Zeroizing::new(value.clone()),
                timeout_ms: *timeout_ms,
            }
        }
        WorkerCall::ProfileRead if has(ScriptCapability::ProfileRead) => {
            ScriptHostOperation::ProfileRead
        }
        WorkerCall::TaskSubmit { binding }
            if has(ScriptCapability::TaskSubmit)
                && !binding.is_empty()
                && binding.len() <= 64
                && binding
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            ScriptHostOperation::TaskSubmit {
                binding: binding.clone(),
            }
        }
        _ => return Ok(HostResult::Denied),
    };
    authority.broker.invoke(operation).await
}
async fn authority_lost(run: &OfflineScriptRun) {
    loop {
        if run
            .live
            .as_ref()
            .is_some_and(|a| !a.grant.valid(&run.start) || !a.broker.authorized())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn memory_watch(pid: u32, heap: u64) -> ScriptRunStatus {
    #[cfg(target_os = "macos")]
    loop {
        match super::containment::memory_within_limit(pid, heap) {
            Ok(true) => {}
            Ok(false) => return ScriptRunStatus::BudgetExceeded,
            Err(_) => return ScriptRunStatus::WorkerFailed,
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, heap);
        std::future::pending().await
    }
}
async fn heartbeat(stdin: &SharedInput, run_id: ScriptRunId) -> Result<(), ScriptWorkerError> {
    let mut interval = tokio::time::interval(Duration::from_millis(WORKER_HEARTBEAT_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await; // Start is always written before the first heartbeat.
    loop {
        interval.tick().await;
        send(stdin, &HostFrameRef::Heartbeat { run_id }).await?;
    }
}
async fn cancellation(receiver: &mut watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}
async fn exit(
    mut child: Child,
    mut lease: Lease,
    report: &mut ScriptRunReport,
    progress: &Progress,
) {
    progress.send_replace((Phase::Stopping, report.clone()));
    let already = child.try_wait().ok().flatten();
    // EOF can arrive just before normal process exit. Do not kill a worker
    // between its final frame and successful exit and report false failure.
    let graceful = if already.is_none() && report.status == ScriptRunStatus::Completed {
        tokio::time::timeout(Duration::from_millis(250), child.wait())
            .await
            .ok()
            .and_then(Result::ok)
    } else {
        None
    };
    let status = if let Some(status) = already.or(graceful) {
        Some(status)
    } else {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .ok()
            .and_then(Result::ok)
    };
    if let Some(status) = status {
        lease.proven = true;
        report.exit_confirmed = true;
        if report.status == ScriptRunStatus::Completed && !status.success() {
            report.status = ScriptRunStatus::WorkerFailed;
        }
        drop(child);
        progress.send_replace((Phase::Cleaning, report.clone()));
        report.sandbox_cleanup_confirmed = lease.cleanup();
    } else {
        report.status = ScriptRunStatus::Unknown;
        tokio::spawn(async move {
            if child.wait().await.is_ok() {
                lease.proven = true;
            }
            drop(child);
            // Lease Drop retains the slot/cwd if wait itself remained unknown.
            drop(lease);
        });
    }
}
pub async fn run_offline(
    run: OfflineScriptRun,
    cancel: ScriptCancellation,
) -> Result<ScriptRunReport, ScriptWorkerError> {
    if run.live.is_some() || run.start.live {
        return Err(ScriptWorkerError::Capability);
    }
    run_supervised(run, cancel).await
}
pub(crate) async fn run_supervised(
    run: OfflineScriptRun,
    cancel: ScriptCancellation,
) -> Result<ScriptRunReport, ScriptWorkerError> {
    run.start.validate()?;
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let permit = SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ScriptWorkerError::Busy)?;
    let report = ScriptRunReport {
        format: "cshell.script-run-report".into(),
        version: WORKER_VERSION,
        run_id: run.start.run_id,
        document_sha256: run.start.document_sha256,
        status: ScriptRunStatus::Unknown,
        operations: 0,
        notifications: 0,
        exit_confirmed: false,
        containment: None,
        sandbox: None,
        sandbox_cleanup_confirmed: false,
        effects_confirmed: true,
    };
    let runtime_limit = Duration::from_millis(u64::from(run.start.document.limits.timeout_ms));
    let (progress, mut changed) = watch::channel((Phase::Preparing, report));
    let (reply, mut result) = tokio::sync::oneshot::channel();
    let stop = cancel.clone();
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    // Native prepare/cleanup calls must never block the caller's executor.
    // This owner thread retains process, durable slot and profile on timeout.
    std::thread::Builder::new()
        .name("cshell-script-owner".into())
        .spawn(move || {
            let progress_lifetime = progress.clone();
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| ScriptWorkerError::Unavailable)
                .and_then(|runtime| {
                    let result = runtime.block_on(run_owned(run, cancel, permit, progress));
                    runtime.shutdown_timeout(Duration::from_secs(1));
                    result
                });
            let _ = reply.send(result);
            drop(progress_lifetime);
        })
        .map_err(|_| ScriptWorkerError::Unavailable)?;
    let mut deadline = Instant::now() + Duration::from_secs(5);
    loop {
        tokio::select! {
            biased;
            result = &mut result => return result.unwrap_or(Err(ScriptWorkerError::Unavailable)),
            update = changed.changed() => {
                if update.is_err() { return Err(ScriptWorkerError::Unavailable); }
                let phase = changed.borrow_and_update().0;
                deadline = Instant::now() + match phase {
                    Phase::Preparing | Phase::Cleaning => Duration::from_secs(5),
                    Phase::Running => runtime_limit + Duration::from_millis(250),
                    Phase::Stopping => Duration::from_millis(5500),
                };
            }
            _ = tokio::time::sleep_until(deadline) => {
                stop.cancel();
                let mut report = changed.borrow().1.clone();
                // A cleanup timeout may retain confirmed process exit. Native
                // calls continue only in their owner, never duplicated here.
                report.status = ScriptRunStatus::Unknown;
                report.sandbox_cleanup_confirmed = false;
                return Ok(report);
            }
        }
    }
}
async fn run_owned(
    run: OfflineScriptRun,
    cancel: ScriptCancellation,
    permit: OwnedSemaphorePermit,
    progress: Progress,
) -> Result<ScriptRunReport, ScriptWorkerError> {
    let mut cancelled = cancel.sender.subscribe();
    let mut report = ScriptRunReport {
        format: "cshell.script-run-report".into(),
        version: WORKER_VERSION,
        run_id: run.start.run_id,
        document_sha256: run.start.document_sha256,
        status: ScriptRunStatus::Cancelled,
        operations: 0,
        notifications: 0,
        exit_confirmed: false,
        containment: None,
        sandbox: None,
        sandbox_cleanup_confirmed: false,
        effects_confirmed: true,
    };
    if *cancelled.borrow() {
        report.exit_confirmed = true;
        report.sandbox_cleanup_confirmed = true;
        return Ok(report);
    }
    let path = worker_path()?;
    let mut slot = crate::recovery::Slot::acquire(run.start.run_id)?;
    let scratch = tempfile::Builder::new()
        .prefix(&format!("run-{}-", run.start.run_id))
        .tempdir_in(slot.root())
        .map_err(|_| ScriptWorkerError::Unavailable)?;
    slot.directory(scratch.path())?;
    // Own quota/cwd before profile creation or staging. Prelaunch failures must
    // also retain quota if profile cleanup cannot be confirmed.
    let mut lease = Lease {
        _permit: permit,
        scratch: Some(scratch),
        proven: true,
        containment: None,
        slot,
        cleanup_result: None,
    };
    lease.containment = Some(ParentContainment::new(
        &run.start.document.limits,
        run.start.run_id,
    )?);
    let directory = lease
        .scratch
        .as_ref()
        .ok_or(ScriptWorkerError::Isolation)?
        .path();
    lease.slot.launching()?;
    lease.proven = false;
    #[cfg(not(windows))]
    let mut child = {
        let mut command = tokio::process::Command::new(path);
        command
            .arg(WORKER_STDIO_ARGUMENT)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .env_clear()
            .env("TZ", "UTC0")
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                lease.proven = true;
                return Err(ScriptWorkerError::Unavailable);
            }
        }
    };
    #[cfg(windows)]
    {
        lease.proven = false;
    }
    #[cfg(windows)]
    let mut child = match lease
        .containment
        .as_mut()
        .ok_or(ScriptWorkerError::Isolation)?
        .spawn(&path, directory)
    {
        Ok(child) => child,
        Err(_) => {
            lease.proven = true;
            report.status = ScriptRunStatus::WorkerFailed;
            report.exit_confirmed = true; // Native launch returned no child.
            progress.send_replace((Phase::Cleaning, report.clone()));
            report.sandbox_cleanup_confirmed = lease.cleanup();
            return Ok(report);
        }
    };
    lease.proven = false;
    if child
        .id()
        .is_none_or(|pid| lease.slot.started(pid).is_err())
    {
        report.status = ScriptRunStatus::WorkerFailed;
        exit(child, lease, &mut report, &progress).await;
        return Ok(report);
    }
    let attached = lease
        .containment
        .as_ref()
        .ok_or(ScriptWorkerError::Isolation)?
        .attach(&child);
    if attached.is_err() {
        report.status = ScriptRunStatus::WorkerFailed;
        exit(child, lease, &mut report, &progress).await;
        return Ok(report);
    }
    let (Some(stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        report.status = ScriptRunStatus::ProtocolFailed;
        exit(child, lease, &mut report, &progress).await;
        return Ok(report);
    };
    let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
    let deadline =
        Instant::now() + Duration::from_millis(u64::from(run.start.document.limits.timeout_ms));
    report.effects_confirmed = run.live.is_none();
    progress.send_replace((Phase::Running, report.clone()));
    let pid = child.id().ok_or(ScriptWorkerError::Isolation)?;
    let status = tokio::select! {
        biased;
        _ = cancellation(&mut cancelled) => ScriptRunStatus::Cancelled,
        _ = tokio::time::sleep_until(deadline) => ScriptRunStatus::TimedOut,
        _ = authority_lost(&run) => ScriptRunStatus::Denied,
        status = memory_watch(pid, run.start.document.limits.memory_bytes) => status,
        result = exchange(&run, &stdin, &mut stdout, &mut report) => result.unwrap_or(ScriptRunStatus::ProtocolFailed),
        _ = heartbeat(&stdin, run.start.run_id) => ScriptRunStatus::ProtocolFailed,
    };
    report.status = status;
    report.effects_confirmed = run
        .live
        .as_ref()
        .is_none_or(|a| a.broker.effects_confirmed());
    if !report.effects_confirmed {
        report.status = ScriptRunStatus::Unknown;
    }
    drop(stdin);
    drop(stdout);
    exit(child, lease, &mut report, &progress).await;
    Ok(report)
}
