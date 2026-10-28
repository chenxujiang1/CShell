//! Explicit consumers of an authenticated SSH transport. Dropping one terminal
//! or SFTP view releases its lease; only the final consumer disconnects SSH.
use crate::ssh_route::{JumpConnection, RoutedSshConnection};
use cshell_ssh::SshConnection;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
struct Group {
    client: Arc<SshConnection>,
    jump: Option<JumpConnection>,
    consumers: Mutex<usize>,
}
impl Group {
    async fn shutdown(&self) {
        let _ = self.client.disconnect_confirmed().await;
        if let Some(jump) = &self.jump {
            jump.disconnect().await;
        }
    }
}
#[derive(Debug)]
struct Consumer {
    group: Arc<Group>,
    released: AtomicBool,
    writer: Mutex<Option<Arc<cshell_ssh::SshTerminalWriter>>>,
}
impl Consumer {
    fn start_release(&self) -> Option<tokio::task::JoinHandle<()>> {
        let last = {
            let mut count = self
                .group
                .consumers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.released.swap(true, Ordering::AcqRel) {
                return None;
            }
            *count -= 1;
            *count == 0
        };
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if !last && writer.is_none() {
            return None;
        }
        let group = self.group.clone();
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        Some(runtime.spawn(async move {
            if let Some(writer) = writer {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(2), writer.close()).await;
            }
            if last {
                group.shutdown().await;
            }
        }))
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.start_release();
    }
}
/// Cloning is an internal reference to the same consumer. `fork` creates a new
/// independently releasable consumer, bounded to 64 per SSH transport.
#[derive(Clone, Debug)]
pub(crate) struct TransportLease(Arc<Consumer>);
impl TransportLease {
    pub(crate) fn new(connection: RoutedSshConnection) -> Self {
        Self(Arc::new(Consumer {
            group: Arc::new(Group {
                client: Arc::new(connection.client),
                jump: connection.jump,
                consumers: Mutex::new(1),
            }),
            released: AtomicBool::new(false),
            writer: Mutex::new(None),
        }))
    }
    pub(crate) fn fork(&self) -> Result<Self, &'static str> {
        let mut count = self
            .0
            .group
            .consumers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.0.released.load(Ordering::Acquire)
            || *count == 0
            || self.0.group.client.transport_closed()
        {
            return Err("原 SSH 连接已断开，请先重新连接");
        }
        if *count >= 64 {
            return Err("此 SSH 连接的共享渠道已达到 64 个");
        }
        *count += 1;
        Ok(Self(Arc::new(Consumer {
            group: self.0.group.clone(),
            released: AtomicBool::new(false),
            writer: Mutex::new(None),
        })))
    }
    pub(crate) fn running(&self) -> bool {
        !self.0.released.load(Ordering::Acquire) && !self.0.group.client.transport_closed()
    }
    pub(crate) fn client(&self) -> Arc<SshConnection> {
        self.0.group.client.clone()
    }
    pub(crate) fn jump(&self) -> Option<JumpConnection> {
        self.0.group.jump.clone()
    }
    pub(crate) fn register_writer(&self, writer: Arc<cshell_ssh::SshTerminalWriter>) {
        *self
            .0
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
    }
    pub(crate) async fn release(&self) {
        // Detaching the join handle on cancellation still completes resource cleanup.
        if let Some(task) = self.0.start_release() {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests;
