mod support;

use cshell_application::*;
use cshell_domain::*;
use cshell_storage::SqliteProfileRepository;
use sqlx::{query, query_scalar, sqlite::SqlitePoolOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn fixture() -> Result<TaskRunRecord, Box<dyn Error>> {
    let id = ProfileId::new();
    let profiles = CatalogSnapshot {
        revision: 1,
        profiles: vec![ProfileRecord {
            id,
            name: "template".into(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::new(),
            favorite: false,
            terminal: TerminalOverrides::default(),
        }],
        ssh_connections: vec![SshConnectionRecord {
            options: Default::default(),
            profile_id: id,
            host: "template.test".into(),
            port: 22,
            username: "operator".into(),
            auth_method: SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: SshAgentBackend::OpenSsh,
            agent_identity: None,
            route: SshRoute::Direct,
        }],
        ..Default::default()
    };
    let hosts = HostCatalogSnapshot {
        revision: 1,
        catalog: HostCatalog {
            hosts: (0..3)
                .map(|i| HostRecord {
                    id: HostId::new(),
                    name: format!("host-{i}"),
                    address: format!("host-{i}.test"),
                    port: 22,
                    profile_id: id,
                    tags: BTreeSet::new(),
                    labels: BTreeMap::new(),
                    platform_hint: HostPlatformHint::Unknown,
                })
                .collect(),
            ..Default::default()
        },
    };
    let frozen = freeze_hosts(
        &hosts,
        &profiles,
        HostSelection::Selector {
            expression: "all".into(),
        },
        1000,
    )?;
    let definition = TaskDefinition {
        id: TaskId::new(),
        name: "check".into(),
        shell: RemoteShellProfile::Posix,
        steps: (0..2)
            .map(|i| TaskStep {
                name: format!("step-{i}"),
                command: TaskCommand::Script {
                    text: "printf PRIVATE_BODY".into(),
                },
                success_codes: BTreeSet::from([0]),
                idempotent: true,
            })
            .collect(),
        variables: BTreeMap::new(),
        environment: BTreeMap::new(),
        policy: TaskPolicy::default(),
    };
    Ok(TaskRunRecord::new(TaskSnapshot::new(
        definition,
        frozen,
        "operator".into(),
        1000,
    )?)?)
}
async fn apply(
    repo: &SqliteProfileRepository,
    record: &mut TaskRunRecord,
    action: TaskRunAction,
    now: u64,
) -> Result<(), Box<dyn Error>> {
    let patch = record.prepare(action, now)?;
    let revision = repo.commit_task_patch(&patch).await?;
    record.apply_committed(&patch, revision)?;
    Ok(())
}

#[tokio::test]
async fn task_cas_event_failure_rolls_back_target_meta_and_event_and_reopens_for_recovery()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("tasks.sqlite");
    let first = SqliteProfileRepository::open(&path).await?;
    let second = SqliteProfileRepository::open(&path).await?;
    let mut record = fixture()?;
    let id = record.id();
    let host = record.targets()[0].host_id;
    first.create_task_run(&record).await?;
    assert_eq!(
        first.create_task_run(&record).await,
        Err(TaskRepositoryError::Conflict)
    );
    let stale = record.prepare(TaskRunAction::Start, 1001)?;
    apply(&first, &mut record, TaskRunAction::Start, 1001).await?;
    assert_eq!(
        second.commit_task_patch(&stale).await,
        Err(TaskRepositoryError::Conflict)
    );
    apply(&first, &mut record, TaskRunAction::Connect { host }, 1002).await?;
    apply(
        &first,
        &mut record,
        TaskRunAction::StepIntent { host, step: 0 },
        1003,
    )
    .await?;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    let original: Vec<u8> = query_scalar("SELECT snapshot_json FROM task_runs WHERE id=?")
        .bind(id.to_string())
        .fetch_one(&raw)
        .await?;
    let backup = first.backup().await?;
    query("CREATE TRIGGER reject_task_event BEFORE INSERT ON task_run_events BEGIN SELECT RAISE(ABORT,'injected failure'); END").execute(&raw).await?;
    let failed = record.prepare(
        TaskRunAction::StepExited {
            host,
            step: 0,
            exit_code: 0,
        },
        1004,
    )?;
    assert_eq!(
        first.commit_task_patch(&failed).await,
        Err(TaskRepositoryError::Unavailable)
    );
    assert_eq!(second.load_task_run(id).await?, Some(record.clone()));
    query("DROP TRIGGER reject_task_event")
        .execute(&raw)
        .await?;
    raw.close().await;
    first.close().await;
    second.close().await;
    let reopened = SqliteProfileRepository::open(&path).await?;
    let mut recovered = reopened.load_task_run(id).await?.ok_or("missing task")?;
    apply(&reopened, &mut recovered, TaskRunAction::Recover, 1005).await?;
    assert_eq!(recovered.meta().state, TaskState::Queued);
    assert_eq!(recovered.targets()[0].state, TargetState::Unknown);
    assert_eq!(recovered.targets()[1].state, TargetState::Pending);
    let summaries = reopened.list_task_runs(None, 64, true).await?;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].targets, [2, 0, 0, 0, 0, 0, 0, 1]);
    let retry = TaskRunRecord::new(TaskSnapshot::retry(
        &recovered,
        &BTreeSet::from([host]),
        true,
        "operator".into(),
        1006,
    )?)?;
    reopened.create_task_run(&retry).await?;
    assert_eq!(reopened.load_task_run(id).await?, Some(recovered));
    assert_eq!(reopened.load_task_run(retry.id()).await?, Some(retry));
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    assert_eq!(
        query_scalar::<_, Vec<u8>>("SELECT snapshot_json FROM task_runs WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&raw)
            .await?,
        original
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM task_run_events WHERE run_id=?")
            .bind(id.to_string())
            .fetch_one(&raw)
            .await?,
        5
    );
    raw.close().await;
    reopened.close().await;
    cshell_storage::restore_backup(&path, &backup).await?;
    let restored = SqliteProfileRepository::open(&path).await?;
    assert_eq!(restored.load_task_run(id).await?, Some(record));
    restored.close().await;
    Ok(())
}

