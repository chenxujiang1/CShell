//! Production daemon lifecycle for native desktop-session observations.
use crate::vault_control::VaultController;
use std::sync::Arc;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
    sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Keep alive for the daemon lifetime. Dropping it stops its owned monitor only.
pub struct SessionLockMonitor {
    #[cfg(windows)]
    _worker: windows::WindowsSessionMonitor,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    _worker: MonitorThread,
}
impl std::fmt::Debug for SessionLockMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLockMonitor").finish_non_exhaustive()
    }
}
impl SessionLockMonitor {
    pub(crate) fn start(vault: &Arc<VaultController>) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            let worker = windows::WindowsSessionMonitor::start(vault)?;
            Ok(Self { _worker: worker })
        }
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                _worker: MonitorThread::start("cshell-logind-session", vault, linux::run)?,
            })
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                _worker: MonitorThread::start("cshell-macos-session", vault, macos::run)?,
            })
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            // Native observations are diagnostic; no automatic Vault locking.
            let _ = vault;
            Ok(Self {})
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct MonitorThread {
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl MonitorThread {
    fn start(
        name: &str,
        vault: &Arc<VaultController>,
        run: impl FnOnce(Weak<VaultController>, Arc<AtomicBool>) + Send + 'static,
    ) -> std::io::Result<Self> {
        vault.observe_desktop_session(cshell_ipc::VaultSessionState::Unavailable);
        let weak = Arc::downgrade(vault);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let worker = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _exit = MonitorExit(weak.clone());
                run(weak, stop);
            })?;
        Ok(Self {
            stopped,
            worker: Some(worker),
        })
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for MonitorThread {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _joined = worker.join();
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct MonitorExit(Weak<VaultController>);
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for MonitorExit {
    fn drop(&mut self) {
        if let Some(vault) = self.0.upgrade() {
            vault.observe_desktop_session(cshell_ipc::VaultSessionState::Unavailable);
        }
    }
}
