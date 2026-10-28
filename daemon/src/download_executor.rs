use crate::{
    ProfileIpcService,
    download_local::LocalDownload,
    ssh_route::{JumpConnection, SshConnectionPlan, connect_frozen_target},
};
use cshell_application::*;
use cshell_domain::HostId;
use cshell_sftp::{PinnedRead, PinnedReadContent, PinnedReadError};
use cshell_ssh::SshConnection;
use std::{fmt, sync::Arc, time::Duration};

#[derive(Debug)]
pub(crate) struct SshDownloadExecutor(pub(crate) Arc<ProfileIpcService>);
#[async_trait::async_trait]
impl DownloadExecutor for SshDownloadExecutor {
    async fn local(
        &self,
        snapshot: &BatchDownloadSnapshot,
        host: HostId,
    ) -> Result<Box<dyn DownloadLocal>, DownloadReason> {
        Ok(Box::new(LocalDownload::new(snapshot, host)?))
    }
    async fn connect(
        &self,
        snapshot: &BatchDownloadSnapshot,
        host: HostId,
        cancel: TaskCancellation,
    ) -> Result<Box<dyn DownloadRemote>, DownloadReason> {
        let target = snapshot
            .targets
            .resolved_connection(host)
            .ok_or(DownloadReason::Invalid)?;
        let original = snapshot
            .targets
            .credential_template(host)
            .ok_or(DownloadReason::Invalid)?;
        let path = self
            .0
            .known_hosts_path()
            .map_err(|_| DownloadReason::Connection)?;
        let plan = SshConnectionPlan::new(String::new(), target, snapshot.targets.connections())
            .map_err(|_| DownloadReason::Invalid)?;
        let mut changes = self.0.vault.cancellation();
        let generation = *changes.borrow_and_update();
        let connected = tokio::select! { biased;
            _ = cancel.cancelled() => return Err(DownloadReason::Cancelled),
            _ = changes.changed() => return Err(DownloadReason::Connection),
            result = connect_frozen_target(&plan, original, &path, &self.0.vault) => result.map_err(|_| DownloadReason::Connection)?,
        };
        let mut remote = RemoteDownload {
            client: Arc::new(connected.client),
            jump: connected.jump,
            reader: None,
        };
        let opened = tokio::select! { biased;
            _ = cancel.cancelled() => Err(DownloadReason::Cancelled),
            _ = changes.changed() => Err(DownloadReason::Connection),
            result = remote.client.open_sftp() => result.map_err(|_| DownloadReason::Connection),
        };
        match opened {
            Ok(reader) => remote.reader = Some(reader.pinned_read()),
            Err(error) => {
                remote.close().await;
                return Err(error);
            }
        }
        if *changes.borrow() != generation {
            remote.close().await;
            return Err(DownloadReason::Connection);
        }
        Ok(Box::new(remote))
    }
}
struct RemoteDownload {
    client: Arc<SshConnection>,
    jump: Option<JumpConnection>,
    reader: Option<PinnedRead>,
}
impl fmt::Debug for RemoteDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteDownload").finish_non_exhaustive()
    }
}
fn reason(error: PinnedReadError) -> DownloadReason {
    match error {
        PinnedReadError::Invalid => DownloadReason::Invalid,
        PinnedReadError::Changed => DownloadReason::SourceChanged,
        PinnedReadError::Unavailable => DownloadReason::Transport,
    }
}
fn content(value: PinnedReadContent) -> DownloadContent {
    DownloadContent {
        size: value.size,
        sha256: value.sha256,
    }
}
impl RemoteDownload {
    fn reader(&mut self) -> Result<&mut PinnedRead, DownloadReason> {
        self.reader.as_mut().ok_or(DownloadReason::Connection)
    }
}
#[async_trait::async_trait]
impl DownloadRemote for RemoteDownload {
    async fn inspect(
        &mut self,
        source: &str,
        maximum: u64,
    ) -> Result<DownloadContent, DownloadReason> {
        self.reader()?
            .open(source, maximum)
            .await
            .map(content)
            .map_err(reason)
    }
    async fn read(&mut self, offset: u64, limit: usize) -> Result<Vec<u8>, DownloadReason> {
        self.reader()?.read(offset, limit).await.map_err(reason)
    }
    async fn recheck(&mut self) -> Result<DownloadContent, DownloadReason> {
        self.reader()?.digest().await.map(content).map_err(reason)
    }
    async fn close(&mut self) {
        if let Some(mut reader) = self.reader.take() {
            reader.close().await;
        }
        let _closed = self.client.disconnect().await;
        if let Some(jump) = self.jump.take() {
            jump.disconnect().await;
        }
    }
}
impl Drop for RemoteDownload {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let mut reader = self.reader.take();
            let jump = self.jump.take();
            runtime.spawn(async move {
                let _closed = tokio::time::timeout(Duration::from_secs(3), async {
                    if let Some(reader) = &mut reader {
                        reader.close().await;
                    }
                    let _closed = client.disconnect().await;
                    if let Some(jump) = jump {
                        jump.disconnect().await;
                    }
                })
                .await;
            });
        }
    }
}
