//! P2 tests supplied leases; no P3 pool is involved.
#![allow(clippy::unwrap_used)]
use super::*;
use crate::ssh_route::{JumpConnection, RoutedSshConnection};
pub(super) struct Endpoint {
    pub(super) verifier: cshell_ssh::KnownHostsVerifier,
    pub(super) known_line: String,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Endpoint {
    pub(super) async fn new() -> Self {
        let key = russh::keys::ssh_key::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let verifier = cshell_ssh::KnownHostsVerifier::parse(
            &format!("[127.0.0.1]:{port} {public}\n"),
            "127.0.0.1",
            port,
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let config = config.clone();
                tasks.spawn(async move {
                    if let Ok(connection) = russh::server::run_stream(
                        config,
                        stream,
                        crate::ssh_session::tests::EchoServer::default(),
                    )
                    .await
                    {
                        let _ = connection.await;
                    }
                });
            }
        });
        Self {
            verifier,
            server,
            known_line: format!("[127.0.0.1]:{port} {public}\n"),
        }
    }
    pub(super) async fn connection(&self) -> SshConnection {
        SshConnection::connect_known_hosts(
            "cshell".into(),
            cshell_ssh::SshAuthentication::Password("phase1".into()),
            self.verifier.clone(),
        )
        .await
        .unwrap()
    }
    pub(super) async fn lease(&self) -> TransportLease {
        TransportLease::new(RoutedSshConnection {
            client: self.connection().await,
            jump: None,
        })
    }
}
async fn echo(connection: &SshConnection) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut terminal = connection.open_terminal(24, 80).await.unwrap();
        terminal
            .send_input(b"p2-keep-alive\n".to_vec())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        while let Some(event) = terminal.next_event().await {
            if let cshell_ssh::TerminalEvent::Data { data, .. } = event {
                bytes.extend_from_slice(&data);
                if bytes
                    .windows(b"ACK:p2-keep-alive\n".len())
                    .any(|value| value == b"ACK:p2-keep-alive\n")
                {
                    terminal.close().await.unwrap();
                    return;
                }
                assert!(bytes.len() <= 8192);
            }
        }
        panic!("live connection did not echo input");
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn closing_b_releases_only_its_borrowed_a_and_other_b_survives() {
    let endpoint = Endpoint::new().await;
    let a = endpoint.lease().await;
    let b1 = TransportLease::new(RoutedSshConnection {
        client: endpoint.connection().await,
        jump: Some(JumpConnection::from_leases(vec![a.fork().unwrap()])),
    });
    let b2 = TransportLease::new(RoutedSshConnection {
        client: endpoint.connection().await,
        jump: Some(JumpConnection::from_leases(vec![a.fork().unwrap()])),
    });
    assert!(b1.release_confirmed().await);
    echo(a.connection()).await;
    echo(b2.connection()).await;
    assert!(b2.release_confirmed().await);
    echo(a.connection()).await;
    assert!(a.release_confirmed().await);
    assert!(a.connection().transport_closed());
}
#[tokio::test]
async fn closing_original_a_retains_b_until_final_consumer_and_cannot_resurrect() {
    let endpoint = Endpoint::new().await;
    let a = endpoint.lease().await;
    let actual_a = a.client();
    let b = TransportLease::new(RoutedSshConnection {
        client: endpoint.connection().await,
        jump: Some(JumpConnection::from_leases(vec![a.fork().unwrap()])),
    });
    assert!(a.release_confirmed().await);
    assert!(a.fork().is_err());
    echo(&actual_a).await;
    echo(b.connection()).await;
    assert!(b.release_confirmed().await);
    assert!(actual_a.transport_closed());
    assert!(b.fork().is_err());
}
#[tokio::test]
async fn dropped_b_jump_lease_and_concurrent_release_do_not_disconnect_original() {
    let endpoint = Endpoint::new().await;
    let a = endpoint.lease().await;
    let borrowed = a.fork().unwrap();
    let observed = borrowed.clone();
    drop(JumpConnection::from_leases(vec![borrowed]));
    assert!(observed.release_confirmed().await);
    echo(a.connection()).await;
    let left = a.clone();
    let right = a.clone();
    let (one, two) = tokio::join!(left.release_confirmed(), right.release_confirmed());
    assert!(one && two);
    assert!(a.release_confirmed().await);
    assert!(a.connection().transport_closed());
}
#[tokio::test]
async fn cancelled_release_waiter_still_closes_last_transport_and_preserves_receipt() {
    let endpoint = Endpoint::new().await;
    let a = endpoint.lease().await;
    let closing = a.clone();
    let waiter = tokio::spawn(async move { closing.release_confirmed().await });
    while !a.0.released.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    waiter.abort();
    assert!(a.release_confirmed().await);
    assert!(a.connection().transport_closed());
}