#[tokio::test]
async fn task_patch_binds_exact_snapshot_and_rejects_corrupt_or_oversized_rows()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("binding.sqlite");
    let repo = SqliteProfileRepository::open(&path).await?;
    let mut record = fixture()?;
    let id = record.id();
    let host = record.targets()[0].host_id;
    repo.create_task_run(&record).await?;
    apply(&repo, &mut record, TaskRunAction::Start, 1001).await?;
    apply(&repo, &mut record, TaskRunAction::Connect { host }, 1002).await?;
    apply(
        &repo,
        &mut record,
        TaskRunAction::StepIntent { host, step: 0 },
        1003,
    )
    .await?;
    let mut doc: serde_json::Value =
        serde_json::from_slice(&encode_task_snapshot(record.snapshot())?)?;
    doc["definition"]["steps"][0]["success_codes"] = serde_json::json!([7]);
    let modified = decode_task_snapshot(&serde_json::to_vec(&doc)?)?;
    let altered =
        TaskRunRecord::restore(modified, record.meta().clone(), record.targets().to_vec())?;
    let patch = altered.prepare(
        TaskRunAction::StepExited {
            host,
            step: 0,
            exit_code: 7,
        },
        1004,
    )?;
    assert_eq!(
        repo.commit_task_patch(&patch).await,
        Err(TaskRepositoryError::Corrupt)
    );
    assert_eq!(repo.load_task_run(id).await?, Some(record.clone()));
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("UPDATE task_targets SET row_json=zeroblob(?) WHERE run_id=? AND host_id=?")
        .bind((MAX_TASK_TARGET_BYTES + 1) as i64)
        .bind(id.to_string())
        .bind(host.to_string())
        .execute(&raw)
        .await?;
    assert_eq!(
        repo.load_task_run(id).await,
        Err(TaskRepositoryError::Corrupt)
    );
    let patch = record.prepare(
        TaskRunAction::StepExited {
            host,
            step: 0,
            exit_code: 0,
        },
        1004,
    )?;
    assert_eq!(
        repo.commit_task_patch(&patch).await,
        Err(TaskRepositoryError::Corrupt)
    );
    query("UPDATE task_targets SET row_json=? WHERE run_id=? AND host_id=?")
        .bind(encode_task_target(&record.targets()[0])?)
        .bind(id.to_string())
        .bind(host.to_string())
        .execute(&raw)
        .await?;
    query("UPDATE task_runs SET snapshot_json=zeroblob(?) WHERE id=?")
        .bind((MAX_TASK_SNAPSHOT_BYTES + 1) as i64)
        .bind(id.to_string())
        .execute(&raw)
        .await?;
    assert_eq!(
        repo.load_task_run(id).await,
        Err(TaskRepositoryError::Corrupt)
    );
    assert_eq!(
        repo.list_task_runs(None, 0, false).await,
        Err(TaskRepositoryError::Corrupt)
    );
    raw.close().await;
    repo.close().await;
    Ok(())
}

