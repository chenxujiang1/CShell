use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, client_handshake, envelope, features,
    read_envelope, transport, write_envelope,
};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[derive(Debug)]
struct ChildGuard(Child);

impl ChildGuard {
    fn spawn(runtime_root: &std::path::Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_cshelld"))
            .env("CSHELL_RUNTIME_DIR", runtime_root)
            .env("RUST_LOG", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("cshelld must start: {error}"));
        Self(child)
    }

    fn terminate(mut self) {
        self.terminate_inner();
    }

    fn terminate_inner(&mut self) {
        if self
            .0
            .try_wait()
            .unwrap_or_else(|error| panic!("daemon status must be readable: {error}"))
            .is_none()
        {
            self.0
                .kill()
                .unwrap_or_else(|error| panic!("daemon must terminate: {error}"));
        }
        self.0
            .wait()
            .unwrap_or_else(|error| panic!("daemon must be reaped: {error}"));
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate_inner();
    }
}

async fn wait_until_ready(child: &mut Child, paths: &RuntimePaths) -> DiscoveryRecord {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|error| panic!("daemon status must be readable: {error}"))
        {
            panic!("daemon exited before publishing discovery: {status}");
        }
        if let Ok(record) = DiscoveryRecord::load(paths) {
            #[cfg(windows)]
            let connected = transport::connect(&record.endpoint.to_string_lossy()).await;
            #[cfg(unix)]
            let connected = transport::connect(std::path::Path::new(&record.endpoint)).await;
            if let Ok(mut stream) = connected {
                let mut handshake = Handshake::new(
                    record.daemon_instance_id.to_vec(),
                    record.instance_token.to_vec(),
                );
                handshake.feature_bits = features::FULL_FRAME_RECOVERY | features::PRIORITY_STREAMS;
                if client_handshake(&mut stream, 1, handshake).await.is_ok() {
                    write_envelope(
                        &mut stream,
                        &Envelope {
                            request_id: 2,
                            deadline_unix_ms: 0,
                            payload: Some(envelope::Payload::SessionListRequest(
                                cshell_ipc::SessionListRequest {},
                            )),
                        },
                    )
                    .await
                    .unwrap_or_else(|error| panic!("session list request must send: {error}"));
                    let response = read_envelope(&mut stream).await.unwrap_or_else(|error| {
                        panic!("session list response must arrive: {error}")
                    });
                    assert!(matches!(
                        response.payload,
                        Some(envelope::Payload::SessionListResponse(_))
                    ));
                    return record;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon did not become ready before timeout"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn daemon_is_single_instance_and_restart_rotates_discovery_credentials() {
    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary runtime must be created: {error}"));
    let runtime_root = directory.path().join("runtime");
    let paths = RuntimePaths::prepare(runtime_root.clone())
        .unwrap_or_else(|error| panic!("runtime paths must prepare: {error}"));

    let mut first = ChildGuard::spawn(&runtime_root);
    let first_record = wait_until_ready(&mut first.0, &paths).await;

    let mut conflicting = ChildGuard::spawn(&runtime_root);
    let conflict_status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = conflicting
                .0
                .try_wait()
                .unwrap_or_else(|error| panic!("conflicting daemon status must read: {error}"))
            {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("conflicting daemon must exit promptly"));
    assert!(!conflict_status.success());
    drop(conflicting);

    first.terminate();
    let mut restarted = ChildGuard::spawn(&runtime_root);
    let restarted_record = wait_until_ready(&mut restarted.0, &paths).await;
    assert_ne!(
        first_record.daemon_instance_id,
        restarted_record.daemon_instance_id
    );
    assert_ne!(first_record.instance_token, restarted_record.instance_token);
    restarted.terminate();
}
