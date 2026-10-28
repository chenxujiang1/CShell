use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

const RECONNECT_GRACE: Duration = Duration::from_secs(2);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct State {
    leases: usize,
    generation: u64,
    closing: bool,
}

#[derive(Debug)]
pub(crate) struct DesktopLifetime {
    state: Mutex<State>,
    shutdown: watch::Sender<bool>,
}
#[derive(Debug)]
pub(crate) struct DesktopLease(Arc<DesktopLifetime>);

impl DesktopLifetime {
    pub fn new(shutdown: watch::Sender<bool>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            shutdown,
        })
    }
    pub fn acquire(self: &Arc<Self>) -> Option<DesktopLease> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closing || *self.shutdown.borrow() {
            return None;
        }
        state.leases += 1;
        state.generation += 1;
        tracing::info!(
            desktop_clients = state.leases,
            "desktop lifetime lease acquired"
        );
        Some(DesktopLease(self.clone()))
    }
    fn stop_if_unused(&self, generation: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.leases == 0 && state.generation == generation && !state.closing {
            state.closing = true;
            tracing::info!("last desktop exited; stopping desktop-managed daemon");
            let _ = self.shutdown.send(true);
        }
    }
    pub async fn startup_watchdog(self: Arc<Self>) {
        tokio::time::sleep(STARTUP_TIMEOUT).await;
        self.stop_if_unused(0);
    }
}
impl Drop for DesktopLease {
    fn drop(&mut self) {
        let generation = {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.leases -= 1;
            tracing::info!(
                desktop_clients = state.leases,
                "desktop lifetime lease released"
            );
            if state.leases != 0 {
                return;
            }
            state.generation
        };
        let lifetime = self.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RECONNECT_GRACE).await;
            lifetime.stop_if_unused(generation);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn last_gui_exit_stops_daemon_but_reconnect_and_other_gui_keep_it_alive() {
        let (shutdown, mut receiver) = watch::channel(false);
        let lifetime = DesktopLifetime::new(shutdown);
        let first = lifetime.acquire().unwrap();
        let second = lifetime.acquire().unwrap();
        drop(first);
        tokio::time::sleep(RECONNECT_GRACE + Duration::from_millis(50)).await;
        assert!(!*receiver.borrow());
        drop(second);
        let replacement = lifetime.acquire().unwrap();
        tokio::time::sleep(RECONNECT_GRACE + Duration::from_millis(50)).await;
        assert!(
            !*receiver.borrow(),
            "a stale disconnect must not shut down a reconnected GUI"
        );
        drop(replacement);
        tokio::time::timeout(RECONNECT_GRACE + Duration::from_secs(1), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*receiver.borrow());
        assert!(
            lifetime.acquire().is_none(),
            "shutdown decision cannot admit a new owner"
        );
    }
    #[tokio::test]
    async fn startup_failure_shutdown_only_applies_before_first_gui() {
        let (shutdown, receiver) = watch::channel(false);
        let lifetime = DesktopLifetime::new(shutdown);
        let lease = lifetime.acquire().unwrap();
        lifetime.stop_if_unused(0);
        assert!(!*receiver.borrow());
        drop(lease);
        lifetime.stop_if_unused(0);
        assert!(
            !*receiver.borrow(),
            "startup timer cannot terminate a previously attached daemon"
        );
        let (shutdown, receiver) = watch::channel(false);
        DesktopLifetime::new(shutdown).stop_if_unused(0);
        assert!(*receiver.borrow());
    }
}
