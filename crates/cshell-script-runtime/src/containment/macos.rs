//! Fixed worker-only Seatbelt policy, installed before input/watchdog threads.
//! No script text, parameter, user path or caller-supplied SBPL is accepted.
#![allow(unsafe_code)]
use crate::ScriptWorkerError;
use std::{
    mem::{MaybeUninit, size_of},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

// Custom source, flags=0: never select deprecated built-in named profiles.
// Denial logging is disabled so attempted names/paths cannot become OS logs.
const PROFILE: &std::ffi::CStr = c"\
(version 1)
(deny default (with no-log))
(deny process-info* (with no-log))
(deny sysctl* (with no-log))
(deny signal (with no-log))
(allow signal (target self))
(allow process-info-pidinfo process-info-pidfdinfo process-info-setcontrol (target self))
(allow file-read-data file-write-data
  (require-all (literal \"/dev/null\") (vnode-type CHARACTER-DEVICE)))
(allow sysctl-read
  (sysctl-name \"hw.ncpu\")
  (sysctl-name \"hw.activecpu\")
  (sysctl-name \"hw.pagesize\"))
";

// The system ABI is isolated here rather than spread through worker/GUI code.
// sandbox_init is deprecated by Apple; there is no API-only fallback if its
// custom-profile implementation is unavailable or rejects this fixed policy.
#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init(
        profile: *const libc::c_char,
        flags: u64,
        error: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(error: *mut libc::c_char);
    fn sandbox_check(
        pid: libc::pid_t,
        operation: *const libc::c_char,
        filter: libc::c_int,
        ...
    ) -> libc::c_int;
}
unsafe extern "C" {
    fn mach_ports_register(
        task: libc::mach_port_t,
        ports: *mut libc::mach_port_t,
        count: u32,
    ) -> libc::kern_return_t;
    fn task_set_special_port(
        task: libc::mach_port_t,
        which: libc::c_int,
        port: libc::mach_port_t,
    ) -> libc::kern_return_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        port: libc::mach_port_t,
    ) -> libc::kern_return_t;
    static mut bootstrap_port: libc::mach_port_t;
}
fn drop_inherited_service_ports() -> Result<(), ScriptWorkerError> {
    // SAFETY: dedicated single-threaded worker, task-local registered stash and
    // bootstrap port only. Keep task/thread/host ports needed by libSystem.
    let task = unsafe { libc::mach_task_self() };
    if unsafe { mach_ports_register(task, ptr::null_mut(), 0) } != 0
        || unsafe { task_set_special_port(task, 4, 0) } != 0
    {
        // TASK_BOOTSTRAP_PORT
        return Err(ScriptWorkerError::Isolation);
    }
    let old = unsafe { bootstrap_port };
    unsafe {
        bootstrap_port = 0;
    }
    if old != 0 && unsafe { mach_port_deallocate(task, old) } != 0 {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(())
}

static INSTALLED: AtomicBool = AtomicBool::new(false);
const MAX_DESCRIPTORS: usize = 32 * 1024;

fn descriptors() -> Result<Vec<libc::proc_fdinfo>, ScriptWorkerError> {
    let mut entries = Vec::with_capacity(MAX_DESCRIPTORS);
    entries.resize_with(MAX_DESCRIPTORS, || libc::proc_fdinfo {
        proc_fd: -1,
        proc_fdtype: 0,
    });
    let capacity = size_of::<libc::proc_fdinfo>() * entries.len();
    // SAFETY: correctly aligned, initialized writable array of the exact ABI
    // struct; only this process is queried and no library-owned pointer escapes.
    let bytes = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDLISTFDS,
            0,
            entries.as_mut_ptr().cast(),
            capacity as libc::c_int,
        )
    };
    // A full buffer might be a truncated snapshot. Never infer absence from it.
    if bytes <= 0
        || bytes as usize >= capacity
        || bytes as usize % size_of::<libc::proc_fdinfo>() != 0
    {
        return Err(ScriptWorkerError::Isolation);
    }
    entries.truncate(bytes as usize / size_of::<libc::proc_fdinfo>());
    if entries.iter().any(|entry| entry.proc_fd < 0) {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(entries)
}

fn close_extra_descriptors() -> Result<(), ScriptWorkerError> {
    // Called only during single-threaded trusted startup. A snapshot enumerates
    // even inherited descriptors above a previously lowered RLIMIT_NOFILE.
    for entry in descriptors()? {
        if entry.proc_fd > 2 {
            // SAFETY: descriptor belongs to this dedicated worker; no other
            // application code owns it. Fail on EINTR rather than retry/reuse.
            if unsafe { libc::close(entry.proc_fd) } != 0 {
                return Err(ScriptWorkerError::Isolation);
            }
        }
    }
    let remaining = descriptors()?;
    if remaining.len() != 3 || (0..=2).any(|fd| !remaining.iter().any(|e| e.proc_fd == fd)) {
        return Err(ScriptWorkerError::Isolation);
    }
    if remaining.iter().any(|entry| {
        entry.proc_fdtype
            != if entry.proc_fd < 2 {
                libc::PROX_FDTYPE_PIPE as u32
            } else {
                libc::PROX_FDTYPE_VNODE as u32
            }
    }) {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(())
}

fn verify_standard_descriptors() -> Result<(), ScriptWorkerError> {
    for fd in 0..=2 {
        let mut status = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid writable stat pointer and an existing standard fd.
        if unsafe { libc::fstat(fd, status.as_mut_ptr()) } != 0 {
            return Err(ScriptWorkerError::Isolation);
        }
        let status = unsafe { status.assume_init() };
        let kind = status.st_mode & libc::S_IFMT;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0
            || (fd < 2 && kind != libc::S_IFIFO)
            || (fd == 0 && flags & libc::O_ACCMODE != libc::O_RDONLY)
            || (fd > 0 && flags & libc::O_ACCMODE != libc::O_WRONLY)
        {
            return Err(ScriptWorkerError::Isolation);
        }
        if fd == 2 {
            let mut null_status = MaybeUninit::<libc::stat>::uninit();
            // SAFETY: constant NUL-terminated path and valid stat buffer.
            if unsafe { libc::stat(c"/dev/null".as_ptr(), null_status.as_mut_ptr()) } != 0 {
                return Err(ScriptWorkerError::Isolation);
            }
            let null_status = unsafe { null_status.assume_init() };
            if kind != libc::S_IFCHR
                || null_status.st_mode & libc::S_IFMT != libc::S_IFCHR
                || status.st_rdev != null_status.st_rdev
                || status.st_dev != null_status.st_dev
                || status.st_ino != null_status.st_ino
            {
                return Err(ScriptWorkerError::Isolation);
            }
        }
    }
    Ok(())
}

pub(super) fn prepare() -> Result<(), ScriptWorkerError> {
    if INSTALLED.load(Ordering::Acquire) {
        return Err(ScriptWorkerError::Isolation);
    }
    close_extra_descriptors()?;
    verify_standard_descriptors()?;
    let mut error = ptr::null_mut();
    // SAFETY: static profile, custom-source flags, valid error out-pointer.
    // Applies to the worker process before it creates any application threads.
    let result = unsafe { sandbox_init(PROFILE.as_ptr(), 0, &mut error) };
    if !error.is_null() {
        // Never read, format or log the native diagnostic text.
        unsafe { sandbox_free_error(error) };
    }
    if result != 0 || !is_sandboxed() {
        return Err(ScriptWorkerError::Isolation);
    }
    // The native profile compiler must not leave extra file handles behind.
    close_extra_descriptors()?;
    drop_inherited_service_ports()?;
    INSTALLED.store(true, Ordering::Release);
    Ok(())
}

#[cfg(feature = "supervisor")]
pub(crate) fn memory_within_limit(pid: u32, heap: u64) -> Result<bool, ScriptWorkerError> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| ScriptWorkerError::Isolation)?;
    let mut info = MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = size_of::<libc::proc_taskinfo>();
    // SAFETY: exact ABI buffer, the parent queries its still-owned child only.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size as libc::c_int,
        )
    };
    if read == 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Ok(true);
    }
    if read != size as i32 {
        return Err(ScriptWorkerError::Isolation);
    }
    let info = unsafe { info.assume_init() };
    Ok(info.pti_resident_size <= heap + 128 * 1024 * 1024)
}

fn is_sandboxed() -> bool {
    // SAFETY: null operation/filter=0 queries sandbox state only; no variadic
    // filter argument is required. Negative errors are never treated as true.
    unsafe { sandbox_check(libc::getpid(), ptr::null(), 0) == 1 }
}

pub(super) fn verify() -> Result<(), ScriptWorkerError> {
    // sandbox_check alone cannot identify a profile. Require our successful
    // fixed-profile installation too, before issuing the Ready receipt.
    if INSTALLED.load(Ordering::Acquire) && is_sandboxed() {
        Ok(())
    } else {
        Err(ScriptWorkerError::Isolation)
    }
}
