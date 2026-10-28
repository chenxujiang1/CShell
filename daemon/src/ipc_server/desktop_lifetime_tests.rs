use super::*;
use cshell_ipc::{Handshake, RuntimePaths, client_handshake, transport};
use std::time::Duration;

#[tokio::test]
async fn authenticated_desktop_lifetime_connections_control_only_managed_daemons() {
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::prepare(root.path().join("runtime")).unwrap();
    let endpoint = paths.endpoint(&[71; 16]);
    #[cfg(windows)]
    let listener = LocalListener::bind(endpoint.to_string_lossy().into_owned()).unwrap();
    #[cfg(unix)]
    let listener = LocalListener::bind(&std::path::PathBuf::from(&endpoint)).unwrap();
    let registry =
        Arc::new(crate::LocalSessionRegistry::new(root.path().join("journals"), 32).unwrap());
    let (shutdown, mut receiver) = watch::channel(false);
    let server = SessionIpcServer::new(
        listener,
        HandshakePolicy::with_instance_id([72; 32], [71; 16], features::DESKTOP_LIFETIME),
        SessionIpcService::new(registry),
    )
    .with_desktop_lifetime(shutdown.clone());
    let stop = receiver.clone();
    let task = tokio::spawn(async move { server.run_until(stop).await });
    #[cfg(windows)]
    let endpoint = endpoint.to_string_lossy().into_owned();
    #[cfg(unix)]
    let endpoint = std::path::PathBuf::from(endpoint);
    let mut rejected = transport::connect(&endpoint).await.unwrap();
    let mut bad = Handshake::new(vec![71; 16], vec![99; 32]);
    bad.feature_bits = features::DESKTOP_LIFETIME;
    assert!(client_handshake(&mut rejected, 1, bad).await.is_err());
    drop(rejected);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(
        !*receiver.borrow(),
        "unauthenticated connections must not arm auto-exit"
    );
    let mut first = transport::connect(&endpoint).await.unwrap();
    let mut second = transport::connect(&endpoint).await.unwrap();
    for stream in [&mut first, &mut second] {
        let mut handshake = Handshake::new(vec![71; 16], vec![72; 32]);
        handshake.feature_bits = features::DESKTOP_LIFETIME;
        assert_ne!(
            client_handshake(stream, 1, handshake)
                .await
                .unwrap()
                .feature_bits
                & features::DESKTOP_LIFETIME,
            0
        );
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(!*receiver.borrow());
    drop(second);
    tokio::time::timeout(Duration::from_secs(3), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(*receiver.borrow());
    assert!(task.await.unwrap().is_ok());
}