#[tokio::test]
async fn v11_task_migration_backs_up_host_data_and_rolls_back_partial_table_creation()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("v11.sqlite");
    let repo = SqliteProfileRepository::open(&path).await?;
    let fixture = fixture()?;
    let catalog = HostCatalog {
        hosts: fixture.snapshot().targets().targets().to_vec(),
        ..Default::default()
    };
    let hosts = repo.save_hosts(0, &catalog).await?;
    repo.close().await;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query("DROP TABLE task_run_events").execute(&raw).await?;
    query("DROP TABLE task_targets").execute(&raw).await?;
    query("DROP TABLE task_runs").execute(&raw).await?;
    query("CREATE TABLE task_targets(dummy TEXT)")
        .execute(&raw)
        .await?;
    support::strip_newer_fixture_schema(&raw).await?;
    query("PRAGMA user_version=11").execute(&raw).await?;
    raw.close().await;
    assert!(SqliteProfileRepository::open(&path).await.is_err());
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&raw)
            .await?,
        11
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master WHERE name='task_runs'")
            .fetch_one(&raw)
            .await?,
        0
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT revision FROM host_catalog_state WHERE id=1")
            .fetch_one(&raw)
            .await?,
        1
    );
    let backup = std::fs::read_dir(directory.path())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains(".backup-"))
        .ok_or("missing v11 backup")?;
    let saved = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", backup.display()))
        .await?;
    assert_eq!(
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&saved)
            .await?,
        11
    );
    assert_eq!(
        query_scalar::<_, Vec<u8>>("SELECT document_json FROM host_catalog_state WHERE id=1")
            .fetch_one(&saved)
            .await?,
        encode_hosts(&catalog)?
    );
    saved.close().await;
    query("DROP TABLE task_targets").execute(&raw).await?;
    raw.close().await;
    let upgraded = SqliteProfileRepository::open(&path).await?;
    assert_eq!(upgraded.load_hosts().await?, Some(hosts));
    assert!(upgraded.list_task_runs(None, 64, false).await?.is_empty());
    upgraded.create_task_run(&fixture).await?;
    assert_eq!(upgraded.load_task_run(fixture.id()).await?, Some(fixture));
    upgraded.close().await;
    Ok(())
}

