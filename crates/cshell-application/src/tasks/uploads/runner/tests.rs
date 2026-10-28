use super::*;
use std::sync::Mutex;

#[derive(Debug)]
struct Repository {
    snapshot: BatchUploadSnapshot,
    rows: Mutex<Vec<UploadTarget>>,
}
#[async_trait]
impl UploadRepository for Repository {
    async fn list_uploads(
        &self,
        _: Option<TaskRunId>,
        _: usize,
    ) -> Result<Vec<UploadRunSummary>, UploadRepositoryError> {
        Ok(Vec::new())
    }
    async fn create_upload(&self, _: &BatchUploadSnapshot) -> Result<(), UploadRepositoryError> {
        Err(UploadRepositoryError::Conflict)
    }
    async fn load_upload(
        &self,
        id: TaskRunId,
    ) -> Result<Option<(BatchUploadSnapshot, Vec<UploadTarget>)>, UploadRepositoryError> {
        Ok((id == self.snapshot.id)
            .then(|| (self.snapshot.clone(), self.rows.lock().unwrap().clone())))
    }
    async fn commit_upload_target(
        &self,
        expected: &UploadTarget,
        next: &UploadTarget,
    ) -> Result<(), UploadRepositoryError> {
        expected
            .validate_next(next, &self.snapshot)
            .map_err(|_| UploadRepositoryError::Corrupt)?;
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|r| r.host_id == expected.host_id)
            .ok_or(UploadRepositoryError::Missing)?;
        if row != expected {
            return Err(UploadRepositoryError::Conflict);
        }
        *row = next.clone();
        Ok(())
    }
}

#[derive(Debug)]
struct Executor {
    calls: Mutex<Vec<HostId>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl UploadExecutor for Executor {
    async fn source(&self, _: &BatchUploadSnapshot) -> Result<Box<dyn UploadSource>, UploadReason> {
        panic!("a failed connection must never read the upload source")
    }
    async fn connect(
        &self,
        _: &BatchUploadSnapshot,
        host: HostId,
        _: TaskCancellation,
    ) -> Result<Box<dyn UploadRemote>, UploadReason> {
        let first = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(host);
            calls.len() == 1
        };
        if first {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Err(UploadReason::Connection)
    }
}

fn fixture() -> (Arc<Repository>, Arc<Executor>, BatchUploadRunner) {
    let targets = crate::tasks::tests::fixture(2).unwrap().targets().clone();
    let clock = Arc::new(SystemTaskClock::new().unwrap());
    let snapshot = BatchUploadSnapshot::new(
        BatchUploadDefinition {
            publication_policy: None,
            name: "queue deadline".into(),
            local_source: std::env::temp_dir()
                .join("cshell-upload-source")
                .to_str()
                .unwrap()
                .into(),
            remote_destination: "/srv/upload".into(),
            size: 0,
            sha256: Sha256::digest([]).into(),
            concurrency: 1,
            queue_timeout_ms: 100,
            connect_timeout_ms: 5_000,
            transfer_timeout_ms: 5_000,
        },
        targets,
        clock.now_ms().unwrap(),
    )
    .unwrap();
    let rows = snapshot
        .targets
        .targets()
        .iter()
        .map(|host| UploadTarget::initial(&snapshot, host.id))
        .collect();
    let repository = Arc::new(Repository {
        snapshot,
        rows: Mutex::new(rows),
    });
    let executor = Arc::new(Executor {
        calls: Mutex::new(Vec::new()),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let runner = BatchUploadRunner::new(
        repository.clone(),
        executor.clone(),
        TaskResourceBudget::new(TaskResourceLimits::default()).unwrap(),
        clock,
    );
    (repository, executor, runner)
}

#[tokio::test]
async fn waiting_for_a_batch_slot_counts_towards_upload_queue_timeout() {
    let (repository, executor, runner) = fixture();
    let id = repository.snapshot.id;
    let running =
        tokio::spawn(async move { runner.run(id, true, TaskCancellation::detached()).await });
    tokio::time::timeout(Duration::from_secs(2), executor.entered.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    executor.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let calls = executor.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "an expired target must never connect");
    let rows = repository.rows.lock().unwrap();
    assert_eq!(rows[0].host_id, calls[0]);
    assert_eq!(rows[0].reason, Some(UploadReason::Connection));
    assert_eq!(rows[1].phase, UploadPhase::Failed);
    assert_eq!(rows[1].reason, Some(UploadReason::Timeout));
    assert!(!rows[1].temporary_owned);
}

#[tokio::test]
async fn expired_upload_queue_deadline_wins_even_when_a_resource_is_available() {
    let (repository, executor, runner) = fixture();
    let row = repository.rows.lock().unwrap()[0].clone();
    runner
        .target(
            &repository.snapshot,
            row,
            false,
            TaskCancellation::detached(),
            tokio::time::Instant::now() - Duration::from_millis(150),
        )
        .await
        .unwrap();
    assert!(executor.calls.lock().unwrap().is_empty());
    assert_eq!(
        repository.rows.lock().unwrap()[0].reason,
        Some(UploadReason::Timeout)
    );
}
