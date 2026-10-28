//! Opt-in release measurements, using only disposable vaults and synthetic secrets.
#![allow(clippy::unwrap_used)]
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
const ROUNDS: usize = 20;

struct Monitor {
    started: Instant,
    before: Option<crate::test_resources::ProcessSample>,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<(Option<crate::test_resources::ProcessSample>, u64)>,
}
impl Monitor {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let before = crate::test_resources::sample();
        let task = tokio::spawn(async move {
            let mut peak: Option<crate::test_resources::ProcessSample> = None;
            let mut lag = 0;
            while !done.load(Ordering::Relaxed) {
                let start = Instant::now();
                tokio::time::sleep(Duration::from_millis(10)).await;
                lag = lag.max(
                    start
                        .elapsed()
                        .saturating_sub(Duration::from_millis(10))
                        .as_micros() as u64,
                );
                if let Some(sample) = crate::test_resources::sample() {
                    match &mut peak {
                        Some(p) => {
                            p.rss_bytes = p.rss_bytes.max(sample.rss_bytes);
                            p.commit_bytes = p.commit_bytes.max(sample.commit_bytes);
                            p.handles = p.handles.max(sample.handles);
                        }
                        None => peak = Some(sample),
                    }
                }
            }
            (peak, lag)
        });
        Self {
            started: Instant::now(),
            before,
            stop,
            task,
        }
    }
    async fn finish(self) -> serde_json::Value {
        self.stop.store(true, Ordering::Relaxed);
        let (peak, lag) = self.task.await.unwrap();
        let elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        let cpu_ms = self
            .before
            .zip(crate::test_resources::sample())
            .map(|(a, b)| b.cpu_ms - a.cpu_ms);
        serde_json::json!({"cpu_ms":cpu_ms,"elapsed_ms":elapsed_ms,"average_cpu_cores":cpu_ms.map(|c|c/elapsed_ms),
            "peak_rss_bytes":peak.map(|p|p.rss_bytes),"peak_commit_bytes":peak.map(|p|p.commit_bytes),"peak_handles":peak.map(|p|p.handles),
            "runtime_heartbeat_max_delay_ms":lag as f64/1000.0})
    }
}

fn p95(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit optimized release authentication performance gate"]
async fn release_unlock_concurrency_and_cancellation() {
    assert!(!cfg!(debug_assertions), "run this gate with --release");
    for concurrency in [1, 4, 16] {
        let (_root, controller, _) = fixture();
        let mut wrong_ms = Vec::new();
        let mut correct_ms = Vec::new();
        let mut completion_ms = Vec::new();
        let mut cached_ms = Vec::new();
        let monitor = Monitor::start();
        eprintln!("release_auth_phase=shared_{concurrency}");
        for round in 0..ROUNDS {
            controller.lock();
            let mut work = Vec::new();
            let phase_start = Instant::now();
            for connection in 0..concurrency {
                let owner = controller.clone();
                work.push(tokio::spawn(async move {
                    owner
                        .prepare_connection(format!("release-fixture-{round}-{connection}:22"))
                        .await
                        .unwrap();
                    phase_start.elapsed().as_secs_f64() * 1000.0
                }));
            }
            let first = prompt(&controller, &work[0]).await;
            let started = Instant::now();
            answer(
                &controller,
                first.id,
                "incorrect-release-test-password",
                false,
            );
            let retry = prompt(&controller, &work[0]).await;
            wrong_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            let started = Instant::now();
            answer(&controller, retry.id, "original-test-password", false);
            for task in work {
                completion_ms.push(task.await.unwrap());
            }
            correct_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            let epoch = generation(&controller);
            let started = Instant::now();
            controller
                .prepare_connection("cached-release-fixture:22".into())
                .await
                .unwrap();
            cached_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(generation(&controller), epoch);
            assert!(controller.response(None).unlocked);
        }
        controller.lock();
        let owner = controller.clone();
        let work = tokio::spawn(async move {
            owner
                .prepare_connection("cancel-release-fixture:22".into())
                .await
        });
        let pending = prompt(&controller, &work).await;
        let started = Instant::now();
        answer(&controller, pending.id, "", true);
        assert!(matches!(
            work.await.unwrap(),
            Err(cshell_ssh::SshError::AuthenticationCancelled)
        ));
        let cancel_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert!(!controller.response(None).unlocked);
        let resources = monitor.finish().await;
        eprintln!(
            "{}",
            serde_json::json!({
                "gate":"release-authentication", "scenario":"shared_vault", "concurrency":concurrency,
            "rounds":ROUNDS, "connection_samples":completion_ms.len(), "wrong_password_p95_ms":p95(&mut wrong_ms),
                "correct_password_p95_ms":p95(&mut correct_ms), "all_connections_p95_ms":p95(&mut completion_ms),
                "cached_unlock_p95_ms":p95(&mut cached_ms), "cancellation_ms":cancel_ms,
                "resources":resources,
            })
        );
    }
    for concurrency in [1, 4, 16] {
        eprintln!("release_auth_phase=independent_{concurrency}");
        let fixtures: Vec<_> = (0..concurrency).map(|_| fixture()).collect();
        let monitor = Monitor::start();
        let mut samples = Vec::new();
        for _ in 0..ROUNDS {
            let mut work = Vec::new();
            for (_, controller, _) in &fixtures {
                controller.lock();
                let owner = controller.clone();
                work.push(tokio::task::spawn_blocking(move || {
                    let started = Instant::now();
                    assert_eq!(
                        owner
                            .unlock(
                                &Secret::new(b"incorrect-release-test-password".to_vec()),
                                generation(&owner)
                            )
                            .err(),
                        Some(VaultError::AuthenticationFailed)
                    );
                    owner.unlock(&password(), generation(&owner)).unwrap();
                    started.elapsed().as_secs_f64() * 1000.0
                }));
            }
            for task in work {
                samples.push(task.await.unwrap());
            }
        }
        let resources = monitor.finish().await;
        eprintln!(
            "{}",
            serde_json::json!({"gate":"release-authentication", "scenario":"independent_vaults",
                "concurrency":concurrency, "rounds":ROUNDS, "samples":samples.len(), "wrong_then_correct_p95_ms":p95(&mut samples),"resources":resources})
        );
    }
}