#[tokio::test]
async fn persisted_retry_cannot_change_parent_commands_endpoints_or_unknown_confirmation()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let repo = SqliteProfileRepository::open(directory.path().join("retry.sqlite")).await?;
    let mut parent = fixture()?;
    let host = parent.targets()[0].host_id;
    repo.create_task_run(&parent).await?;
    apply(&repo, &mut parent, TaskRunAction::Start, 1001).await?;
    apply(&repo, &mut parent, TaskRunAction::Connect { host }, 1002).await?;
    apply(
        &repo,
        &mut parent,
        TaskRunAction::StepIntent { host, step: 0 },
        1003,
    )
    .await?;
    apply(&repo, &mut parent, TaskRunAction::Recover, 1004).await?;
    let retry = TaskSnapshot::retry(
        &parent,
        &BTreeSet::from([host]),
        true,
        "operator".into(),
        1005,
    )?;
    let original: serde_json::Value = serde_json::from_slice(&encode_task_snapshot(&retry)?)?;
    let mut changed = original.clone();
    changed["unknown_retry_confirmed"] = false.into();
    let unconfirmed = TaskRunRecord::new(decode_task_snapshot(&serde_json::to_vec(&changed)?)?)?;
    assert_eq!(
        repo.create_task_run(&unconfirmed).await,
        Err(TaskRepositoryError::Corrupt)
    );
    let mut changed = original.clone();
    changed["definition"]["steps"][0]["success_codes"] = serde_json::json!([7]);
    let different_task = TaskRunRecord::new(decode_task_snapshot(&serde_json::to_vec(&changed)?)?)?;
    assert_eq!(
        repo.create_task_run(&different_task).await,
        Err(TaskRepositoryError::Corrupt)
    );
    let mut changed = original;
    changed["targets"]["hosts"][0]["address"] = "substituted.test".into();
    let different_host = TaskRunRecord::new(decode_task_snapshot(&serde_json::to_vec(&changed)?)?)?;
    assert_eq!(
        repo.create_task_run(&different_host).await,
        Err(TaskRepositoryError::Corrupt)
    );
    assert_eq!(repo.load_task_run(retry.id()).await?, None);
    assert_eq!(repo.load_task_run(parent.id()).await?, Some(parent));
    repo.create_task_run(&TaskRunRecord::new(retry)?).await?;
    repo.close().await;
    Ok(())
}

