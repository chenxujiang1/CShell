//! Real SSH sockets, authentication, terminal parsers, journals and recovery.
#![allow(clippy::unwrap_used)]
use super::*;
use std::time::Instant;

fn frame_has(session: &SshSession, marker: &str, size: TerminalSize) -> bool {
    session
        .full_frame()
        .and_then(|f| f.decode_terminal_snapshot().ok())
        .is_some_and(|f| {
            f.rows == size.rows
                && f.cols == size.cols
                && f.cells
                    .iter()
                    .map(|c| c.character)
                    .collect::<String>()
                    .contains(marker)
        })
}

async fn probe(fixtures: &mut [Fixture], round: usize) -> (u64, f64) {
    let size = if round % 2 == 0 {
        TerminalSize::cells(24, 80)
    } else {
        TerminalSize::cells(30, 100)
    };
    let mut starts = Vec::new();
    for (index, f) in fixtures.iter_mut().enumerate() {
        f.received.lock().unwrap().clear();
        let marker = format!("SOAK_{index:03}_{round:06}");
        f.session.resize(size).unwrap();
        starts.push((Instant::now(), marker.clone()));
        f.session
            .send_input_observed(
                &InputAction::Text(format!("{marker}\n")),
                Some(f.session.generation()),
            )
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut acknowledged = vec![false; fixtures.len()];
    let mut latencies = Vec::new();
    while acknowledged.iter().any(|done| !done) {
        for (index, f) in fixtures.iter().enumerate() {
            if !acknowledged[index]
                && frame_has(&f.session, &format!("ACK:{}", starts[index].1), size)
            {
                assert_eq!(
                    f.received.lock().unwrap().as_slice(),
                    format!("{}\n", starts[index].1).as_bytes(),
                    "input replay or misrouting"
                );
                latencies.push(starts[index].0.elapsed().as_secs_f64() * 1000.0);
                acknowledged[index] = true;
            }
        }
        assert!(Instant::now() < deadline, "SSH echo or resize timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    latencies.sort_by(f64::total_cmp);
    (
        fixtures.len() as u64,
        latencies[(latencies.len() * 95).div_ceil(100) - 1],
    )
}

async fn run(stages: &[(usize, u64)], mode: &str) {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let root = project.join("target/ssh-soak-fixtures");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    assert!(root.starts_with(project.join("target")));
    let baseline = crate::test_resources::sample();
    let started = Instant::now();
    let mut fixtures = Vec::new();
    let mut handles = Vec::new();
    let mut rounds = 0;
    let mut acknowledgements = 0;
    let mut recoveries = 0;
    for &(count, seconds) in stages {
        while fixtures.len() < count {
            let mut f = fixture_in(false, true, tempfile::tempdir_in(&root).unwrap()).await;
            handles.push(f.handles.recv().await.unwrap());
            fixtures.push(f);
        }
        let stage_start = Instant::now();
        let mut last_fault = Instant::now();
        let mut warm = None;
        let mut last = None;
        while stage_start.elapsed() < Duration::from_secs(seconds) {
            let (acks, p95) = probe(&mut fixtures, rounds).await;
            acknowledgements += acks;
            rounds += 1;
            if last_fault.elapsed() >= Duration::from_secs(if mode == "smoke" { 2 } else { 120 }) {
                let index = recoveries % fixtures.len();
                let old_id = fixtures[index].session.id();
                let old_epoch = fixtures[index]
                    .session
                    .connection_epoch
                    .load(Ordering::Acquire);
                disconnect(handles[index].clone()).await;
                let new_handle =
                    tokio::time::timeout(Duration::from_secs(20), fixtures[index].handles.recv())
                        .await
                        .unwrap()
                        .unwrap();
                wait(|| {
                    fixtures[index].session.running()
                        && !fixtures[index].session.summary().reconnecting
                        && fixtures[index]
                            .session
                            .connection_epoch
                            .load(Ordering::Acquire)
                            > old_epoch
                })
                .await;
                assert_eq!(fixtures[index].session.id(), old_id);
                handles[index] = new_handle;
                recoveries += 1;
                last_fault = Instant::now();
            }
            let resources = crate::test_resources::sample();
            if stage_start.elapsed() >= Duration::from_secs(60) && warm.is_none() {
                warm = resources;
            }
            last = resources;
            eprintln!(
                "{}",
                serde_json::json!({"gate":"ssh-session-soak", "mode":mode, "elapsed_seconds":started.elapsed().as_secs(),
                "sessions":fixtures.len(), "round":rounds,"acknowledgements":acknowledgements, "recoveries":recoveries,
                "input_to_frame_p95_ms":p95,"process":resources})
            );
            tokio::time::sleep(Duration::from_secs(if mode == "smoke" { 1 } else { 10 })).await;
        }
        if let (Some(warm), Some(last)) = (warm, last) {
            assert!(
                last.rss_bytes.saturating_sub(warm.rss_bytes) < 128 * 1024 * 1024,
                "steady-stage RSS growth exceeded 128 MiB"
            );
            assert!(
                last.handles <= warm.handles + 64,
                "steady-stage handles keep growing"
            );
        }
    }
    for f in &fixtures {
        f.session.close().await.unwrap();
    }
    let mut directories = Vec::new();
    for f in fixtures {
        assert!(!f.session.running());
        let Fixture {
            _directory: directory,
            session,
            server,
            vault,
            ..
        } = f;
        server.abort();
        let _ = server.await;
        drop(session);
        drop(vault);
        directories.push(directory);
    }
    drop(handles);
    tokio::time::sleep(Duration::from_secs(15)).await;
    for directory in directories {
        directory.close().unwrap();
    }
    let final_resources = crate::test_resources::sample();
    if let (Some(before), Some(after)) = (baseline, final_resources) {
        assert!(
            after.handles <= before.handles + 64,
            "closed sessions leaked handles: {before:?} -> {after:?}"
        );
    }
    eprintln!(
        "{}",
        serde_json::json!({"gate":"ssh-session-soak-complete", "mode":mode,"elapsed_seconds":started.elapsed().as_secs(),
        "rounds":rounds,"acknowledgements":acknowledgements,"recoveries":recoveries,"baseline":baseline,"after_close":final_resources})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit real SSH soak fixture smoke test"]
async fn real_ssh_soak_smoke() {
    run(&[(2, 5), (4, 5)], "smoke").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit 30 minute real SSH 20-to-100-session soak"]
async fn real_ssh_20_to_100_connections_soak() {
    run(&[(20, 300), (100, 1500)], "full").await;
}
