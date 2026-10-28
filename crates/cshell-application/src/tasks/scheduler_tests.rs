use super::*;
use std::{
    error::Error,
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Debug)]
struct Clock(AtomicU64);
impl TaskClock for Clock {
    fn now_ms(&self) -> Result<u64, TaskError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
#[derive(Debug)]
struct MemoryRepository {
    record: Mutex<TaskRunRecord>,
    failure: Mutex<Option<(TaskRunEventKind, bool)>>,
    blocked: Option<TaskRunEventKind>,
    entered: Notify,
    release: Notify,
}
impl MemoryRepository {
    fn new(record: TaskRunRecord) -> Self {
        Self {
            record: Mutex::new(record),
            failure: Mutex::new(None),
            blocked: None,
            entered: Notify::new(),
            release: Notify::new(),
        }
    }
    fn record(&self) -> TaskRunRecord {
        self.record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
#[async_trait]
impl TaskRepository for MemoryRepository {
    async fn create_task_run(&self, _: &TaskRunRecord) -> Result<(), TaskRepositoryError> {
        Err(TaskRepositoryError::Conflict)
    }
    async fn load_task_run(
        &self,
        id: TaskRunId,
    ) -> Result<Option<TaskRunRecord>, TaskRepositoryError> {
        let record = self.record();
        Ok((record.id() == id).then_some(record))
    }
    async fn commit_task_patch(&self, patch: &TaskRunPatch) -> Result<u64, TaskRepositoryError> {
        if self.blocked == Some(patch.event().kind) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        let mut failure = self
            .failure
            .lock()
            .map_err(|_| TaskRepositoryError::Corrupt)?;
        let fault = failure.filter(|(kind, _)| *kind == patch.event().kind);
        if fault.is_some() {
            *failure = None;
        }
        if fault.is_some_and(|(_, committed)| !committed) {
            return Err(TaskRepositoryError::Unavailable);
        }
        self.record
            .lock()
            .map_err(|_| TaskRepositoryError::Corrupt)?
            .apply_committed(patch, patch.after().revision)
            .map_err(|_| TaskRepositoryError::Conflict)?;
        if fault.is_some() {
            Err(TaskRepositoryError::Unavailable)
        } else {
            Ok(patch.after().revision)
        }
    }
    async fn list_task_runs(
        &self,
        _: Option<TaskRunId>,
        _: usize,
        _: bool,
    ) -> Result<Vec<TaskRunSummary>, TaskRepositoryError> {
        Ok(vec![])
    }
}
#[derive(Debug)]
struct Probe {
    repo: Arc<MemoryRepository>,
    connects: AtomicUsize,
    executes: AtomicUsize,
    closes: AtomicUsize,
    live: AtomicUsize,
    maximum_live: AtomicUsize,
    entered: Notify,
    connect_hangs: bool,
    execute_hangs: bool,
    exit_code: u32,
    cancel_at_exit: Option<TaskRunControl>,
    clock_at_exit: Option<Arc<Clock>>,
}
#[derive(Debug)]
struct Executor(Arc<Probe>);
#[derive(Debug)]
struct Connection {
    probe: Arc<Probe>,
    host: HostId,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.probe.live.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl TaskExecutor for Executor {
    async fn connect(
        &self,
        _: TaskSnapshot,
        host: HostId,
        _: TaskCancellation,
    ) -> Result<Box<dyn TaskConnection>, TaskStopReason> {
        assert_eq!(
            self.0.repo.record().target(host).map(|t| t.state),
            Some(TargetState::Connecting)
        );
        self.0.connects.fetch_add(1, Ordering::SeqCst);
        if self.0.connect_hangs {
            self.0.entered.notify_one();
            return std::future::pending().await;
        }
        let live = self.0.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.maximum_live.fetch_max(live, Ordering::SeqCst);
        Ok(Box::new(Connection {
            probe: self.0.clone(),
            host,
        }))
    }
}
#[async_trait]
impl TaskConnection for Connection {
    async fn execute(
        &mut self,
        step: usize,
        _: &str,
        _: TaskCancellation,
    ) -> Result<u32, TaskStopReason> {
        // This checks the durable barrier, rather than merely matching compiler output.
        assert_eq!(
            self.probe
                .repo
                .record()
                .target(self.host)
                .map(|t| t.steps[step].state),
            Some(TargetState::Running)
        );
        self.probe.executes.fetch_add(1, Ordering::SeqCst);
        self.probe.entered.notify_one();
        if self.probe.execute_hangs {
            return std::future::pending().await;
        }
        if let Some(control) = &self.probe.cancel_at_exit {
            control.cancel();
        }
        if let Some(clock) = &self.probe.clock_at_exit {
            clock.0.store(2000, Ordering::SeqCst);
        }
        Ok(self.probe.exit_code)
    }
    async fn close(&mut self) {
        self.probe.closes.fetch_add(1, Ordering::SeqCst);
    }
}
fn plan(count: usize, policy: TaskPolicy) -> Result<TaskRunRecord, Box<dyn Error>> {
    let source = super::tests::fixture(count)?;
    let mut definition = source.definition().clone();
    definition.policy = policy;
    Ok(TaskRunRecord::new(TaskSnapshot::new(
        definition,
        source.targets().clone(),
        "operator".into(),
        1000,
    )?)?)
}
fn probe(repo: Arc<MemoryRepository>) -> Probe {
    Probe {
        repo,
        connects: AtomicUsize::new(0),
        executes: AtomicUsize::new(0),
        closes: AtomicUsize::new(0),
        live: AtomicUsize::new(0),
        maximum_live: AtomicUsize::new(0),
        entered: Notify::new(),
        connect_hangs: false,
        execute_hangs: false,
        exit_code: 0,
        cancel_at_exit: None,
        clock_at_exit: None,
    }
}
fn runner(
    repo: Arc<MemoryRepository>,
    probe: Arc<Probe>,
    clock: Arc<Clock>,
    budget: TaskResourceBudget,
) -> TaskScheduler {
    TaskScheduler::new(repo, Arc::new(Executor(probe)), budget, clock)
}
fn budget(global: u16) -> Result<TaskResourceBudget, TaskError> {
    TaskResourceBudget::new(TaskResourceLimits {
        global,
        per_endpoint: 1,
        per_jump: global.min(10),
    })
}
async fn wait_until(
    repo: &MemoryRepository,
    state: TaskState,
) -> Result<TaskRunRecord, Box<dyn Error>> {
    Ok(tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record = repo.record();
            if record.meta().state == state {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?)
}

#[test]
fn resource_limits_share_actual_endpoints_and_release_all_counters_atomically()
-> Result<(), Box<dyn Error>> {
    let first = plan(
        3,
        TaskPolicy {
            concurrency: 1,
            ..Default::default()
        },
    )?;
    let other = plan(1, TaskPolicy::default())?;
    let shared = budget(2)?;
    let first_id = first
        .snapshot()
        .targets()
        .targets()
        .iter()
        .find(|host| host.address == "host-0.test")
        .ok_or("host")?
        .id;
    let held = shared
        .try_acquire(first.snapshot(), first_id)?
        .ok_or("no lease")?;
    assert!(
        shared
            .try_acquire(
                first.snapshot(),
                first
                    .targets()
                    .iter()
                    .find(|target| target.host_id != first_id)
                    .ok_or("other host")?
                    .host_id
            )?
            .is_none()
    );
    // Different Host/Profile/TaskRun IDs still share the same actual endpoint.
    assert!(
        shared
            .try_acquire(other.snapshot(), other.targets()[0].host_id)?
            .is_none()
    );
    drop(held);
    let replacement = shared
        .try_acquire(other.snapshot(), other.targets()[0].host_id)?
        .ok_or("leaked resource")?;
    drop(replacement);
    for target in first.targets() {
        drop(
            shared
                .try_acquire(first.snapshot(), target.host_id)?
                .ok_or("leaked run counter")?,
        );
    }
    assert!(!format!("{shared:?}").contains("host-0.test"));
    Ok(())
}

#[test]
fn jump_budget_is_shared_across_profile_aliases_without_reserving_partial_routes()
-> Result<(), Box<dyn Error>> {
    let source = super::tests::fixture(3)?;
    let mut document: serde_json::Value =
        serde_json::from_slice(&crate::encode_frozen_hosts(source.targets())?)?;
    let jump_id = ProfileId::new();
    let mut jump = document["connections"][0].clone();
    jump["profile_id"] = jump_id.to_string().into();
    jump["host"] = "JUMP.TEST.".into();
    document["connections"][0]["route"] = serde_json::json!({"kind":"jump","profile_id":jump_id});
    document["connections"]
        .as_array_mut()
        .ok_or("connections")?
        .push(jump);
    let frozen = crate::decode_frozen_hosts(&serde_json::to_vec(&document)?)?;
    let first = TaskSnapshot::new(source.definition().clone(), frozen, "operator".into(), 1000)?;
    let mut document: serde_json::Value =
        serde_json::from_slice(&crate::encode_frozen_hosts(first.targets())?)?;
    let alias = ProfileId::new();
    document["connections"][1]["profile_id"] = alias.to_string().into();
    document["connections"][1]["host"] = "jump.test".into();
    document["connections"][0]["route"]["profile_id"] = alias.to_string().into();
    let second = TaskSnapshot::new(
        source.definition().clone(),
        crate::decode_frozen_hosts(&serde_json::to_vec(&document)?)?,
        "operator".into(),
        1000,
    )?;
    let shared = TaskResourceBudget::new(TaskResourceLimits {
        global: 2,
        per_endpoint: 1,
        per_jump: 1,
    })?;
    let held = shared
        .try_acquire(&first, first.targets().targets()[0].id)?
        .ok_or("no lease")?;
    assert!(
        shared
            .try_acquire(&second, second.targets().targets()[1].id)?
            .is_none()
    );
    // The rejected reservation did not consume the other destination/global slot.
    let direct = plan(3, TaskPolicy::default())?;
    let direct_host = direct
        .snapshot()
        .targets()
        .targets()
        .iter()
        .find(|host| host.address != first.targets().targets()[0].address)
        .ok_or("direct host")?
        .id;
    drop(
        shared
            .try_acquire(direct.snapshot(), direct_host)?
            .ok_or("partial reservation")?,
    );
    drop(held);
    drop(
        shared
            .try_acquire(&second, second.targets().targets()[1].id)?
            .ok_or("jump resource leaked")?,
    );
    Ok(())
}

#[tokio::test]
async fn scheduler_runs_1000_model_targets_through_durable_intents_and_bounded_workers()
-> Result<(), Box<dyn Error>> {
    let record = plan(
        1000,
        TaskPolicy {
            concurrency: 50,
            ..Default::default()
        },
    )?;
    let id = record.id();
    let repo = Arc::new(MemoryRepository::new(record));
    let probe = Arc::new(probe(repo.clone()));
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let (_, signals) = task_run_controls(); // Dropping a UI control does not cancel background work.
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        runner(repo.clone(), probe.clone(), clock, budget(50)?).run(id, signals),
    )
    .await??;
    assert_eq!(result.meta().state, TaskState::Completed);
    assert_eq!(probe.executes.load(Ordering::SeqCst), 1000);
    assert!(probe.maximum_live.load(Ordering::SeqCst) <= 50);
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    assert_eq!(repo.record(), result);
    Ok(())
}

#[tokio::test]
async fn cancel_before_dispatch_and_during_connect_commit_never_calls_executor()
-> Result<(), Box<dyn Error>> {
    for blocked in [None, Some(TaskRunEventKind::Connecting)] {
        let record = plan(2, TaskPolicy::default())?;
        let id = record.id();
        let repo = Arc::new(MemoryRepository {
            blocked,
            ..MemoryRepository::new(record)
        });
        let probe = Arc::new(probe(repo.clone()));
        let (control, signals) = task_run_controls();
        let scheduler = runner(
            repo.clone(),
            probe.clone(),
            Arc::new(Clock(AtomicU64::new(1000))),
            budget(2)?,
        );
        if blocked.is_none() {
            control.cancel();
        }
        let job = tokio::spawn(async move { scheduler.run(id, signals).await });
        if blocked.is_some() {
            tokio::time::timeout(Duration::from_secs(2), repo.entered.notified()).await?;
            control.cancel();
            repo.release.notify_one();
        }
        let result = tokio::time::timeout(Duration::from_secs(3), job).await???;
        assert_eq!(result.meta().state, TaskState::Cancelled);
        assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn cancel_during_intent_commit_prevents_exec_and_preserves_uncertainty()
-> Result<(), Box<dyn Error>> {
    let record = plan(1, TaskPolicy::default())?;
    let id = record.id();
    let repo = Arc::new(MemoryRepository {
        blocked: Some(TaskRunEventKind::StepIntent),
        ..MemoryRepository::new(record)
    });
    let probe = Arc::new(probe(repo.clone()));
    let (control, signals) = task_run_controls();
    let scheduler = runner(
        repo.clone(),
        probe.clone(),
        Arc::new(Clock(AtomicU64::new(1000))),
        budget(1)?,
    );
    let job = tokio::spawn(async move { scheduler.run(id, signals).await });
    tokio::time::timeout(Duration::from_secs(2), repo.entered.notified()).await?;
    control.cancel();
    repo.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(3), job).await???;
    assert_eq!(probe.executes.load(Ordering::SeqCst), 0);
    assert_eq!(result.targets()[0].state, TargetState::Unknown);
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn lost_intent_ack_stops_dispatch_and_recovery_does_not_replay_exec()
-> Result<(), Box<dyn Error>> {
    let record = plan(1, TaskPolicy::default())?;
    let id = record.id();
    let repo = Arc::new(MemoryRepository::new(record));
    *repo.failure.lock().map_err(|_| "mutex")? = Some((TaskRunEventKind::StepIntent, true));
    let probe = Arc::new(probe(repo.clone()));
    let scheduler = runner(
        repo.clone(),
        probe.clone(),
        Arc::new(Clock(AtomicU64::new(1000))),
        budget(1)?,
    );
    let (_, signals) = task_run_controls();
    assert_eq!(
        scheduler.run(id, signals).await,
        Err(TaskSchedulerError::Storage(
            TaskRepositoryError::Unavailable
        ))
    );
    assert_eq!(probe.executes.load(Ordering::SeqCst), 0);
    assert_eq!(repo.record().targets()[0].state, TargetState::Running);
    let (_, signals) = task_run_controls();
    let recovered = scheduler.run(id, signals).await?;
    assert_eq!(recovered.targets()[0].state, TargetState::Unknown);
    assert_eq!(recovered.meta().state, TaskState::PartialFailed);
    assert_eq!(probe.executes.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn connect_and_step_timeout_have_distinct_certainty_and_release_resources()
-> Result<(), Box<dyn Error>> {
    for connect in [true, false] {
        let record = plan(
            1,
            TaskPolicy {
                connect_timeout_ms: 30,
                step_timeout_ms: 30,
                ..Default::default()
            },
        )?;
        let id = record.id();
        let repo = Arc::new(MemoryRepository::new(record));
        let probe = Arc::new(Probe {
            connect_hangs: connect,
            execute_hangs: !connect,
            ..probe(repo.clone())
        });
        let (_, signals) = task_run_controls();
        let shared = budget(1)?;
        let scheduler = runner(
            repo,
            probe.clone(),
            Arc::new(Clock(AtomicU64::new(1000))),
            shared.clone(),
        );
        let result =
            tokio::time::timeout(Duration::from_secs(3), scheduler.run(id, signals)).await??;
        assert_eq!(
            result.targets()[0].state,
            if connect {
                TargetState::TimedOut
            } else {
                TargetState::Unknown
            }
        );
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
        drop(
            shared
                .try_acquire(result.snapshot(), result.targets()[0].host_id)?
                .ok_or("leaked reservation")?,
        );
    }
    Ok(())
}

#[tokio::test]
async fn queue_timeout_while_another_run_holds_the_endpoint_never_connects()
-> Result<(), Box<dyn Error>> {
    let record = plan(
        1,
        TaskPolicy {
            queue_timeout_ms: 100,
            ..Default::default()
        },
    )?;
    let id = record.id();
    let shared = budget(2)?;
    let other = plan(1, TaskPolicy::default())?;
    let _held = shared
        .try_acquire(other.snapshot(), other.targets()[0].host_id)?
        .ok_or("no lease")?;
    let repo = Arc::new(MemoryRepository::new(record));
    let probe = Arc::new(probe(repo.clone()));
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let scheduler = runner(repo.clone(), probe.clone(), clock.clone(), shared);
    let (_, signals) = task_run_controls();
    let job = tokio::spawn(async move { scheduler.run(id, signals).await });
    wait_until(&repo, TaskState::Running).await?;
    clock.0.store(1100, Ordering::SeqCst);
    let result = tokio::time::timeout(Duration::from_secs(3), job).await???;
    assert_eq!(result.targets()[0].state, TargetState::TimedOut);
    assert_eq!(
        result.targets()[0].reason,
        Some(TaskStopReason::QueueTimeout)
    );
    assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn canary_gate_and_persisted_batch_delay_block_new_connections_until_approved_and_due()
-> Result<(), Box<dyn Error>> {
    let record = plan(
        2,
        TaskPolicy {
            canary_count: 1,
            approve_after_canary: true,
            batch_delay_ms: 100,
            ..Default::default()
        },
    )?;
    let id = record.id();
    let repo = Arc::new(MemoryRepository::new(record));
    let probe = Arc::new(probe(repo.clone()));
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let scheduler = runner(repo.clone(), probe.clone(), clock.clone(), budget(2)?);
    let (control, signals) = task_run_controls();
    let job = tokio::spawn(async move { scheduler.run(id, signals).await });
    let paused = wait_until(&repo, TaskState::Paused).await?;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    assert_eq!(paused.meta().batch_not_before_ms, 1100);
    assert_eq!(
        control.approve_gate(TaskGateId::new()).await,
        Err(TaskSchedulerError::Task(TaskError::Transition))
    );
    control
        .approve_gate(paused.meta().gate.as_ref().ok_or("gate")?.id)
        .await?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    clock.0.store(1100, Ordering::SeqCst);
    let result = tokio::time::timeout(Duration::from_secs(3), job).await???;
    assert_eq!(result.meta().state, TaskState::Completed);
    assert_eq!(probe.executes.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn total_deadline_also_stops_a_paused_gate_without_starting_queued_targets()
-> Result<(), Box<dyn Error>> {
    let record = plan(
        2,
        TaskPolicy {
            canary_count: 1,
            approve_after_canary: true,
            total_timeout_ms: 100,
            ..Default::default()
        },
    )?;
    let id = record.id();
    let repo = Arc::new(MemoryRepository::new(record));
    let probe = Arc::new(probe(repo.clone()));
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let scheduler = runner(repo.clone(), probe.clone(), clock.clone(), budget(2)?);
    let (_, signals) = task_run_controls();
    let job = tokio::spawn(async move { scheduler.run(id, signals).await });
    wait_until(&repo, TaskState::Paused).await?;
    clock.0.store(1100, Ordering::SeqCst);
    let result = tokio::time::timeout(Duration::from_secs(3), job).await???;
    assert_eq!(
        result.meta().stop_reason,
        Some(TaskStopReason::TotalTimeout)
    );
    assert_eq!(result.targets()[1].state, TargetState::TimedOut);
    assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn known_exit_racing_cancel_is_preserved_and_failure_threshold_stops_new_dispatch()
-> Result<(), Box<dyn Error>> {
    for racing in [true, false] {
        let record = plan(
            3,
            TaskPolicy {
                concurrency: 1,
                failure_limit: Some(1),
                ..Default::default()
            },
        )?;
        let id = record.id();
        let repo = Arc::new(MemoryRepository::new(record));
        let (control, signals) = task_run_controls();
        let probe = Arc::new(Probe {
            exit_code: 7,
            cancel_at_exit: racing.then_some(control),
            ..probe(repo.clone())
        });
        let scheduler = runner(
            repo,
            probe.clone(),
            Arc::new(Clock(AtomicU64::new(1000))),
            budget(3)?,
        );
        let result =
            tokio::time::timeout(Duration::from_secs(3), scheduler.run(id, signals)).await??;
        assert_eq!(result.targets()[0].steps[0].exit_code, Some(7));
        assert_eq!(result.targets()[0].state, TargetState::Failed);
        assert!(
            result.targets()[1..]
                .iter()
                .all(|t| t.state == TargetState::Cancelled)
        );
        assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_runner_is_rejected_without_recovering_the_live_owners_intent()
-> Result<(), Box<dyn Error>> {
    let record = plan(1, TaskPolicy::default())?;
    let id = record.id();
    let repo = Arc::new(MemoryRepository::new(record));
    let probe = Arc::new(Probe {
        execute_hangs: true,
        ..probe(repo.clone())
    });
    let shared = budget(1)?;
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let scheduler = Arc::new(runner(repo.clone(), probe.clone(), clock, shared.clone()));
    let (control, signals) = task_run_controls();
    let first = scheduler.clone();
    let job = tokio::spawn(async move { first.run(id, signals).await });
    tokio::time::timeout(Duration::from_secs(2), probe.entered.notified()).await?;
    let (_, signals) = task_run_controls();
    assert_eq!(
        scheduler.run(id, signals).await,
        Err(TaskSchedulerError::Task(TaskError::Transition))
    );
    assert_eq!(repo.record().targets()[0].state, TargetState::Running);
    control.cancel();
    let result = tokio::time::timeout(Duration::from_secs(3), job).await???;
    assert_eq!(result.targets()[0].state, TargetState::Unknown);
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    drop(
        shared
            .try_acquire(result.snapshot(), result.targets()[0].host_id)?
            .ok_or("leaked resource")?,
    );
    Ok(())
}

#[tokio::test]
async fn late_known_exit_is_kept_while_the_next_step_is_stopped_by_its_deadline()
-> Result<(), Box<dyn Error>> {
    let original = plan(
        1,
        TaskPolicy {
            step_timeout_ms: 100,
            ..Default::default()
        },
    )?;
    let mut definition = original.snapshot().definition().clone();
    let mut next = definition.steps[0].clone();
    next.name = "next".into();
    definition.steps.push(next);
    let record = TaskRunRecord::new(TaskSnapshot::new(
        definition,
        original.snapshot().targets().clone(),
        "operator".into(),
        1000,
    )?)?;
    let id = record.id();
    let repo = Arc::new(MemoryRepository::new(record));
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let probe = Arc::new(Probe {
        clock_at_exit: Some(clock.clone()),
        ..probe(repo.clone())
    });
    let scheduler = runner(repo, probe.clone(), clock, budget(1)?);
    let (_, signals) = task_run_controls();
    let result = tokio::time::timeout(Duration::from_secs(3), scheduler.run(id, signals)).await??;
    assert_eq!(probe.executes.load(Ordering::SeqCst), 1);
    assert_eq!(result.targets()[0].steps[0].exit_code, Some(0));
    assert_eq!(result.targets()[0].steps[0].state, TargetState::Succeeded);
    assert_eq!(result.targets()[0].steps[1].state, TargetState::Cancelled);
    assert_eq!(result.targets()[0].state, TargetState::TimedOut);
    Ok(())
}
