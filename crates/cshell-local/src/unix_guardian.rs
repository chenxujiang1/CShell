//! Minimal Unix PTY guardian used before any async runtime is initialized.

#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

pub const PTY_GUARDIAN_MODE_ARG: &str = "--cshell-pty-guardian-v1";
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const TERM_GRACE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug)]
struct ProcessEntry {
    pid: libc::pid_t,
    parent: libc::pid_t,
    session: libc::pid_t,
}

/// Runs guardian mode when the first argument is the private mode marker.
///
/// The remaining arguments are the owner PID, local profile program, and argv.
/// Call this before starting tracing, threads, or an async runtime.
pub fn run_pty_guardian_from_args() -> io::Result<Option<i32>> {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next();
    if arguments.next().as_deref() != Some(OsStr::new(PTY_GUARDIAN_MODE_ARG)) {
        return Ok(None);
    }
    let owner_pid = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "guardian owner PID is invalid")
        })?;
    let program = arguments.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "guardian program is missing")
    })?;
    run_guardian(owner_pid, program, arguments.collect()).map(Some)
}

fn run_guardian(
    owner_pid: libc::pid_t,
    program: OsString,
    arguments: Vec<OsString>,
) -> io::Result<i32> {
    let guardian_pid = current_pid();
    if session_id(0)? != guardian_pid {
        return Err(io::Error::other("PTY guardian is not its session leader"));
    }
    configure_guardian_signals()?;

    let mut command = Command::new(program);
    command.args(arguments);
    // SAFETY: guardian mode is deliberately single-threaded. The callback only
    // invokes async-signal-safe libc functions before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGHUP, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            let mut empty = MaybeUninit::<libc::sigset_t>::zeroed().assume_init();
            if libc::sigemptyset(&mut empty) == -1
                || libc::sigprocmask(libc::SIG_SETMASK, &raw const empty, std::ptr::null_mut())
                    == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let child_pid = libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::other("guarded child PID exceeds pid_t"))?;

    loop {
        if parent_pid() != owner_pid || termination_requested()? {
            terminate_descendants(&mut child, child_pid, guardian_pid)?;
            return Ok(1);
        }
        if let Some(status) = child.try_wait()? {
            terminate_descendants(&mut child, child_pid, guardian_pid)?;
            return Ok(status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn configure_guardian_signals() -> io::Result<()> {
    // SAFETY: guardian mode is single-threaded and changes only its own dispositions/mask.
    unsafe {
        if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        let mut blocked = MaybeUninit::<libc::sigset_t>::zeroed().assume_init();
        if libc::sigemptyset(&mut blocked) == -1
            || libc::sigaddset(&mut blocked, libc::SIGTERM) == -1
            || libc::sigprocmask(libc::SIG_BLOCK, &raw const blocked, std::ptr::null_mut()) == -1
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn termination_requested() -> io::Result<bool> {
    // SAFETY: sigpending writes a complete signal set and sigismember reads it.
    unsafe {
        let mut pending = MaybeUninit::<libc::sigset_t>::zeroed().assume_init();
        if libc::sigpending(&mut pending) == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(libc::sigismember(&pending, libc::SIGTERM) == 1)
    }
}

fn terminate_descendants(
    child: &mut Child,
    child_pid: libc::pid_t,
    guardian_session: libc::pid_t,
) -> io::Result<()> {
    let mut targets = collect_targets(child_pid, guardian_session);
    targets.insert(child_pid);
    signal_targets(&targets, libc::SIGTERM)?;

    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() && !targets.iter().any(|pid| process_exists(*pid)) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }

    targets.extend(collect_targets(child_pid, guardian_session));
    signal_targets(&targets, libc::SIGKILL)?;
    for _ in 0..10 {
        if child.try_wait()?.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn collect_targets(root: libc::pid_t, guardian_session: libc::pid_t) -> BTreeSet<libc::pid_t> {
    let entries = process_snapshot().unwrap_or_default();
    let mut targets = BTreeSet::from([root]);
    loop {
        let before = targets.len();
        for entry in &entries {
            if entry.session == guardian_session || targets.contains(&entry.parent) {
                targets.insert(entry.pid);
            }
        }
        if targets.len() == before {
            break;
        }
    }
    targets.remove(&current_pid());
    targets.retain(|pid| *pid > 1);
    targets
}

fn process_snapshot() -> io::Result<Vec<ProcessEntry>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,sess="])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "/bin/ps failed while collecting PTY descendants",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(ProcessEntry {
                pid: fields.next()?.parse().ok()?,
                parent: fields.next()?.parse().ok()?,
                session: fields.next()?.parse().ok()?,
            })
        })
        .collect())
}

fn signal_targets(targets: &BTreeSet<libc::pid_t>, signal: libc::c_int) -> io::Result<()> {
    let mut first_error = None;
    for pid in targets {
        // SAFETY: only positive PIDs from the guarded descendant/session snapshot are used.
        if unsafe { libc::kill(*pid, signal) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) && first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal zero performs no delivery.
    unsafe {
        libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn session_id(pid: libc::pid_t) -> io::Result<libc::pid_t> {
    // SAFETY: getsid is read-only.
    let session = unsafe { libc::getsid(pid) };
    if session == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(session)
    }
}

fn current_pid() -> libc::pid_t {
    // SAFETY: getpid has no preconditions.
    unsafe { libc::getpid() }
}

fn parent_pid() -> libc::pid_t {
    // SAFETY: getppid has no preconditions.
    unsafe { libc::getppid() }
}
