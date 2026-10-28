//! Only changes the dedicated worker, never the GUI/daemon resource limits.
#![allow(unsafe_code)]
use super::*;

pub(super) fn prepare(expected_parent: u32) -> Result<(), ScriptWorkerError> {
    // SAFETY: getppid has no caller-owned pointers or mutation of other processes.
    if unsafe { libc::getppid() } as u32 != expected_parent {
        return Err(ScriptWorkerError::Isolation);
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: documented prctl scalar arguments, no borrowed pointers.
        // Applied before creating input/watchdog threads so they inherit NNP.
        if unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        } != 0
            || unsafe {
                libc::prctl(
                    libc::PR_SET_NO_NEW_PRIVS,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                )
            } != 0
            || unsafe {
                libc::prctl(
                    libc::PR_SET_DUMPABLE,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                )
            } != 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        // Parent may have died before PDEATHSIG was installed. Never continue
        // based only on a PID supplied by the command line.
        if unsafe { libc::getppid() } as u32 != expected_parent {
            return Err(ScriptWorkerError::Isolation);
        }
    }
    Ok(())
}
pub(super) fn restrict(limits: &ScriptLimits) -> Result<(), ScriptWorkerError> {
    // Resource constants have different C types on Linux and macOS. Keep the
    // resource argument inferred at each call. Never raise an inherited limit.
    macro_rules! lower {
        ($resource:expr, $cap:expr) => {{
            let mut current = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: initialized, valid rlimit pointer; current process only.
            if unsafe { libc::getrlimit($resource, &mut current) } != 0 {
                return Err(ScriptWorkerError::Isolation);
            }
            let cap = ($cap as libc::rlim_t)
                .min(current.rlim_cur)
                .min(current.rlim_max);
            let value = libc::rlimit {
                rlim_cur: cap,
                rlim_max: cap,
            };
            if unsafe { libc::setrlimit($resource, &value) } != 0 {
                return Err(ScriptWorkerError::Isolation);
            }
        }};
    }
    lower!(libc::RLIMIT_CORE, 0);
    lower!(libc::RLIMIT_FSIZE, 0);
    lower!(libc::RLIMIT_NOFILE, 32);
    // OS CPU accounting has whole-second granularity, in addition to the
    // existing finer, conservative QuickJS active-wall interrupt budget.
    lower!(
        libc::RLIMIT_CPU,
        u64::from(limits.cpu_time_ms).div_ceil(1000)
    );
    #[cfg(target_os = "linux")]
    lower!(libc::RLIMIT_AS, limits.memory_bytes + 256 * 1024 * 1024);
    // macOS does not provide an equivalent dependable process address-space
    // cap here. Do not report QuickJS's heap limit as a macOS RSS/VM cap.
    Ok(())
}
