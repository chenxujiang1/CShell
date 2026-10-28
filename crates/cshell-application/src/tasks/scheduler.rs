use super::budget::ResourceRequest;
use super::*;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STORAGE_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum TaskSchedulerError {
    #[error(transparent)]
    Task(#[from] TaskError),
    #[error(transparent)]
    Storage(#[from] TaskRepositoryError),
    #[error("task control queue is full")]
    Busy,
    #[error("task runner has stopped")]
    Stopped,
    #[error("task executor worker failed")]
    Worker,
}
pub trait TaskClock: fmt::Debug + Send + Sync {
    fn now_ms(&self) -> Result<u64, TaskError>;
}
/// A wall-time anchor plus monotonic elapsed time, so clock corrections cannot
/// move a live run backwards. A restart still checks persisted timestamps.
#[derive(Debug)]
pub struct SystemTaskClock {
    anchor_ms: u64,
    started: Instant,
}
impl SystemTaskClock {
    pub fn new() -> Result<Self, TaskError> {
        let anchor_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TaskError::Clock)?
            .as_millis();
        Ok(Self {
            anchor_ms: u64::try_from(anchor_ms).map_err(|_| TaskError::Clock)?,
            started: Instant::now(),
        })
    }
}
impl TaskClock for SystemTaskClock {
    fn now_ms(&self) -> Result<u64, TaskError> {
        let elapsed =
            u64::try_from(self.started.elapsed().as_millis()).map_err(|_| TaskError::Clock)?;
        self.anchor_ms
            .checked_add(elapsed)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or(TaskError::Clock)
    }
}
#[derive(Clone)]
pub struct TaskCancellation {
    local: watch::Sender<Option<TaskStopReason>>,
    parent: Option<watch::Sender<Option<TaskStopReason>>>,
}
impl fmt::Debug for TaskCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskCancellation")
            .field("requested", &self.reason().is_some())
            .finish()
    }
}
impl TaskCancellation {
    #[must_use]
    pub fn detached() -> Self {
        Self::new()
    }
    pub fn cancel(&self, reason: TaskStopReason) {
        self.request(reason);
    }
    fn new() -> Self {
        Self {
            local: watch::channel(None).0,
            parent: None,
        }
    }
    fn child(parent: &Self) -> Self {
        Self {
            local: watch::channel(None).0,
            parent: Some(parent.local.clone()),
        }
    }
    fn request(&self, reason: TaskStopReason) {
        self.local.send_if_modified(|value| {
            if value.is_none() {
                *value = Some(reason);
                true
            } else {
                false
            }
        });
    }
    #[must_use]
    pub fn reason(&self) -> Option<TaskStopReason> {
        self.parent
            .as_ref()
            .and_then(|parent| *parent.borrow())
            .or(*self.local.borrow())
    }
    pub async fn cancelled(&self) -> TaskStopReason {
        let mut receiver = self.local.subscribe();
        let mut parent = self.parent.as_ref().map(watch::Sender::subscribe);
        loop {
            if let Some(reason) = self.reason() {
                return reason;
            }
            tokio::select! {
                result = receiver.changed() => if result.is_err() { return TaskStopReason::TransportLost; },
                _ = async { match parent.as_mut() { Some(parent) => { let _ = parent.changed().await; }, None => std::future::pending::<()>().await } } => {},
            }
        }
    }
}
/// Implementations own task channels, perform host-key/credential checks, and
/// close/drop their channels when a future is cancelled. No implicit retries.
#[async_trait]
pub trait TaskExecutor: fmt::Debug + Send + Sync {
    async fn connect(
        &self,
        snapshot: TaskSnapshot,
        host: HostId,
        cancel: TaskCancellation,
    ) -> Result<Box<dyn TaskConnection>, TaskStopReason>;
}
#[async_trait]
pub trait TaskConnection: fmt::Debug + Send {
    /// Return an exit code only after stdout/stderr have been fully processed.
    /// Errors must distinguish definitive exec rejection from uncertain execution.
    async fn execute(
        &mut self,
        step: usize,
        command: &str,
        cancel: TaskCancellation,
    ) -> Result<u32, TaskStopReason>;
    /// Close only task-owned channels; never a connection shared by other features.
    async fn close(&mut self);
}
struct GateRequest {
    id: TaskGateId,
    reply: oneshot::Sender<Result<(), TaskSchedulerError>>,
}
#[derive(Clone)]
pub struct TaskRunControl {
    cancel: TaskCancellation,
    approvals: mpsc::Sender<GateRequest>,
}
impl fmt::Debug for TaskRunControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskRunControl").finish_non_exhaustive()
    }
}
impl TaskRunControl {
    pub(super) fn cancel_for(&self, reason: TaskStopReason) {
        self.cancel.request(reason);
    }
    /// Effective immediately for future dispatch, even while storage is awaiting an acknowledgement.
    pub fn cancel(&self) {
        self.cancel.request(TaskStopReason::UserCancelled);
    }
    /// Success acknowledges persistence, not merely acceptance by the control queue.
    pub async fn approve_gate(&self, id: TaskGateId) -> Result<(), TaskSchedulerError> {
        let (reply, result) = oneshot::channel();
        self.approvals
            .try_send(GateRequest { id, reply })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => TaskSchedulerError::Busy,
                mpsc::error::TrySendError::Closed(_) => TaskSchedulerError::Stopped,
            })?;
        result.await.map_err(|_| TaskSchedulerError::Stopped)?
    }
}
pub struct TaskRunSignals {
    cancel: TaskCancellation,
    approvals: mpsc::Receiver<GateRequest>,
}
impl fmt::Debug for TaskRunSignals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskRunSignals").finish_non_exhaustive()
    }
}
#[must_use]
pub fn task_run_controls() -> (TaskRunControl, TaskRunSignals) {
    let cancel = TaskCancellation::new();
    let (tx, approvals) = mpsc::channel(16);
    (
        TaskRunControl {
            cancel: cancel.clone(),
            approvals: tx,
        },
        TaskRunSignals { cancel, approvals },
    )
}
#[derive(Debug)]
pub struct TaskScheduler {
    repository: Arc<dyn TaskRepository>,
    executor: Arc<dyn TaskExecutor>,
    budget: TaskResourceBudget,
    clock: Arc<dyn TaskClock>,
}
struct Grant {
    step: usize,
    command: String,
}
struct Ready {
    host: HostId,
    exited: Option<(usize, u32)>,
    reply: oneshot::Sender<Option<Grant>>,
}
struct WorkerDone {
    host: HostId,
    reason: Option<TaskStopReason>,
}
struct Workers {
    jobs: JoinSet<WorkerDone>,
    active: BTreeMap<HostId, TaskCancellation>,
}
impl Workers {
    fn new() -> Self {
        Self {
            jobs: JoinSet::new(),
            active: BTreeMap::new(),
        }
    }
    fn cancel_all(&self, reason: TaskStopReason) {
        for cancel in self.active.values() {
            cancel.request(reason);
        }
    }
    async fn shutdown(&mut self) {
        self.cancel_all(TaskStopReason::TransportLost);
        if tokio::time::timeout(CLOSE_TIMEOUT + Duration::from_secs(1), async {
            while self.jobs.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            self.jobs.abort_all();
            while self.jobs.join_next().await.is_some() {}
        }
    }
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.cancel_all(TaskStopReason::TransportLost);
        self.jobs.abort_all();
    }
}
impl TaskScheduler {
    #[must_use]
    pub fn new(
        repository: Arc<dyn TaskRepository>,
        executor: Arc<dyn TaskExecutor>,
        budget: TaskResourceBudget,
        clock: Arc<dyn TaskClock>,
    ) -> Self {
        Self {
            repository,
            executor,
            budget,
            clock,
        }
    }
    async fn commit(
        &self,
        record: &mut TaskRunRecord,
        action: TaskRunAction,
    ) -> Result<(), TaskSchedulerError> {
        let patch = record.prepare(action, self.clock.now_ms()?)?;
        let revision =
            tokio::time::timeout(STORAGE_TIMEOUT, self.repository.commit_task_patch(&patch))
                .await
                .map_err(|_| TaskRepositoryError::Unavailable)??;
        record.apply_committed(&patch, revision)?;
        Ok(())
    }
    /// Run an already persisted task. Never creates a record or retries a mutation.
    pub async fn run(
        &self,
        id: TaskRunId,
        signals: TaskRunSignals,
    ) -> Result<TaskRunRecord, TaskSchedulerError> {
        let _owner = self.budget.claim_run(id)?;
        let mut record = tokio::time::timeout(STORAGE_TIMEOUT, self.repository.load_task_run(id))
            .await
            .map_err(|_| TaskRepositoryError::Unavailable)??
            .ok_or(TaskRepositoryError::Missing)?;
        if task_terminal(record.meta().state) {
            return Ok(record);
        }
        if record.meta().state == TaskState::Running
            || record
                .targets()
                .iter()
                .any(|t| matches!(t.state, TargetState::Connecting | TargetState::Running))
        {
            self.commit(&mut record, TaskRunAction::Recover).await?;
        }
        let mut workers = Workers::new();
        let result = self.drive(&mut record, signals, &mut workers).await;
        workers.shutdown().await;
        result?;
        Ok(record)
    }
    async fn drive(
        &self,
        record: &mut TaskRunRecord,
        mut signals: TaskRunSignals,
        workers: &mut Workers,
    ) -> Result<(), TaskSchedulerError> {
        let requests: BTreeMap<_, _> = record
            .targets()
            .iter()
            .map(|t| {
                Ok((
                    t.host_id,
                    ResourceRequest::new(record.snapshot(), t.host_id)?,
                ))
            })
            .collect::<Result<_, TaskError>>()?;
        let (ready_tx, mut ready_rx) = mpsc::channel(100);
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut controls_open = true;
        loop {
            self.reconcile(record, &signals.cancel, workers).await?;
            if task_terminal(record.meta().state) && workers.jobs.is_empty() {
                return Ok(());
            }
            let mut reconcile_again = false;
            if record.meta().state == TaskState::Running && signals.cancel.reason().is_none() {
                let now = self.clock.now_ms()?;
                let eligible = batch_ready_at(record);
                if now >= eligible {
                    let batch = record
                        .snapshot()
                        .batches()
                        .get(record.meta().next_batch)
                        .cloned()
                        .unwrap_or_default();
                    for host in batch {
                        if signals.cancel.reason().is_some() {
                            break;
                        }
                        if record
                            .target(host)
                            .is_none_or(|t| t.state != TargetState::Pending)
                        {
                            continue;
                        }
                        if self.total_expired(record)? {
                            reconcile_again = true;
                            break;
                        }
                        if self.clock.now_ms()?.saturating_sub(eligible)
                            >= record.snapshot().definition().policy.queue_timeout_ms
                        {
                            self.commit(
                                record,
                                TaskRunAction::AbortTarget {
                                    host,
                                    reason: TaskStopReason::QueueTimeout,
                                },
                            )
                            .await?;
                            reconcile_again = true;
                            break;
                        }
                        let Some(lease) = self.budget.acquire(
                            requests.get(&host).ok_or(TaskError::UnknownTarget)?.clone(),
                        )?
                        else {
                            continue;
                        };
                        self.commit(record, TaskRunAction::Connect { host }).await?;
                        // A cancel can arrive during the commit. Do not connect afterwards.
                        let now = self.clock.now_ms()?;
                        let connect_expired = record
                            .target(host)
                            .and_then(|t| t.started_at_ms)
                            .is_some_and(|started| {
                                now.saturating_sub(started)
                                    >= record.snapshot().definition().policy.connect_timeout_ms
                            });
                        if signals.cancel.reason().is_some()
                            || self.total_expired(record)?
                            || connect_expired
                        {
                            drop(lease);
                            let reason = if signals.cancel.reason().is_some() {
                                TaskStopReason::UserCancelled
                            } else if self.total_expired(record)? {
                                TaskStopReason::TotalTimeout
                            } else {
                                TaskStopReason::ConnectionTimeout
                            };
                            self.commit(record, TaskRunAction::AbortTarget { host, reason })
                                .await?;
                            reconcile_again = true;
                            break;
                        }
                        let cancel = TaskCancellation::child(&signals.cancel);
                        workers.active.insert(host, cancel.clone());
                        workers.jobs.spawn(worker(
                            Arc::clone(&self.executor),
                            record.snapshot().clone(),
                            host,
                            cancel,
                            ready_tx.clone(),
                            lease,
                        ));
                    }
                }
            }
            if reconcile_again {
                continue;
            }
            tokio::select! {
                biased;
                _ = signals.cancel.cancelled(), if record.meta().state != TaskState::Cancelling && !task_terminal(record.meta().state) => {},
                ready = ready_rx.recv(), if !workers.jobs.is_empty() => {
                    let ready = ready.ok_or(TaskSchedulerError::Worker)?;
                    if let Some((step, exit_code)) = ready.exited {
                        let now = self.clock.now_ms()?;
                        let overdue = record.target(ready.host).and_then(|target| target.steps[step].started_at_ms).is_some_and(|started| now.saturating_sub(started) >= record.snapshot().definition().policy.step_timeout_ms);
                        self.commit(record, TaskRunAction::StepExited { host: ready.host, step, exit_code }).await?;
                        if overdue && record.target(ready.host).is_some_and(|target| !target_terminal(target.state)) {
                            self.commit(record, TaskRunAction::AbortTarget { host: ready.host, reason: TaskStopReason::StepTimeout }).await?;
                        }
                    }
                    self.reconcile(record, &signals.cancel, workers).await?;
                    let grant = if record.meta().state == TaskState::Running && signals.cancel.reason().is_none()
                        && workers.active.get(&ready.host).is_some_and(|cancel| cancel.reason().is_none()) {
                        let target = record.target(ready.host).ok_or(TaskError::UnknownTarget)?;
                        if !target_terminal(target.state) {
                            let step = target.steps.iter().position(|s| s.state == TargetState::Pending).ok_or(TaskError::Transition)?;
                            self.commit(record, TaskRunAction::StepIntent { host: ready.host, step }).await?;
                            let step_expired = self.clock.now_ms()?.saturating_sub(record.target(ready.host).and_then(|t| t.steps[step].started_at_ms).ok_or(TaskError::Invalid)?) >= record.snapshot().definition().policy.step_timeout_ms;
                            if step_expired && let Some(cancel) = workers.active.get(&ready.host) { cancel.request(TaskStopReason::StepTimeout); }
                            if signals.cancel.reason().is_none() && !self.total_expired(record)? && !step_expired {
                                Some(Grant { step, command: record.snapshot().commands()[step].clone() })
                            } else { None }
                        } else { None }
                    } else { None };
                    let _ = ready.reply.send(grant);
                }
                done = workers.jobs.join_next(), if !workers.jobs.is_empty() => {
                    let done = done.ok_or(TaskSchedulerError::Worker)?.map_err(|_| TaskSchedulerError::Worker)?;
                    workers.active.remove(&done.host);
                    if record.target(done.host).is_some_and(|t| !target_terminal(t.state)) {
                        self.commit(record, TaskRunAction::AbortTarget { host: done.host, reason: done.reason.or(record.meta().stop_reason).unwrap_or(TaskStopReason::TransportLost) }).await?;
                    }
                }
                request = signals.approvals.recv(), if controls_open => {
                    if let Some(request) = request {
                        let result = if signals.cancel.reason().is_some() || self.total_expired(record)? { Err(TaskError::Transition.into()) }
                            else { self.commit(record, TaskRunAction::ApproveGate(request.id)).await };
                        let fatal = matches!(result, Err(TaskSchedulerError::Storage(_)));
                        let _ = request.reply.send(result);
                        if fatal { return result; }
                    } else { controls_open = false; }
                }
                _ = self.budget.changed() => {},
                _ = tick.tick() => {},
            }
        }
    }
    fn total_expired(&self, record: &TaskRunRecord) -> Result<bool, TaskError> {
        Ok(self
            .clock
            .now_ms()?
            .saturating_sub(record.meta().created_at_ms)
            >= record.snapshot().definition().policy.total_timeout_ms)
    }
    async fn reconcile(
        &self,
        record: &mut TaskRunRecord,
        cancel: &TaskCancellation,
        workers: &Workers,
    ) -> Result<(), TaskSchedulerError> {
        if task_terminal(record.meta().state) {
            return Ok(());
        }
        if record.targets().iter().all(|t| target_terminal(t.state)) {
            return self.commit(record, TaskRunAction::Finish).await;
        }
        let policy = record.snapshot().definition().policy.clone();
        let now = self.clock.now_ms()?;
        for target in record.targets() {
            let reason = if target.state == TargetState::Connecting
                && target
                    .started_at_ms
                    .is_some_and(|start| now.saturating_sub(start) >= policy.connect_timeout_ms)
            {
                Some(TaskStopReason::ConnectionTimeout)
            } else if target.steps.iter().any(|step| {
                step.state == TargetState::Running
                    && step
                        .started_at_ms
                        .is_some_and(|start| now.saturating_sub(start) >= policy.step_timeout_ms)
            }) {
                Some(TaskStopReason::StepTimeout)
            } else {
                None
            };
            if let Some(reason) = reason
                && let Some(cancel) = workers.active.get(&target.host_id)
            {
                cancel.request(reason);
            }
        }
        if record.meta().state != TaskState::Cancelling {
            let reason = if cancel.reason().is_some() {
                Some(TaskStopReason::UserCancelled)
            } else if self.total_expired(record)? {
                Some(TaskStopReason::TotalTimeout)
            } else if policy.failure_limit.is_some_and(|limit| {
                record
                    .targets()
                    .iter()
                    .filter(|t| {
                        matches!(
                            t.state,
                            TargetState::Failed | TargetState::TimedOut | TargetState::Unknown
                        )
                    })
                    .count()
                    >= usize::from(limit)
            }) {
                Some(TaskStopReason::FailureThreshold)
            } else {
                None
            };
            if let Some(reason) = reason {
                self.commit(record, TaskRunAction::Stop(reason)).await?;
            }
        }
        if record.meta().state == TaskState::Cancelling {
            workers.cancel_all(
                record
                    .meta()
                    .stop_reason
                    .unwrap_or(TaskStopReason::UserCancelled),
            );
            return Ok(());
        }
        if record.meta().state == TaskState::Queued {
            self.commit(record, TaskRunAction::Start).await?;
        }
        if record.meta().state == TaskState::Running {
            let batches = record.snapshot().batches();
            let batch = batches
                .get(record.meta().next_batch)
                .ok_or(TaskError::Transition)?;
            let now = self.clock.now_ms()?;
            let ready_at = batch_ready_at(record);
            for host in batch {
                if now >= ready_at
                    && now.saturating_sub(ready_at) >= policy.queue_timeout_ms
                    && record
                        .target(*host)
                        .is_some_and(|t| t.state == TargetState::Pending)
                {
                    self.commit(
                        record,
                        TaskRunAction::AbortTarget {
                            host: *host,
                            reason: TaskStopReason::QueueTimeout,
                        },
                    )
                    .await?;
                }
            }
            if policy.failure_limit.is_some_and(|limit| {
                record
                    .targets()
                    .iter()
                    .filter(|t| {
                        matches!(
                            t.state,
                            TargetState::Failed | TargetState::TimedOut | TargetState::Unknown
                        )
                    })
                    .count()
                    >= usize::from(limit)
            }) {
                self.commit(
                    record,
                    TaskRunAction::Stop(TaskStopReason::FailureThreshold),
                )
                .await?;
                workers.cancel_all(TaskStopReason::FailureThreshold);
                return Ok(());
            }
            if batch.iter().all(|host| {
                record
                    .target(*host)
                    .is_some_and(|t| target_terminal(t.state))
            }) {
                self.commit(record, TaskRunAction::AdvanceBatch).await?;
            }
        }
        Ok(())
    }
}
fn batch_ready_at(record: &TaskRunRecord) -> u64 {
    if record.meta().batch_not_before_ms == 0 && record.meta().next_batch > 0 {
        // Older foundation records did not persist this field; conservatively
        // wait a full delay after their latest durable update.
        record
            .meta()
            .updated_at_ms
            .saturating_add(record.snapshot().definition().policy.batch_delay_ms)
    } else {
        record
            .meta()
            .batch_not_before_ms
            .max(record.meta().created_at_ms)
    }
}
async fn worker(
    executor: Arc<dyn TaskExecutor>,
    snapshot: TaskSnapshot,
    host: HostId,
    cancel: TaskCancellation,
    ready: mpsc::Sender<Ready>,
    _lease: TaskResourceLease,
) -> WorkerDone {
    let policy = &snapshot.definition().policy;
    let connection = tokio::select! {
        biased;
        reason = cancel.cancelled() => Err(reason),
        result = tokio::time::timeout(Duration::from_millis(policy.connect_timeout_ms), executor.connect(snapshot.clone(), host, cancel.clone())) => match result {
            Err(_) => Err(TaskStopReason::ConnectionTimeout),
            Ok(Err(_)) => Err(TaskStopReason::ConnectionFailure),
            Ok(Ok(connection)) => Ok(connection),
        }
    };
    let mut connection = match connection {
        Ok(connection) => connection,
        Err(reason) => {
            return WorkerDone {
                host,
                reason: Some(reason),
            };
        }
    };
    let mut exited = None;
    let reason = loop {
        let (reply, response) = oneshot::channel();
        // Once an exit is known, publish it even if cancellation raced with it.
        // The bounded queue holds at least one event for every active worker.
        let sent = if exited.is_some() {
            ready
                .send(Ready {
                    host,
                    exited,
                    reply,
                })
                .await
        } else {
            tokio::select! {
                biased;
                reason = cancel.cancelled() => break Some(reason),
                result = ready.send(Ready { host, exited, reply }) => result,
            }
        };
        if sent.is_err() {
            break Some(TaskStopReason::TransportLost);
        }
        let grant = tokio::select! {
            biased;
            reason = cancel.cancelled() => break Some(reason),
            result = response => match result { Ok(Some(grant)) => grant, Ok(None) => break cancel.reason(), Err(_) => break Some(TaskStopReason::TransportLost) },
        };
        let result = tokio::select! {
            biased;
            reason = cancel.cancelled() => Err(reason),
            result = tokio::time::timeout(Duration::from_millis(policy.step_timeout_ms), connection.execute(grant.step, &grant.command, cancel.clone())) => match result {
                Err(_) => Err(TaskStopReason::StepTimeout),
                Ok(Ok(code)) => Ok(code),
                Ok(Err(reason @ (TaskStopReason::ExecRejected | TaskStopReason::OutputFailure | TaskStopReason::TransportLost))) => Err(reason),
                Ok(Err(_)) => Err(TaskStopReason::TransportLost),
            }
        };
        match result {
            Ok(code) => exited = Some((grant.step, code)),
            Err(reason) => break Some(reason),
        }
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, connection.close()).await;
    WorkerDone { host, reason }
}
