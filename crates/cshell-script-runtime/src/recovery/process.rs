//! Process birth stamps: a recycled PID proves the original has exited.
#![allow(unsafe_code)]
use super::{ProcessIdentity, ScriptWorkerError};

#[cfg(target_os = "linux")]
pub(super) fn identity(pid: u32) -> Result<Option<ProcessIdentity>, ScriptWorkerError> {
    use std::io::Read;
    let file = match std::fs::File::open(format!("/proc/{pid}/stat")) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ScriptWorkerError::Isolation),
    };
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| ScriptWorkerError::Isolation)?;
    if bytes.len() > 4096 {
        return Err(ScriptWorkerError::Isolation);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ScriptWorkerError::Isolation)?;
    let (_, fields) = text.rsplit_once(')').ok_or(ScriptWorkerError::Isolation)?;
    let birth = fields
        .split_whitespace()
        .nth(19)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(ScriptWorkerError::Isolation)?;
    Ok(Some(ProcessIdentity {
        pid,
        birth: [birth, 0],
    }))
}
#[cfg(target_os = "macos")]
pub(super) fn identity(pid: u32) -> Result<Option<ProcessIdentity>, ScriptWorkerError> {
    let pid_native = libc::pid_t::try_from(pid).map_err(|_| ScriptWorkerError::Isolation)?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: exact ABI output buffer, identity query without process mutation.
    let read = unsafe {
        libc::proc_pidinfo(
            pid_native,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as libc::c_int,
        )
    };
    if read == 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Ok(None);
    }
    if read != size as i32 {
        return Err(ScriptWorkerError::Isolation);
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(Some(ProcessIdentity {
        pid,
        birth: [info.pbi_start_tvsec, info.pbi_start_tvusec],
    }))
}
#[cfg(windows)]
pub(super) fn identity(pid: u32) -> Result<Option<ProcessIdentity>, ScriptWorkerError> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{Foundation::*, System::Threading::*};
    // SAFETY: read/synchronize rights only. Access denied is not absence.
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if raw.is_null() {
        return if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
            Ok(None)
        } else {
            Err(ScriptWorkerError::Isolation)
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
        return Ok(None);
    }
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    if unsafe {
        GetProcessTimes(
            handle.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(ScriptWorkerError::Isolation);
    }
    let birth = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Ok(Some(ProcessIdentity {
        pid,
        birth: [birth, 0],
    }))
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(super) fn identity(_: u32) -> Result<Option<ProcessIdentity>, ScriptWorkerError> {
    Err(ScriptWorkerError::Isolation)
}