#[derive(Debug)]
struct SqlBarrierExecutor {
    repo: Arc<SqliteProfileRepository>,
    calls: Arc<AtomicUsize>,
}
#[derive(Debug)]
struct SqlBarrierConnection {
    repo: Arc<SqliteProfileRepository>,
    calls: Arc<AtomicUsize>,
    run: TaskRunId,
    host: HostId,
}
#[async_trait::async_trait]
impl TaskExecutor for SqlBarrierExecutor {
    async fn connect(
        &self,
        snapshot: TaskSnapshot,
        host: HostId,
        _: TaskCancellation,
    ) -> Result<Box<dyn TaskConnection>, TaskStopReason> {
        Ok(Box::new(SqlBarrierConnection {
            repo: self.repo.clone(),
            calls: self.calls.clone(),
            run: snapshot.id(),
            host,
        }))
    }
}
#[async_trait::async_trait]
impl TaskConnection for SqlBarrierConnection {
    async fn execute(
        &mut self,
        step: usize,
        _: &str,
        _: TaskCancellation,
    ) -> Result<u32, TaskStopReason> {
        let record = self
            .repo
            .load_task_run(self.run)
            .await
            .map_err(|_| TaskStopReason::OutputFailure)?
            .ok_or(TaskStopReason::OutputFailure)?;
        if record
            .target(self.host)
            .is_none_or(|target| target.steps[step].state != TargetState::Running)
        {
            return Err(TaskStopReason::OutputFailure);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }
    async fn close(&mut self) {}
}
fn current_plan(clock: &SystemTaskClock, gated: bool) -> Result<TaskRunRecord, Box<dyn Error>> {
    let original = fixture()?;
    let mut definition = original.snapshot().definition().clone();
    definition.policy.concurrency = 2;
    if gated {
        definition.policy.canary_count = 1;
        definition.policy.approve_after_canary = true;
        definition.policy.batch_delay_ms = 100;
    }
    Ok(TaskRunRecord::new(TaskSnapshot::new(
        definition,
        original.snapshot().targets().clone(),
        "operator".into(),
        clock.now_ms()?,
    )?)?)
}

#[tokio::test]
async fn scheduler_commits_real_sqlite_intents_and_reopened_gate_before_continuing_steps()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scheduled.sqlite");
    let repo = Arc::new(SqliteProfileRepository::open(&path).await?);
    let clock = Arc::new(SystemTaskClock::new()?);
    let record = current_plan(&clock, true)?;
    let id = record.id();
    repo.create_task_run(&record).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let scheduler = TaskScheduler::new(
        repo.clone(),
        Arc::new(SqlBarrierExecutor {
            repo: repo.clone(),
            calls: calls.clone(),
        }),
        TaskResourceBudget::new(TaskResourceLimits::default())?,
        clock,
    );
    let (control, signals) = task_run_controls();
    let job = tokio::spawn(async move { scheduler.run(id, signals).await });
    let paused = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let current = repo
                .load_task_run(id)
                .await?
                .ok_or(TaskRepositoryError::Missing)?;
            if current.meta().state == TaskState::Paused {
                return Ok::<_, TaskRepositoryError>(current);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    let reopened = SqliteProfileRepository::open(&path).await?;
    assert_eq!(reopened.load_task_run(id).await?, Some(paused.clone()));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(paused.meta().batch_not_before_ms > paused.meta().updated_at_ms);
    control
        .approve_gate(paused.meta().gate.as_ref().ok_or("gate")?.id)
        .await?;
    let finished = tokio::time::timeout(Duration::from_secs(5), job).await???;
    assert_eq!(finished.meta().state, TaskState::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(reopened.load_task_run(id).await?, Some(finished));
    reopened.close().await;
    Arc::try_unwrap(repo)
        .map_err(|_| "repository still held")?
        .close()
        .await;
    Ok(())
}

#[tokio::test]
async fn scheduler_sqlite_intent_event_failure_closes_worker_and_recovery_never_executes_it()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("intent-failure.sqlite");
    let repo = Arc::new(SqliteProfileRepository::open(&path).await?);
    let clock = Arc::new(SystemTaskClock::new()?);
    let record = current_plan(&clock, false)?;
    let id = record.id();
    repo.create_task_run(&record).await?;
    let raw = SqlitePoolOptions::new()
        .connect(&format!("sqlite:{}", path.display()))
        .await?;
    query(r#"CREATE TRIGGER reject_exec_intent BEFORE INSERT ON task_run_events WHEN CAST(NEW.event_json AS TEXT) LIKE '%"kind":"step_intent"%' BEGIN SELECT RAISE(ABORT,'injected intent failure'); END"#).execute(&raw).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let scheduler = TaskScheduler::new(
        repo.clone(),
        Arc::new(SqlBarrierExecutor {
            repo: repo.clone(),
            calls: calls.clone(),
        }),
        TaskResourceBudget::new(TaskResourceLimits::default())?,
        clock,
    );
    let (_, signals) = task_run_controls();
    assert_eq!(
        scheduler.run(id, signals).await,
        Err(TaskSchedulerError::Storage(
            TaskRepositoryError::Unavailable
        ))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let stopped = repo.load_task_run(id).await?.ok_or("run")?;
    assert!(
        stopped
            .targets()
            .iter()
            .all(|target| !matches!(target.state, TargetState::Running | TargetState::Succeeded))
    );
    query("DROP TRIGGER reject_exec_intent")
        .execute(&raw)
        .await?;
    // Cancel queued work while startup recovery marks every in-flight connection Unknown.
    let (control, signals) = task_run_controls();
    control.cancel();
    let recovered = scheduler.run(id, signals).await?;
    assert!(
        recovered
            .targets()
            .iter()
            .any(|target| target.state == TargetState::Unknown)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(repo.load_task_run(id).await?, Some(recovered));
    raw.close().await;
    drop(scheduler);
    Arc::try_unwrap(repo)
        .map_err(|_| "repository still held")?
        .close()
        .await;
    Ok(())
}
