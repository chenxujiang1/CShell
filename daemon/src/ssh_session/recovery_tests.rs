use super::*;
use cshell_domain::{SshAuthMethod, SshConnectionOptions, SshConnectionRecord};
use rand::rng;
use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
mod soak;

struct Fixture {
    _directory: tempfile::TempDir,
    session: SshSession,
    server: tokio::task::JoinHandle<()>,
    handles: tokio::sync::mpsc::UnboundedReceiver<russh::server::Handle>,
    connections: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<u8>>>,
    vault: Arc<crate::vault_control::VaultController>,
}

async fn fixture(change_host_key: bool, auto_reconnect: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    fixture_in(change_host_key, auto_reconnect, directory).await
}

async fn fixture_in(change_host_key: bool, auto_reconnect: bool, directory: tempfile::TempDir) -> Fixture {
    let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let host_public = key.public_key().to_openssh().unwrap();
    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let accepted_key = client_key.public_key().clone();
    let key_path = directory.path().join("id_ed25519");
    std::fs::write(
        &key_path,
        client_key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let known_hosts = directory.path().join("known_hosts");
    std::fs::write(
        &known_hosts,
        format!("[127.0.0.1]:{} {host_public}\n", address.port()),
    )
    .unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(Mutex::new(Vec::new()));
    let (send_handles, handles) = tokio::sync::mpsc::unbounded_channel();
    let server_count = Arc::clone(&connections);
    let server_received = Arc::clone(&received);
    let server = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let index = server_count.fetch_add(1, Ordering::AcqRel);
            let mut config = russh::server::Config::default();
            config.auth_rejection_time = Duration::from_millis(1);
            config.keys.push(if change_host_key && index > 0 {
                PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap()
            } else {
                key.clone()
            });
            let mut handler = super::tests::EchoServer::default();
            handler.accepted_public_key = Some(accepted_key.clone());
            handler.received_input = Some(Arc::clone(&server_received));
            let send_handles = send_handles.clone();
            tasks.spawn(async move {
                if let Ok(running) =
                    russh::server::run_stream(Arc::new(config), stream, handler).await
                {
                    let _ = send_handles.send(running.handle());
                    let _ = running.await;
                }
            });
        }
    });
    let target = SshConnectionRecord {
        profile_id: ProfileId::new(),
        host: "127.0.0.1".into(),
        port: address.port(),
        username: "cshell".into(),
        auth_method: SshAuthMethod::PrivateKey,
        private_key_path: Some(key_path.to_str().unwrap().into()),
        certificate_path: None,
        agent_backend: Default::default(),
        agent_identity: None,
        route: Default::default(),
        options: SshConnectionOptions {
            auto_reconnect,
            reconnect_delay_secs: 1,
            authentication_order: vec![cshell_domain::SshAuthStep::PrivateKey],
            ..SshConnectionOptions::new_profile()
        },
    };
    let plan =
        crate::ssh_route::SshConnectionPlan::new("recovery test".into(), target.clone(), &[])
            .unwrap();
    let vault = crate::vault_control::VaultController::new(directory.path().join("vault"));
    let connection = crate::ssh_route::connect_profile(&plan, &known_hosts, &vault)
        .await
        .unwrap_or_else(|failure| panic!("{}", failure.detail));
    let session = SshSession::from_connection_configured(
        SessionId::new(),
        plan.title.clone(),
        connection,
        TerminalSize::cells(24, 80),
        &directory.path().join("terminal.csjr"),
        16,
        "xterm-256color",
        true,
        Some((plan.clone(), known_hosts, Arc::clone(&vault))),
    )
    .await
    .unwrap()
    .with_profile_id(target.profile_id);
    Fixture {
        _directory: directory,
        session,
        server,
        handles,
        connections,
        received,
        vault,
    }
}

async fn wait(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("SSH recovery state timeout");
}

