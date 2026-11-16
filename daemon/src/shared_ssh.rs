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
    id: cshell_domain::SessionId,
    client: Arc<SshConnection>,
    jump: Option<JumpConnection>,
    consumers: Mutex<usize>,
    jump_setup: Arc<tokio::sync::Semaphore>,
}
impl Group {
    async fn shutdown(&self) -> bool {
        let mut confirmed = self.client.disconnect_confirmed().await;
        if let Some(jump) = &self.jump {
            confirmed &= jump.disconnect_confirmed().await;
        }
        confirmed
    }
}
#[derive(Debug)]
struct Consumer {
    group: Arc<Group>,
    released: AtomicBool,
    finished: tokio::sync::watch::Sender<Option<bool>>,
    writer: Mutex<Option<Arc<cshell_ssh::SshTerminalWriter>>>,
}
impl Consumer {
    fn start_release(&self) {
        let last = {
            let mut count = self
                .group
                .consumers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.released.swap(true, Ordering::AcqRel) {
                return;
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
            self.finished.send_replace(Some(true));
            return;
        }
        let group = self.group.clone();
        let finished = self.finished.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            finished.send_replace(Some(false));
            return;
        };
        runtime.spawn(async move {
            let mut confirmed = true;
            if let Some(writer) = writer {
                confirmed &=
                    tokio::time::timeout(std::time::Duration::from_secs(2), writer.close())
                        .await
                        .is_ok_and(|result| result.is_ok());
            }
            if last {
                confirmed &= group.shutdown().await;
            }
            finished.send_replace(Some(confirmed));
        });
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        self.start_release();
    }
}
/// Cloning is an internal reference to the same consumer. `fork` creates a new
/// independently releasable consumer, bounded to 64 per SSH transport.
#[derive(Clone, Debug)]
pub(crate) struct TransportLease(Arc<Consumer>);
#[derive(Clone, Debug)]
pub(crate) struct WeakTransport(std::sync::Weak<Group>);
impl WeakTransport {
    pub(crate) fn acquire(&self) -> Result<TransportLease, &'static str> {
        let group = self.0.upgrade().ok_or("SSH 连接已释放")?;
        let mut count = group
            .consumers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *count == 0 || group.client.transport_closed() {
            return Err("SSH 连接已断开");
        }
        if *count >= 64 {
            return Err("SSH 连接已达到共享上限");
        }
        *count += 1;
        drop(count);
        Ok(TransportLease(Arc::new(Consumer {
            group,
            released: AtomicBool::new(false),
            finished: tokio::sync::watch::channel(None).0,
            writer: Mutex::new(None),
        })))
    }
    pub(crate) fn live(&self) -> bool {
        self.0.upgrade().is_some_and(|group| {
            !group.client.transport_closed()
                && *group
                    .consumers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    > 0
        })
    }
    pub(crate) fn client(&self) -> Option<Arc<SshConnection>> {
        self.0.upgrade().map(|group| group.client.clone())
    }
    pub(crate) fn consumers(&self) -> usize {
        self.0.upgrade().map_or(0, |group| {
            *group
                .consumers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
    }
}
impl TransportLease {
    pub(crate) fn new(connection: RoutedSshConnection) -> Self {
        Self(Arc::new(Consumer {
            group: Arc::new(Group {
                id: cshell_domain::SessionId::new(),
                client: Arc::new(connection.client),
                jump: connection.jump,
                consumers: Mutex::new(1),
                jump_setup: Arc::new(tokio::sync::Semaphore::new(2)),
            }),
            released: AtomicBool::new(false),
            finished: tokio::sync::watch::channel(None).0,
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
            finished: tokio::sync::watch::channel(None).0,
            writer: Mutex::new(None),
        })))
    }
    pub(crate) fn running(&self) -> bool {
        !self.0.released.load(Ordering::Acquire) && !self.0.group.client.transport_closed()
    }
    pub(crate) fn client(&self) -> Arc<SshConnection> {
        self.0.group.client.clone()
    }
    pub(crate) fn connection(&self) -> &SshConnection {
        &self.0.group.client
    }
    pub(crate) fn identity(&self) -> cshell_domain::SessionId {
        self.0.group.id
    }
    pub(crate) fn weak(&self) -> WeakTransport {
        WeakTransport(Arc::downgrade(&self.0.group))
    }
    pub(crate) async fn reserve_jump_setup(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, &'static str> {
        if !self.running() {
            return Err("跳板连接已断开");
        }
        let permit = self
            .0
            .group
            .jump_setup
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "跳板连接正在关闭")?;
        if !self.running() {
            return Err("跳板连接已断开");
        }
        Ok(permit)
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
        let _ = self.release_confirmed().await;
    }
    pub(crate) async fn release_confirmed(&self) -> bool {
        // The cleanup task survives cancellation; repeated callers await the same receipt.
        let mut finished = self.0.finished.subscribe();
        self.0.start_release();
        tokio::time::timeout(std::time::Duration::from_secs(25), async {
            loop {
                if let Some(confirmed) = *finished.borrow_and_update() {
                    return confirmed;
                }
                if finished.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests;
