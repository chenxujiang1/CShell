//! Raw handles never cross this adapter. The job is unnamed and not inheritable.
#![allow(unsafe_code)]
use super::*;
#[cfg(feature = "supervisor")]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::{mem::size_of, ptr};
use windows_sys::Win32::System::{JobObjects::*, Threading::GetCurrentProcess};
mod identity;
#[cfg(feature = "supervisor")]
mod launch;
#[cfg(feature = "supervisor")]
pub(crate) use launch::WindowsChild;

const REQUIRED: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
    | JOB_OBJECT_LIMIT_PROCESS_MEMORY
    | JOB_OBJECT_LIMIT_PROCESS_TIME
    | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
#[cfg(feature = "supervisor")]
pub(crate) fn cleanup_orphan_profile(run_id: crate::ScriptRunId) -> bool {
    identity::cleanup_orphan(run_id)
}
fn memory(limits: &ScriptLimits) -> usize {
    limits.memory_bytes as usize + 64 * 1024 * 1024
}
fn cpu(limits: &ScriptLimits) -> i64 {
    i64::from(limits.cpu_time_ms) * 10_000
}
#[cfg(feature = "supervisor")]
pub(crate) struct ParentContainment {
    handle: Option<OwnedHandle>,
    profile: identity::Profile,
    run_id: crate::ScriptRunId,
}
#[cfg(feature = "supervisor")]
impl ParentContainment {
    pub(crate) fn new(
        limits: &ScriptLimits,
        run_id: crate::ScriptRunId,
    ) -> Result<Self, ScriptWorkerError> {
        // SAFETY: null attributes/name create a non-inherited unnamed job;
        // successful handle ownership is immediately transferred to OwnedHandle.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(ScriptWorkerError::Isolation);
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = REQUIRED;
        info.BasicLimitInformation.ActiveProcessLimit = 1;
        info.BasicLimitInformation.PerProcessUserTimeLimit = cpu(limits);
        info.ProcessMemoryLimit = memory(limits);
        // SAFETY: buffer is initialized, correctly sized and alive for this call.
        if unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        let profile = identity::Profile::create(run_id)?;
        Ok(Self {
            handle: Some(handle),
            profile,
            run_id,
        })
    }
    pub(crate) fn spawn(
        &mut self,
        source: &std::path::Path,
        directory: &std::path::Path,
    ) -> Result<WindowsChild, ScriptWorkerError> {
        launch::spawn(self, source, directory)
    }
    pub(crate) fn cleanup(&mut self) -> bool {
        // Called only without a child or after wait has proved exit. Closing
        // the last Job handle without wait is never taken as that proof.
        self.handle.take();
        self.profile.cleanup()
    }
    pub(crate) fn attach(&self, child: &WindowsChild) -> Result<(), ScriptWorkerError> {
        let process = child.raw_handle();
        let job = self.handle.as_ref().ok_or(ScriptWorkerError::Isolation)?;
        let mut assigned = 0;
        // SAFETY: child/job handles are borrowed while their owners are alive.
        // Job membership was installed atomically at process creation.
        if unsafe { IsProcessInJob(process, job.as_raw_handle(), &mut assigned) } == 0
            || assigned == 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        identity::verify(process, self.run_id)?;
        Ok(())
    }
}
pub(super) fn verify_access(run_id: crate::ScriptRunId) -> Result<(), ScriptWorkerError> {
    identity::verify(unsafe { GetCurrentProcess() }, run_id)
}
pub(super) fn verify(limits: &ScriptLimits) -> Result<(), ScriptWorkerError> {
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut assigned = 0;
    // SAFETY: pseudo process handle is valid; null job queries the current job.
    // Buffers are initialized and have the exact documented sizes.
    if unsafe { IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &mut assigned) } == 0
        || assigned == 0
        || unsafe {
            QueryInformationJobObject(
                ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ptr::null_mut(),
            )
        } == 0
    {
        return Err(ScriptWorkerError::Isolation);
    }
    let flags = info.BasicLimitInformation.LimitFlags;
    if flags & REQUIRED != REQUIRED
        || flags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK) != 0
        || info.BasicLimitInformation.ActiveProcessLimit != 1
        || info.ProcessMemoryLimit == 0
        || info.ProcessMemoryLimit > memory(limits)
        || info.BasicLimitInformation.PerProcessUserTimeLimit <= 0
        || info.BasicLimitInformation.PerProcessUserTimeLimit > cpu(limits)
    {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(())
}
