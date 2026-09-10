//! Platform-local transport. Endpoint ownership/token checks are performed during handshake.

#[cfg(unix)]
mod platform {
    use std::io;
    use std::path::Path;
    use tokio::net::{UnixListener, UnixStream};

    #[derive(Debug)]
    pub struct LocalListener(UnixListener);

    pub type LocalStream = UnixStream;
    pub type ClientStream = UnixStream;

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            use std::os::unix::fs::PermissionsExt;

            let listener = UnixListener::bind(path)?;
            if let Err(error) =
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            {
                let _result = std::fs::remove_file(path);
                return Err(error);
            }
            Ok(Self(listener))
        }

        pub async fn accept(&self) -> io::Result<LocalStream> {
            self.0.accept().await.map(|(stream, _address)| stream)
        }
    }

    pub async fn connect(path: &Path) -> io::Result<LocalStream> {
        UnixStream::connect(path).await
    }
}

#[cfg(windows)]
mod platform {
    use crate::windows_security::{PrivateSecurityDescriptor, validate_named_pipe};
    use std::io;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use tokio::time::{Duration, sleep};

    #[derive(Clone, Debug)]
    pub struct LocalListener {
        name: String,
    }

    pub type LocalStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    impl LocalListener {
        pub fn bind(name: impl Into<String>) -> io::Result<Self> {
            let name = name.into();
            if !name.starts_with(r"\\.\pipe\cshell-") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CShell pipe name must use the per-user prefix",
                ));
            }
            Ok(Self { name })
        }

        pub async fn accept(&self) -> io::Result<LocalStream> {
            // Keep this explicit even though Tokio currently defaults to rejecting
            // remote clients; an IPC dependency update must not silently expose the
            // daemon over the network redirector.
            let mut options = ServerOptions::new();
            options.reject_remote_clients(true);
            let server = {
                let mut security = PrivateSecurityDescriptor::current_user(false)?;
                let server = security.create_named_pipe(&options, &self.name)?;
                validate_named_pipe(&server)?;
                server
            };
            server.connect().await?;
            Ok(server)
        }
    }

    pub async fn connect(name: &str) -> io::Result<ClientStream> {
        let mut last_error = None;
        for _ in 0..100 {
            match ClientOptions::new().open(name) {
                Ok(client) => return Ok(client),
                Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {
                    last_error = Some(error);
                    sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::TimedOut, "named pipe did not become ready")
        }))
    }
}

pub use platform::*;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::{
        Envelope, Handshake, HandshakePolicy, client_handshake, envelope, features, read_envelope,
        server_handshake, write_envelope,
    };

    fn request() -> Envelope {
        Envelope {
            request_id: 7,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::Request(b"ping".to_vec())),
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn named_pipe_round_trip() {
        let name = format!(r"\\.\pipe\cshell-test-{}", std::process::id());
        let listener = super::LocalListener::bind(&name).unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let incoming = read_envelope(&mut stream).await.unwrap();
            write_envelope(&mut stream, &incoming).await.unwrap();
        });
        let mut client = super::connect(&name).await.unwrap();
        write_envelope(&mut client, &request()).await.unwrap();
        let response = read_envelope(&mut client).await.unwrap();
        assert_eq!(response.request_id, 7);
        server.await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn named_pipe_authenticated_handshake() {
        let name = format!(r"\\.\pipe\cshell-test-auth-{}", std::process::id());
        let listener = super::LocalListener::bind(&name).unwrap();
        let token = [0x5a; 32];
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            server_handshake(
                &mut stream,
                &HandshakePolicy::new(token, features::FULL_FRAME_RECOVERY),
            )
            .await
            .unwrap()
        });

        let mut client = super::connect(&name).await.unwrap();
        let mut handshake = Handshake::new(vec![9; 16], token.to_vec());
        handshake.feature_bits = features::FULL_FRAME_RECOVERY | features::CANCELLATION;
        let negotiated = client_handshake(&mut client, 41, handshake).await.unwrap();
        assert_eq!(negotiated.feature_bits, features::FULL_FRAME_RECOVERY);
        assert_eq!(server.await.unwrap(), negotiated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cshell.sock");
        let listener = super::LocalListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let incoming = read_envelope(&mut stream).await.unwrap();
            write_envelope(&mut stream, &incoming).await.unwrap();
        });
        let mut client = super::connect(&path).await.unwrap();
        write_envelope(&mut client, &request()).await.unwrap();
        let response = read_envelope(&mut client).await.unwrap();
        assert_eq!(response.request_id, 7);
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_authenticated_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cshell-auth.sock");
        let listener = super::LocalListener::bind(&path).unwrap();
        let token = [0x5a; 32];
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            server_handshake(
                &mut stream,
                &HandshakePolicy::new(token, features::FULL_FRAME_RECOVERY),
            )
            .await
            .unwrap()
        });

        let mut client = super::connect(&path).await.unwrap();
        let mut handshake = Handshake::new(vec![9; 16], token.to_vec());
        handshake.feature_bits = features::FULL_FRAME_RECOVERY | features::CANCELLATION;
        let negotiated = client_handshake(&mut client, 41, handshake).await.unwrap();
        assert_eq!(negotiated.feature_bits, features::FULL_FRAME_RECOVERY);
        assert_eq!(server.await.unwrap(), negotiated);
    }
}
