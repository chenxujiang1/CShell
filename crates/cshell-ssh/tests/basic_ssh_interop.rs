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
    let Some(environment) = BasicInteropEnvironment::load() else {
        return;
    };
    let encoded_key = tokio::fs::read_to_string(&environment.private_key_path)
        .await
        .unwrap();
    let private_key = SshPrivateKey::decode_openssh(&encoded_key, None).unwrap();
    let client = RusshClient::connect_public_key(
        environment.address,
        environment.username,
        private_key,
        PinnedHostKey::sha256(environment.host_fingerprint),
    )
    .await
    .unwrap();
    let result = client
        .exec_with_pty(b"printf CSHELL_BASIC_INTEROP_OK", 24, 80)
        .await
        .unwrap();
    assert_eq!(result.exit_status, 0);
    assert!(
        result
            .stdout
            .windows(b"CSHELL_BASIC_INTEROP_OK".len())
            .any(|window| window == b"CSHELL_BASIC_INTEROP_OK")
    );
    client.disconnect().await.unwrap();
}