async fn disconnect(handle: russh::server::Handle) {
    handle
        .disconnect(
            russh::Disconnect::ByApplication,
            "network loss simulation".into(),
            "".into(),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unexpected_disconnect_recovers_same_session_without_replaying_input_and_logout_stays_closed()
 {
    let mut fixture = fixture(false, true).await;
    let id = fixture.session.id();
    let old_epoch = fixture.session.connection_epoch.load(Ordering::Acquire);
    let old_generation = fixture.session.generation();
    let first = fixture.handles.recv().await.unwrap();
    fixture
        .session
        .send_input(&InputAction::Text("before\n".into()))
        .unwrap();
    wait(|| fixture.received.lock().unwrap().as_slice() == b"before\n").await;
    disconnect(first).await;
    wait(|| fixture.session.summary().reconnecting && !fixture.session.running()).await;
    assert!(
        fixture
            .session
            .send_input(&InputAction::Text("must not send\n".into()))
            .is_err()
    );
    let _second = tokio::time::timeout(Duration::from_secs(5), fixture.handles.recv())
        .await
        .unwrap()
        .unwrap();
    wait(|| fixture.session.running() && !fixture.session.summary().reconnecting).await;
    assert_eq!(fixture.session.id(), id);
    assert_eq!(fixture.connections.load(Ordering::Acquire), 2);
    assert!(
        fixture
            .session
            .send_input_observed(
                &InputAction::Text("delayed IPC input\n".into()),
                Some(old_generation)
            )
            .is_err()
    );
    assert!(
        fixture
            .session
            .send_input_observed(&InputAction::Text("unfenced IPC input\n".into()), None)
            .is_err()
    );
    fixture
        .session
        .commands
        .try_send(Command::Input(old_epoch, 0, b"stale queued input\n".to_vec()))
        .unwrap();
    fixture
        .session
        .send_input(&InputAction::Text("after\n".into()))
        .unwrap();
    wait(|| fixture.received.lock().unwrap().as_slice() == b"before\nafter\n").await;
    fixture
        .session
        .send_input(&InputAction::Text("exit\n".into()))
        .unwrap();
    wait(|| !fixture.session.running()).await;
    assert!(fixture.session.summary().terminal_detail.contains("42"));
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(
        fixture.connections.load(Ordering::Acquire),
        2,
        "normal logout must not reopen a shell"
    );
    fixture.session.close().await.unwrap();
    fixture.server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_close_cancels_waiting_recovery() {
    let mut fixture = fixture(false, true).await;
    disconnect(fixture.handles.recv().await.unwrap()).await;
    wait(|| fixture.session.summary().reconnecting).await;
    fixture.session.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(fixture.connections.load(Ordering::Acquire), 1);
    assert!(!fixture.session.running());
    assert!(!fixture.session.summary().reconnecting);
    fixture.server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_budget_exhausts_and_host_key_change_stops_without_retrusting() {
    for changed_key in [false, true] {
        let mut fixture = fixture(changed_key, true).await;
        let first = fixture.handles.recv().await.unwrap();
        if !changed_key {
            fixture.server.abort();
        }
        disconnect(first).await;
        wait(|| {
            fixture
                .session
                .summary()
                .terminal_detail
                .contains("自动重连已停止")
        })
        .await;
        assert!(!fixture.session.running());
        if changed_key {
            assert_eq!(
                fixture.connections.load(Ordering::Acquire),
                2,
                "host trust failure must not be retried"
            );
            assert!(
                fixture
                    .session
                    .summary()
                    .terminal_detail
                    .contains("HostKey")
            );
        } else {
            assert!(fixture.session.summary().terminal_detail.contains("3/3"));
        }
        fixture.session.close().await.unwrap();
        fixture.server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_recovery_stays_closed_and_flapping_connections_respect_retry_limit() {
    for enabled in [false, true] {
        let mut fixture = fixture(false, enabled).await;
        let expected = if enabled { 4 } else { 1 };
        for index in 1..=expected {
            let handle = tokio::time::timeout(Duration::from_secs(5), fixture.handles.recv())
                .await
                .unwrap()
                .unwrap();
            wait(|| fixture.session.running() && !fixture.session.summary().reconnecting).await;
            assert_eq!(fixture.connections.load(Ordering::Acquire), index);
            disconnect(handle).await;
            if index == expected {
                wait(|| !fixture.session.running() && !fixture.session.summary().reconnecting)
                    .await;
            } else {
                wait(|| fixture.session.summary().reconnecting).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(1400)).await;
        assert_eq!(fixture.connections.load(Ordering::Acquire), expected);
        fixture.session.close().await.unwrap();
        fixture.server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vault_generation_change_cancels_recovery_before_another_authentication() {
    let mut fixture = fixture(false, true).await;
    disconnect(fixture.handles.recv().await.unwrap()).await;
    wait(|| {
        fixture
            .session
            .summary()
            .terminal_detail
            .contains("秒后自动重连")
    })
    .await;
    fixture.vault.invalidate();
    wait(|| {
        fixture
            .session
            .summary()
            .terminal_detail
            .contains("凭据库已锁定")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(fixture.connections.load(Ordering::Acquire), 1);
    fixture.session.close().await.unwrap();
    fixture.server.abort();
}
