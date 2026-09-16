//! The PTY library calls `setsid` before exec, making the child PID its PGID.

#![allow(unsafe_code)]

use portable_pty::Child;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

const TERM_GRACE: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub(super) struct ProcessTree {
    pgid: libc::pid_t,
    active: bool,
}

impl ProcessTree {
    pub(super) fn attach(child: &dyn Child) -> io::Result<Self> {
        let pid = child
            .process_id()
            .ok_or_else(|| io::Error::other("PTY child has no process ID"))?;
        let pgid = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::other("PTY child process ID exceeds pid_t"))?;
        if pgid <= 0 {
            return Err(io::Error::other("PTY child has an invalid process group"));
        }
        Ok(Self { pgid, active: true })
    }

    pub(super) fn terminate(&mut self, foreground_pgid: Option<libc::pid_t>) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let foreground_pgid = foreground_pgid.filter(|pgid| *pgid != self.pgid && *pgid > 0);
        self.signal_group(foreground_pgid, libc::SIGTERM)?;
        self.signal_group(Some(self.pgid), libc::SIGTERM)?;
        let deadline = Instant::now() + TERM_GRACE;
        while (self.group_exists(foreground_pgid)? || self.group_exists(Some(self.pgid))?)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        self.signal_group(foreground_pgid, libc::SIGKILL)?;
        self.signal_group(Some(self.pgid), libc::SIGKILL)?;
        self.active = false;
        Ok(())
    }

    fn signal_group(&self, pgid: Option<libc::pid_t>, signal: libc::c_int) -> io::Result<()> {
        let Some(pgid) = pgid else {
            return Ok(());
        };
        // SAFETY: portable-pty creates a new session with PGID equal to the
        // child PID; a negative PGID targets one group within that session.
        if unsafe { libc::kill(-pgid, signal) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn group_exists(&self, pgid: Option<libc::pid_t>) -> io::Result<bool> {
        let Some(pgid) = pgid else {
            return Ok(false);
        };
        // SAFETY: signal 0 probes the process group without sending a signal.
        if unsafe { libc::kill(-pgid, 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.active {
            let _ = self.signal_group(Some(self.pgid), libc::SIGKILL);
        }
    }
}

#[cfg(test)]
pub(crate) fn make_self_foreground() -> io::Result<()> {
    // SAFETY: all calls target the current test subprocess and its controlling PTY.
    unsafe {
        let pid = libc::getpid();
        if libc::signal(libc::SIGTTOU, libc::SIG_IGN) == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        if libc::setpgid(0, pid) == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::tcsetpgrp(libc::STDIN_FILENO, pid) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
