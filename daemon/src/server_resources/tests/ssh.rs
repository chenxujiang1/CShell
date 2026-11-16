use super::*;
use crate::ssh_session::SshConnect;
use cshell_domain::{InputAction, TerminalSize};
use russh::{
    Channel, ChannelId,
    server::{Auth, ChannelOpenHandle, Msg, Session},
};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Handler {
    channels: HashMap<ChannelId, Channel<Msg>>,
    execs: Arc<AtomicUsize>,
    input: Arc<Mutex<Vec<u8>>>,
    unconfirmed: bool,
}
impl russh::server::Handler for Handler {
    type Error = russh::Error;
    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == "test" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.input.lock().unwrap().extend_from_slice(data);
        session.data(channel, data.to_vec())?;
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        assert_eq!(
            command,
            format!("/bin/sh -c '{}'", SCRIPT.replace('\'', "'\\''")).as_bytes()
        );
        self.execs.fetch_add(1, Ordering::SeqCst);
        session.channel_success(channel)?;
        let handle = session.handle();
        let unconfirmed = self.unconfirmed;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            let _ = handle.data(channel, SAMPLE.as_bytes().to_vec()).await;
            if !unconfirmed {
                let _ = handle.exit_status_request(channel, 0).await;
            }
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
        Ok(())
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    registry: SshSessionRegistry,
    session: Arc<SshSession>,
    execs: Arc<AtomicUsize>,
    input: Arc<Mutex<Vec<u8>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(unconfirmed: bool) -> Self {
        let key = russh::keys::ssh_key::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let execs = Arc::new(AtomicUsize::new(0));
        let input = Arc::new(Mutex::new(Vec::new()));
        let handler = Handler {
            channels: HashMap::new(),
            execs: execs.clone(),
            input: input.clone(),
            unconfirmed,
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let config = russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            };
            let running = russh::server::run_stream(Arc::new(config), stream, handler)
                .await
                .unwrap();
            let _ = running.await;
        });
        let verifier = cshell_ssh::KnownHostsVerifier::parse(
            &format!("[127.0.0.1]:{} {public}\n", address.port()),
            "127.0.0.1",
            address.port(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let registry = SshSessionRegistry::default();
        let session = registry.insert(
            SshSession::connect(
                SessionId::new(),
                "resource test".into(),
                SshConnect {
                    username: "test".into(),
                    verifier,
                    authentication: cshell_ssh::SshAuthentication::Password("test".into()),
                },
                TerminalSize::cells(24, 80),
                &directory.path().join("terminal.csjr"),
                16,
            )
            .await
            .unwrap(),
        );
        Self {
            directory,
            registry,
            session,
            execs,
            input,
            server,
        }
    }
}

#[tokio::test]
async fn shares_sample_across_terminal_channels_without_touching_terminal_input() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fixture = Fixture::new(false).await;
        let source_id = SessionId::from_bytes(
            fixture
                .session
                .summary()
                .session_id
                .as_slice()
                .try_into()
                .unwrap(),
        );
        let other_id = SessionId::new();
        let other = fixture.registry.insert(
            fixture
                .session
                .duplicate_channel(
                    other_id,
                    TerminalSize::cells(24, 80),
                    &fixture.directory.path().join("other.csjr"),
                )
                .await
                .unwrap(),
        );
        let service = Service::default();
        let (a, b) = tokio::join!(
            service.sample(&fixture.registry, source_id),
            service.sample(&fixture.registry, other_id)
        );
        assert_eq!(a.unwrap().cpu_basis_points, Some(4000));
        assert_eq!(b.unwrap().session, other_id);
        assert_eq!(fixture.execs.load(Ordering::SeqCst), 1);
        assert!(fixture.input.lock().unwrap().is_empty());
        // A manual refresh during the small cooldown reuses the timestamped sample.
        service.sample(&fixture.registry, source_id).await.unwrap();
        assert_eq!(fixture.execs.load(Ordering::SeqCst), 1);
        fixture
            .session
            .send_input(&InputAction::Paste {
                text: "echo alive".into(),
                bracketed: false,
            })
            .unwrap();
        for _ in 0..100 {
            if !fixture.input.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(&*fixture.input.lock().unwrap(), b"echo alive");
        assert!(fixture.session.running());
        other.close().await.unwrap();
        fixture.session.close().await.unwrap();
        assert!(service.sample(&fixture.registry, source_id).await.is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn missing_exit_receipt_blocks_repeated_remote_work() {
    let fixture = Fixture::new(true).await;
    let id = SessionId::from_bytes(
        fixture
            .session
            .summary()
            .session_id
            .as_slice()
            .try_into()
            .unwrap(),
    );
    let service = Service::default();
    assert!(service.sample(&fixture.registry, id).await.is_err());
    let error = service.sample(&fixture.registry, id).await.unwrap_err();
    assert!(error.contains("结束状态无法确认"));
    assert_eq!(fixture.execs.load(Ordering::SeqCst), 1);
    fixture.session.close().await.unwrap();
}
