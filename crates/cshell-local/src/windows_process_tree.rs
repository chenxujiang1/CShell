//! A kill-on-close Job Object owns every ConPTY child of a local terminal.

#![allow(unsafe_code)]

use portable_pty::Child;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};

#[derive(Debug)]
pub(super) struct ProcessTree {
    job: OwnedHandle,
}

impl ProcessTree {
    pub(super) fn attach(child: &dyn Child) -> io::Result<Self> {
        let process = child
            .as_raw_handle()
            .ok_or_else(|| io::Error::other("ConPTY child has no process handle"))?;
        // SAFETY: null security attributes/name request a private, non-inherited job.
        let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw_job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a fresh owned handle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the pointer is valid for the full structure size and job is live.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: portable-pty retains a live process handle while child exists.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { job })
    }

    pub(super) fn terminate(&mut self) -> io::Result<()> {
        // SAFETY: the job handle remains owned by this ProcessTree.
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
