use super::*;
use crate::daemon_connection::ResolvedDesktopEndpoint;
use cshell_domain::*;
use cshell_ipc::{DiscoveryRecord, HandshakePolicy, RuntimePaths, server_handshake};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct LostReply<S> {
    inner: S,
}
impl<S: AsyncRead + Unpin> AsyncRead for LostReply<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for LostReply<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "deliberately lost reply",
        )))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
async fn settled(connection: &HostGroupConnection) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while connection.view().busy {
        assert!(Instant::now() < deadline, "host worker did not settle");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_host_worker_blocks_lost_save_replies_and_freezes_exact_profile_versions()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("hosts.sqlite");
    let service = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    let profile = ProfileRecord {
        id: ProfileId::new(),
        name: "template".into(),
        kind: ProfileKind::Ssh,
        folder_id: None,
        tags: BTreeSet::new(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    };
    let ssh = SshConnectionRecord {
        options: Default::default(),
        profile_id: profile.id,
        host: "template.test".into(),
        port: 22,
        username: "operator".into(),
        auth_method: SshAuthMethod::Password,
        private_key_path: None,
        certificate_path: None,
        agent_backend: SshAgentBackend::OpenSsh,
        agent_identity: None,
        route: SshRoute::Direct,
    };
    service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(profile.clone()),
                CatalogChange::UpsertSshConnection(ssh.clone()),
            ],
        )
        .await?;
    let host = HostRecord {
        id: HostId::new(),
        name: "original".into(),
        address: "host.test".into(),
        port: 2222,
        profile_id: profile.id,
        tags: BTreeSet::from(["web".into()]),
        labels: BTreeMap::from([("env".into(), "prod".into())]),
        platform_hint: HostPlatformHint::Linux,
    };
    let catalog = HostCatalog {
        hosts: vec![host.clone()],
        ..Default::default()
    };
    service.repository().save_hosts(0, &catalog).await?;
    let registry = Arc::new(LocalSessionRegistry::new(
        directory.path().join("journals"),
        4,
    )?);
    let ipc = Arc::new(
        SessionIpcService::new(Arc::clone(&registry))
            .with_profiles(Arc::new(ProfileIpcService::new(service.into_repository()))),
    );
    let paths = RuntimePaths::prepare(directory.path().join("runtime"))?;
    let discovery = DiscoveryRecord::generate(&paths);
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\cshell-host-fixture-{}", ConnectionId::new());
    #[cfg(unix)]
    let endpoint = directory.path().join("hosts.sock");
    let listener = transport::LocalListener::bind(&endpoint)?;
    let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
        endpoint,
        instance_token: discovery.instance_token,
        daemon_instance_id: discovery.daemon_instance_id,
    });
    let policy = HandshakePolicy::with_instance_id(
        discovery.instance_token,
        discovery.daemon_instance_id,
        features::HOST_GROUP_CONTROL,
    );
    let server = tokio::spawn(async move {
        for step in 0..8 {
            let mut stream = listener.accept().await?;
            let negotiated = server_handshake(&mut stream, &policy).await?;
            if step == 1 {
                let _expected_failure = ipc
                    .serve_connection_with_features(
                        LostReply { inner: stream },
                        negotiated.feature_bits,
                    )
                    .await;
            } else {
                ipc.serve_connection_with_features(stream, negotiated.feature_bits)
                    .await?;
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let connection = HostGroupConnection::start(config)?;
    settled(&connection).await;
    assert!(connection.view().writable);
    let mut updated = host.clone();
    updated.name = "saved even without reply".into();
    assert!(connection.change((1, 1), HostCatalogChange::UpsertHost(updated.clone())));
    settled(&connection).await;
    assert!(!connection.view().writable);
    assert!(!connection.change((1, 1), HostCatalogChange::RemoveHost(host.id)));
    let observer = ProfileService::new(SqliteProfileRepository::open(&path).await?);
    assert_eq!(
        observer
            .repository()
            .load_hosts()
            .await?
            .ok_or("missing saved host")?
            .catalog
            .hosts,
        vec![updated.clone()]
    );
    assert!(connection.reload());
    settled(&connection).await;
    assert_eq!(
        connection
            .view()
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.revision),
        Some(2)
    );
    assert!(!connection.change((1, 1), HostCatalogChange::RemoveHost(host.id)));
    let selection = HostSelection::Selector {
        expression: "tag:web AND env:prod".into(),
    };
    assert!(connection.query(selection.clone(), false));
    settled(&connection).await;
    assert_eq!(
        connection
            .view()
            .preview
            .as_ref()
            .map(|preview| preview.hosts.clone()),
        Some(vec![updated])
    );
    assert!(connection.query(selection.clone(), true));
    settled(&connection).await;
    let frozen = connection.view().frozen.ok_or("missing frozen targets")?;
    assert_eq!((frozen.host_revision(), frozen.profile_revision()), (2, 1));
    assert_eq!(
        frozen
            .resolved_connection(host.id)
            .ok_or("missing endpoint")?
            .host,
        "host.test"
    );
    let mut changed = ssh;
    changed.username = "changed".into();
    observer
        .apply_batch(1, &[CatalogChange::UpsertSshConnection(changed)])
        .await?;
    assert!(connection.query(selection.clone(), false));
    settled(&connection).await;
    assert!(!connection.view().writable);
    assert!(connection.view().preview.is_none());
    assert!(connection.reload());
    settled(&connection).await;
    assert!(connection.query(selection, true));
    settled(&connection).await;
    let refreshed = connection
        .view()
        .frozen
        .ok_or("missing refreshed snapshot")?;
    assert_eq!(refreshed.profile_revision(), 2);
    assert_eq!(refreshed.connections()[0].username, "changed");
    assert_eq!(frozen.connections()[0].username, "operator");
    assert!(registry.list()?.is_empty());
    drop(connection);
    server.await?.map_err(|error| error.to_string())?;
    observer.into_repository().close().await;
    Ok(())
}
