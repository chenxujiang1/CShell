use crate::{LocalSessionRegistry, SessionRegistryError};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub enum SessionExitMonitorError {
    #[error(transparent)]
    Registry(#[from] SessionRegistryError),
    #[error("local session exit monitor worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    #[error("local session exit monitor shutdown channel closed unexpectedly")]
    ShutdownChannelClosed,
}

/// Reaps exited PTYs even when no client is listing or attaching to sessions.
#[derive(Clone, Debug)]
pub struct SessionExitMonitor {
    registry: Arc<LocalSessionRegistry>,
    poll_interval: Duration,
}

impl SessionExitMonitor {
    #[must_use]
    pub fn new(registry: Arc<LocalSessionRegistry>) -> Self {
        Self {
            registry,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    fn with_poll_interval(registry: Arc<LocalSessionRegistry>, poll_interval: Duration) -> Self {
        Self {
            registry,
            poll_interval,
        }
    }

    pub async fn run_until(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<u64, SessionExitMonitorError> {
        let mut reaped = 0_u64;
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                return Ok(reaped);
            }
            tokio::select! {
                _ = interval.tick() => {
                    let registry = Arc::clone(&self.registry);
                    let exits = tokio::task::spawn_blocking(move || registry.poll_exits()).await??;
                    for (session_id, exit) in exits {
                        reaped = reaped.saturating_add(1);
                        tracing::info!(%session_id, ?exit, "local terminal process exited");
                    }
                }
                changed = shutdown.changed() => {
                    changed.map_err(|_| SessionExitMonitorError::ShutdownChannelClosed)?;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::SessionExitMonitor;
    use crate::LocalSessionRegistry;
    use cshell_domain::TerminalSize;
    use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    fn short_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "exit monitor probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "echo CSHELL_EXIT_MONITOR".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "exit monitor probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-lc".to_owned(), "printf CSHELL_EXIT_MONITOR".to_owned()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn exited_session_is_reaped_without_a_client_poll() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(LocalSessionRegistry::new(directory.path(), 32).unwrap());
        let attachment = registry
            .spawn_local(&short_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = attachment.session_id();
        let monitor = SessionExitMonitor::with_poll_interval(
            Arc::clone(&registry),
            Duration::from_millis(10),
        );
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let monitor_task = tokio::spawn(async move { monitor.run_until(shutdown_receiver).await });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !attachment.is_closed() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "monitor did not reap the exited PTY"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let exit = registry.poll_exit(session_id).unwrap().unwrap();
        assert!(exit.success, "PTY exit: {exit:?}");

        shutdown_sender.send(true).unwrap();
        let reaped = monitor_task.await.unwrap().unwrap();
        assert_eq!(reaped, 1);
        assert_eq!(registry.close(session_id).unwrap(), Some(exit));
    }
}
