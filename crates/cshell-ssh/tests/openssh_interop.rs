#![allow(clippy::unwrap_used)]

#[cfg(unix)]
use cshell_ssh::AgentBackend;
use cshell_ssh::{PinnedHostKey, RusshClient, SshCertificate, SshPrivateKey};

#[derive(Debug)]
struct InteropEnvironment {
    address: std::net::SocketAddr,
    username: String,
    host_fingerprint: String,
    public_key_path: std::path::PathBuf,
    certificate_key_path: std::path::PathBuf,
    certificate_path: std::path::PathBuf,
}

impl InteropEnvironment {
    fn load() -> Option<Self> {
        std::env::var_os("CSHELL_OPENSSH_INTEROP")?;
        Some(Self {
            address: std::env::var("CSHELL_OPENSSH_ADDRESS").ok()?.parse().ok()?,
            username: std::env::var("CSHELL_OPENSSH_USERNAME").ok()?,
            host_fingerprint: std::env::var("CSHELL_OPENSSH_HOST_FINGERPRINT").ok()?,
            public_key_path: std::env::var_os("CSHELL_OPENSSH_PUBLIC_KEY")?.into(),
            certificate_key_path: std::env::var_os("CSHELL_OPENSSH_CERTIFICATE_KEY")?.into(),
            certificate_path: std::env::var_os("CSHELL_OPENSSH_CERTIFICATE")?.into(),
        })
    }

    fn pinned_host_key(&self) -> PinnedHostKey {
        PinnedHostKey::sha256(self.host_fingerprint.clone())
    }
}

async fn read_private_key(path: &std::path::Path) -> SshPrivateKey {
    let encoded = tokio::fs::read_to_string(path).await.unwrap();
    SshPrivateKey::decode_openssh(&encoded, None).unwrap()
}

async fn assert_openssh_exec(client: &RusshClient) {
    let result = client
        .exec_with_pty(b"printf CSHELL_OPENSSH_OK", 24, 80)
        .await
        .unwrap();
    assert_eq!(result.exit_status, 0);
    assert!(
        result
            .stdout
            .windows(b"CSHELL_OPENSSH_OK".len())
            .any(|window| window == b"CSHELL_OPENSSH_OK")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_openssh_accepts_project_public_key_authentication() {
    let Some(environment) = InteropEnvironment::load() else {
        return;
    };
    let private_key = read_private_key(&environment.public_key_path).await;
    let client = RusshClient::connect_public_key(
        environment.address,
        environment.username.clone(),
        private_key,
        environment.pinned_host_key(),
    )
    .await
    .unwrap();
    assert_openssh_exec(&client).await;
    client.disconnect().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_openssh_accepts_project_certificate_authentication() {
    let Some(environment) = InteropEnvironment::load() else {
        return;
    };
    let private_key = read_private_key(&environment.certificate_key_path).await;
    let encoded_certificate = tokio::fs::read_to_string(&environment.certificate_path)
        .await
        .unwrap();
    let certificate = SshCertificate::decode_openssh(&encoded_certificate).unwrap();
    let client = RusshClient::connect_certificate(
        environment.address,
        environment.username.clone(),
        private_key,
        certificate,
        environment.pinned_host_key(),
    )
    .await
    .unwrap();
    assert_openssh_exec(&client).await;
    client.disconnect().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_openssh_accepts_agent_held_certificate_identity() {
    let Some(environment) = InteropEnvironment::load() else {
        return;
    };
    let client = RusshClient::connect_agent_with_backend(
        environment.address,
        environment.username.clone(),
        environment.pinned_host_key(),
        AgentBackend::OpenSsh,
    )
    .await
    .unwrap();
    assert_openssh_exec(&client).await;
    client.disconnect().await.unwrap();
}
