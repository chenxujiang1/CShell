#![cfg(unix)]
#![allow(clippy::unwrap_used, unsafe_code)]

use cshell_domain::TerminalSize;
use cshell_local::{LocalProfile, PtySession, WorkingDirectoryPolicy};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const FIXTURE_MODE: &str = "CSHELL_GUARDIAN_FIXTURE";
const FIXTURE_READY: &str = "CSHELL_GUARDIAN_READY";
const FIXTURE_SCRIPT: &str = "CSHELL_GUARDIAN_SCRIPT";
const FIXTURE_HEARTBEAT: &str = "CSHELL_GUARDIAN_HEARTBEAT";

#[test]
fn guardian_reaps_a_hup_ignoring_pty_after_daemon_is_force_killed() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("guardian-ready");
    let heartbeat = directory.path().join("heartbeat");
    let script = directory.path().join("heartbeat.sh");
    let quote = char::from(34);
    let script_body =
        format!("trap '' HUP\nwhile :; do printf x >> {quote}$1{quote}; /bin/sleep 0.05; done\n");
    std::fs::write(&script, script_body).unwrap();

    let mut fixture = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "guardian_daemon_fixture",
            "--nocapture",
        ])
        .env(FIXTURE_MODE, "1")
        .env(FIXTURE_READY, &ready)
        .env(FIXTURE_SCRIPT, &script)
        .env(FIXTURE_HEARTBEAT, &heartbeat)
        .spawn()
        .unwrap();

    wait_for_file_growth(&mut fixture, &heartbeat, 3, Duration::from_secs(10));
    let guardian_pid: libc::pid_t = std::fs::read_to_string(&ready)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    fixture.kill().unwrap();
    fixture.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_exists(guardian_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let guardian_stopped = !process_exists(guardian_pid);
    if !guardian_stopped {
        // SAFETY: this PID came from the fixture's freshly spawned guardian.
        unsafe {
            libc::kill(guardian_pid, libc::SIGKILL);
        }
    }
    assert!(
        guardian_stopped,
        "guardian {guardian_pid} survived its daemon"
    );

    std::thread::sleep(Duration::from_millis(200));
    let settled = heartbeat.metadata().unwrap().len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        heartbeat.metadata().unwrap().len(),
        settled,
        "HUP-ignoring PTY child continued after daemon and guardian exited"
    );
}

#[test]
#[ignore = "subprocess fixture for the Unix guardian test"]
fn guardian_daemon_fixture() {
    if std::env::var_os(FIXTURE_MODE).is_none() {
        return;
    }
    let ready = required_path(FIXTURE_READY);
    let script = required_path(FIXTURE_SCRIPT);
    let heartbeat = required_path(FIXTURE_HEARTBEAT);
    let profile = LocalProfile {
        name: "guardian crash fixture".to_owned(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            script.to_string_lossy().into_owned(),
            heartbeat.to_string_lossy().into_owned(),
        ],
        cwd_policy: WorkingDirectoryPolicy::Inherit,
        env_overrides: BTreeMap::new(),
    };
    let session = PtySession::spawn_guarded(
        &profile,
        TerminalSize::cells(24, 80),
        env!("CARGO_BIN_EXE_cshell-pty-guardian"),
    )
    .unwrap();
    let mut ready_file = std::fs::File::create(ready).unwrap();
    write!(ready_file, "{}", session.process_id().unwrap()).unwrap();
    ready_file.flush().unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(30));
    }
}

fn required_path(name: &str) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap()
}

fn wait_for_file_growth(fixture: &mut Child, path: &Path, minimum: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        assert!(
            fixture.try_wait().unwrap().is_none(),
            "daemon fixture exited before publishing a heartbeat"
        );
        if path
            .metadata()
            .is_ok_and(|metadata| metadata.len() >= minimum)
        {
            return;
        }
        assert!(Instant::now() < deadline, "guardian fixture did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal zero performs no delivery.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
