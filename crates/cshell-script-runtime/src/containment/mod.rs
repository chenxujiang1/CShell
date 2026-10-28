//! Resource/lifecycle constraints and explicit platform access-policy setup.
use crate::{ScriptWorkerError, WorkerContainment, WorkerSandbox};
use cshell_domain::ScriptLimits;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(windows)]
mod windows;

pub fn prepare_worker_lifecycle(expected_parent: u32) -> Result<(), ScriptWorkerError> {
    if expected_parent == 0 {
        return Err(ScriptWorkerError::Isolation);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        unix::prepare(expected_parent)?;
        #[cfg(target_os = "linux")]
        linux::prepare()?;
        #[cfg(target_os = "macos")]
        macos::prepare()?;
        Ok(())
    }
    #[cfg(windows)]
    {
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Err(ScriptWorkerError::Isolation)
    }
}
/// Must run after resource setup and trusted input/watchdog thread creation,
/// before any user code. macOS verifies its earlier single-thread installation.
/// No platform falls back to the API-only policy.
pub fn enter_worker_access_policy(
    _run_id: crate::ScriptRunId,
) -> Result<WorkerSandbox, ScriptWorkerError> {
    #[cfg(target_os = "linux")]
    linux::install()?;
    #[cfg(target_os = "macos")]
    macos::verify()?;
    #[cfg(windows)]
    windows::verify_access(_run_id)?;
    WorkerSandbox::current_platform()
}
pub fn enter_worker_containment(
    limits: &ScriptLimits,
) -> Result<WorkerContainment, ScriptWorkerError> {
    if !(8 * 1024 * 1024..=64 * 1024 * 1024).contains(&limits.memory_bytes)
        || !(100..=10_000).contains(&limits.cpu_time_ms)
    {
        return Err(ScriptWorkerError::Isolation);
    }
    #[cfg(windows)]
    {
        windows::verify(limits)?;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        unix::restrict(limits)?;
    }
    WorkerContainment::current_platform()
}

#[cfg(all(feature = "supervisor", target_os = "macos"))]
pub(super) use macos::memory_within_limit;
#[cfg(all(feature = "supervisor", windows))]
pub(super) use windows::ParentContainment;
#[cfg(all(feature = "supervisor", windows))]
pub(super) use windows::WindowsChild;
#[cfg(all(feature = "supervisor", windows))]
pub(crate) use windows::cleanup_orphan_profile;
#[cfg(all(feature = "supervisor", not(windows)))]
pub(super) struct ParentContainment;
#[cfg(all(feature = "supervisor", not(windows)))]
impl ParentContainment {
    pub(super) fn new(_: &ScriptLimits, _: crate::ScriptRunId) -> Result<Self, ScriptWorkerError> {
        WorkerContainment::current_platform()?;
        WorkerSandbox::current_platform()?;
        Ok(Self)
    }
    pub(super) fn cleanup(&mut self) -> bool {
        true
    }
    pub(super) fn attach(&self, _: &tokio::process::Child) -> Result<(), ScriptWorkerError> {
        Ok(())
    }
}
