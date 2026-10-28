use super::*;
use tokio::io::AsyncReadExt;

/// A process-wide lease. Terminal/view clients may reconnect without releasing it.
#[derive(Debug)]
pub(crate) struct DesktopDaemonLifetime {
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl DesktopDaemonLifetime {
    pub fn start(config: &DesktopConnectionConfig) -> Result<Self, DesktopConnectionError> {
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        if matches!(config.source, DesktopEndpointSource::Explicit(_)) {
            // Explicit IPC endpoints belong to independently managed services.
            return Ok(Self {
                shutdown,
                worker: None,
            });
        }
        let config = config.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(DesktopConnectionError::Thread)?;
        let worker = std::thread::Builder::new()
            .name("cshell-desktop-lifetime".into())
            .spawn(move || runtime.block_on(run(config, receiver)))
            .map_err(DesktopConnectionError::Thread)?;
        Ok(Self {
            shutdown,
            worker: Some(worker),
        })
    }
}
impl Drop for DesktopDaemonLifetime {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn run(config: DesktopConnectionConfig, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut last_start: Option<std::time::Instant> = None;
    while !*shutdown.borrow() {
        let result = tokio::select! { biased;
            _ = shutdown.changed() => break,
            result = hold_lease(&config) => result,
        };
        match result {
            Ok(false) => {
                tracing::info!(
                    "daemon is independently managed; desktop lifetime lease not negotiated"
                );
                return;
            }
            Ok(true) => tracing::info!("desktop lifetime connection ended; reconnecting"),
            Err(error) => {
                if config.should_start_daemon_after(&error)
                    && last_start.is_none_or(|started| started.elapsed() >= Duration::from_secs(5))
                {
                    last_start = Some(std::time::Instant::now());
                    if let Err(error) = config.start_companion_daemon() {
                        tracing::warn!(%error, "desktop companion start failed");
                    }
                }
            }
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
}

async fn hold_lease(config: &DesktopConnectionConfig) -> Result<bool, DesktopConnectionError> {
    let resolved = config.resolve()?;
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        transport::connect(&resolved.endpoint),
    )
    .await
    .map_err(|_| DesktopConnectionError::IpcSetupTimeout)?
    .map_err(DesktopConnectionError::Connect)?;
    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::DESKTOP_LIFETIME;
    let negotiated = tokio::time::timeout(
        Duration::from_secs(5),
        client_handshake(&mut stream, 1, handshake),
    )
    .await
    .map_err(|_| DesktopConnectionError::IpcSetupTimeout)??;
    if negotiated.feature_bits & features::DESKTOP_LIFETIME == 0 {
        return Ok(false);
    }
    tracing::info!("desktop lifetime lease connected");
    let mut byte = [0];
    stream
        .read(&mut byte)
        .await
        .map_err(DesktopConnectionError::Connect)?;
    Ok(true)
}
