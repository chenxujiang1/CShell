use cshell_domain::TerminalSize;
use cshell_ipc::{
    DiscoveryPublication, DiscoveryRecord, HandshakePolicy, RuntimePaths, SingleInstanceGuard,
    features, transport::LocalListener,
};
use cshell_local::LocalProfile;
use cshell_ssh::{RusshProvider, SshProvider};
use cshell_storage::SqliteProfileRepository;
use cshelld::{
    LocalSessionRegistry, ProfileIpcService, SessionExitMonitor, SessionIpcServer,
    SessionIpcService,
};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    if let Some(code) = cshell_local::run_pty_guardian_from_args()? {
        std::process::exit(code);
    }
    run_daemon()
}

#[tokio::main]
async fn run_daemon() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let provider = RusshProvider::default();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        ssh_provider = provider.name(),
        capabilities = ?provider.capabilities(),
        "cshelld Phase 0 service skeleton initialized"
    );

    let explicit_endpoint = std::env::var_os("CSHELL_IPC_ENDPOINT");
    let runtime_paths = explicit_endpoint
        .is_none()
        .then(RuntimePaths::for_current_user)
        .transpose()?;
    let _single_instance = runtime_paths
        .as_ref()
        .map(SingleInstanceGuard::acquire)
        .transpose()?;
    let discovery_record = runtime_paths.as_ref().map(DiscoveryRecord::generate);
    let endpoint = explicit_endpoint
        .or_else(|| {
            discovery_record
                .as_ref()
                .map(|record| record.endpoint.clone())
        })
        .ok_or("daemon endpoint initialization failed")?;
    let token = match &discovery_record {
        Some(record) => record.instance_token,
        None => required_hex::<32>("CSHELL_INSTANCE_TOKEN_HEX")?,
    };
    let daemon_instance_id = match &discovery_record {
        Some(record) => record.daemon_instance_id,
        None => required_hex::<16>("CSHELL_DAEMON_INSTANCE_ID_HEX")?,
    };
    let journal_root = std::env::var_os("CSHELL_JOURNAL_ROOT")
        .map(PathBuf::from)
        .or_else(|| runtime_paths.as_ref().map(RuntimePaths::journal_root))
        .ok_or("CSHELL_JOURNAL_ROOT is required with an explicit daemon endpoint")?;
    #[cfg(unix)]
    let registry = Arc::new(LocalSessionRegistry::new_with_guardian(
        journal_root,
        256,
        std::env::current_exe()?,
    )?);
    #[cfg(windows)]
    let registry = Arc::new(LocalSessionRegistry::new(journal_root, 256)?);

    if std::env::var_os("CSHELL_START_LOCAL").is_some_and(|value| value == "1") {
        let attachment = registry.spawn_local(
            &LocalProfile::platform_default(),
            TerminalSize::cells(24, 80),
        )?;
        info!(session_id = %attachment.session_id(), "default local terminal registered");
    }

    #[cfg(unix)]
    let _socket_cleanup = if runtime_paths.is_some() {
        let path = PathBuf::from(&endpoint);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Some(UnixSocketCleanup(path))
    } else {
        None
    };

    #[cfg(windows)]
    let listener = LocalListener::bind(endpoint.to_string_lossy().into_owned())?;
    #[cfg(unix)]
    let listener = LocalListener::bind(&PathBuf::from(&endpoint))?;

    let profile_repository = SqliteProfileRepository::open(profile_database_path()?).await?;
    let profiles = Arc::new(ProfileIpcService::new(profile_repository));
    let server = SessionIpcServer::new(
        listener,
        HandshakePolicy::with_instance_id(
            token,
            daemon_instance_id,
            features::FULL_FRAME_RECOVERY
                | features::PRIORITY_STREAMS
                | features::LOG_PAGING
                | features::TERMINAL_CONTROL
                | features::HISTORY_SEARCH
                | features::PROFILE_CONTROL
                | features::SSH_PROFILE_TARGET
                | features::SSH_PROFILE_SESSION
                | features::SSH_PROFILE_AUTH
                | features::SSH_HOST_KEY_IMPORT
                | features::SSH_PROFILE_ROUTE
                | features::LOCAL_PROFILE
                | features::LOCAL_LAUNCH_OPTIONS
                | features::SSH_SESSION_STATUS,
        ),
        SessionIpcService::new(Arc::clone(&registry)).with_profiles(profiles),
    );
    let _discovery_publication = runtime_paths
        .as_ref()
        .zip(discovery_record.as_ref())
        .map(|(paths, record)| DiscoveryPublication::publish(paths, record))
        .transpose()?;
    let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
    let signal_shutdown = shutdown_sender.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _result = signal_shutdown.send(true);
        }
    });
    let exit_monitor = SessionExitMonitor::new(registry);
    let monitor_shutdown = shutdown_receiver.clone();
    let mut monitor_task =
        tokio::spawn(async move { exit_monitor.run_until(monitor_shutdown).await });
    info!(endpoint = %endpoint.to_string_lossy(), "daemon IPC server listening");
    let mut server_task = Box::pin(server.run_until(shutdown_receiver));
    let (stats, reaped_sessions) = tokio::select! {
        server_result = &mut server_task => {
            let _result = shutdown_sender.send(true);
            let reaped_sessions = monitor_task.await??;
            (server_result?, reaped_sessions)
        }
        monitor_result = &mut monitor_task => {
            let reaped_sessions = monitor_result??;
            let _result = shutdown_sender.send(true);
            (server_task.await?, reaped_sessions)
        }
    };
    info!(reaped_sessions, "local session exit monitor stopped");
    info!(?stats, "daemon IPC server stopped");
    Ok(())
}

#[cfg(unix)]
#[derive(Debug)]
struct UnixSocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for UnixSocketCleanup {
    fn drop(&mut self) {
        let _result = std::fs::remove_file(&self.0);
    }
}

fn required_hex<const N: usize>(name: &'static str) -> Result<[u8; N], Box<dyn Error>> {
    let value = std::env::var(name).map_err(|_| format!("{name} is required"))?;
    if value.len() != N * 2 {
        return Err(format!("{name} must contain exactly {N} hexadecimal bytes").into());
    }
    let mut decoded = [0_u8; N];
    for (index, slot) in decoded.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&value[start..start + 2], 16)
            .map_err(|_| format!("{name} contains invalid hexadecimal data"))?;
    }
    Ok(decoded)
}

fn profile_database_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = std::env::var_os("CSHELL_PROFILE_DB") {
        return Ok(PathBuf::from(path));
    }
    if let Some(root) = std::env::var_os("CSHELL_DATA_DIR") {
        return Ok(PathBuf::from(root).join("cshell.db"));
    }
    #[cfg(windows)]
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA is required for Profile storage")?
        .join("CShell");
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is required for Profile storage")?
        .join("Library")
        .join("Application Support")
        .join("CShell");
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .ok_or("HOME or XDG_DATA_HOME is required for Profile storage")?
        .join("cshell");
    Ok(root.join("cshell.db"))
}
