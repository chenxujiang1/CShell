//! Process-local measurements for opt-in native verification, never application telemetry.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct ProcessSample {
    pub rss_bytes: u64,
    pub commit_bytes: u64,
    pub handles: u32,
    pub cpu_ms: f64,
}

#[cfg(windows)]
pub(crate) fn sample() -> Option<ProcessSample> {
    windows::sample()
}
#[cfg(not(windows))]
pub(crate) fn sample() -> Option<ProcessSample> {
    None
}

#[cfg(windows)]
mod windows {
    #![allow(unsafe_code)]
    use super::ProcessSample;
    use std::mem::{MaybeUninit, size_of};
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{GetCurrentProcess, GetProcessHandleCount, GetProcessTimes},
        },
    };
    pub(super) fn sample() -> Option<ProcessSample> {
        unsafe {
            let process = GetCurrentProcess();
            let mut counters = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::zeroed();
            (*counters.as_mut_ptr()).cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            let mut handles = 0;
            let mut created = FILETIME::default();
            let mut exited = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            if K32GetProcessMemoryInfo(
                process,
                counters.as_mut_ptr(),
                size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ) == 0
                || GetProcessHandleCount(process, &mut handles) == 0
                || GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) == 0
            {
                return None;
            }
            let counters = counters.assume_init();
            let ticks =
                |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
            Some(ProcessSample {
                rss_bytes: counters.WorkingSetSize as u64,
                commit_bytes: counters.PagefileUsage as u64,
                handles,
                cpu_ms: (ticks(kernel) + ticks(user)) as f64 / 10_000.0,
            })
        }
    }
}
