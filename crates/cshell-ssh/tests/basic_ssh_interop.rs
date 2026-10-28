#![allow(clippy::unwrap_used)]

use cshell_ssh::{PinnedHostKey, RusshClient, SshPrivateKey};

#[derive(Debug)]
struct BasicInteropEnvironment {
    address: std::net::SocketAddr,
    username: String,
    host_fingerprint: String,
    private_key_path: std::path::PathBuf,
}

impl BasicInteropEnvironment {
    fn load() -> Option<Self> {
        std::env::var_os("CSHELL_BASIC_SSH_INTEROP")?;
        Some(Self {
            address: std::env::var("CSHELL_BASIC_SSH_ADDRESS")
                .ok()?
                .parse()
                .ok()?,
            username: std::env::var("CSHELL_BASIC_SSH_USERNAME").ok()?,
            host_fingerprint: std::env::var("CSHELL_BASIC_SSH_HOST_FINGERPRINT").ok()?,
            private_key_path: std::env::var_os("CSHELL_BASIC_SSH_PRIVATE_KEY")?.into(),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_accepts_project_public_key_pty_and_exec() {
    if std::env::var_os("CSHELL_BASIC_SSH_INTEROP").is_none() {
        return;
    }
    let environment = BasicInteropEnvironment::load().unwrap_or_else(|| {
        eprintln!("::error title=Basic SSH interop stage::environment validation");
        panic!("enabled SSH interoperability test has incomplete environment");
    });
    let encoded_key = require_stage(
        tokio::fs::read_to_string(&environment.private_key_path).await,
        "read fixture key",
    );
    let private_key = require_stage(
        SshPrivateKey::decode_openssh(&encoded_key, None),
        "decode fixture key",
    );
    let client = require_stage(
        RusshClient::connect_public_key(
            environment.address,
            environment.username,
            private_key,
            PinnedHostKey::sha256(environment.host_fingerprint),
        )
        .await,
        "verified public-key authentication",
    );
    let result = require_stage(
        client
            .exec_with_pty(b"echo CSHELL_BASIC_INTEROP_OK", 24, 80)
            .await,
        "PTY exec request",
    );
    if result.exit_status != 0 {
        eprintln!("::error title=Basic SSH interop stage::PTY command exit status");
    }
    assert_eq!(result.exit_status, 0);
    let marker_received = result
        .stdout
        .windows(b"CSHELL_BASIC_INTEROP_OK".len())
        .any(|window| window == b"CSHELL_BASIC_INTEROP_OK");
    if !marker_received {
        eprintln!("::error title=Basic SSH interop stage::PTY stdout marker");
    }
    assert!(marker_received);
    require_stage(client.disconnect().await, "disconnect");
}

fn require_stage<T, E: std::fmt::Debug>(result: Result<T, E>, stage: &'static str) -> T {
    result.unwrap_or_else(|error| {
        // Only a fixed stage label reaches the public check annotation.
        eprintln!("::error title=Basic SSH interop stage::{stage}");
        panic!("SSH interoperability stage {stage} failed: {error:?}");
    })
}
