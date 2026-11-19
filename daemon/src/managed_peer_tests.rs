//! Explicitly invoked against the isolated loopback OpenSSH fixture.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]
use crate::*;
use cshell_application::{
    TerminalUploadProtocol, TerminalUploadSnapshot, TerminalUploadStart, TerminalUploadState,
};
use cshell_domain::*;
use cshell_ipc::{SftpOperation, SftpRequest, SftpStatus};
use serde::Deserialize;
use std::{path::Path, sync::Arc, time::Duration};

#[derive(Deserialize)]
struct Fixture {
    a_port: u16,
    b_port: u16,
    a_public: String,
    b_public: String,
    a_private_windows: String,
    b_private_remote: String,
    a_files: String,
    b_files: String,
    b_audit: String,
    a_audit: String,
}
pub(crate) async fn fixture_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated CSHELL_MANAGED_SSH_FIXTURE; interrupts only owned foreground processes"]
async fn control_interrupt_real_openssh_foreground_and_mode_recovery() {
    let _fixture = fixture_guard().await;
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(std::env::var_os("CSHELL_MANAGED_SSH_FIXTURE").unwrap()).unwrap(),
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let verifier = cshell_ssh::KnownHostsVerifier::parse(
        &format!("[127.0.0.1]:{} {}\n", fixture.a_port, fixture.a_public),
        "127.0.0.1",
        fixture.a_port,
    )
    .unwrap();
    let key = cshell_ssh::SshPrivateKey::decode_private_key(
        &std::fs::read_to_string(&fixture.a_private_windows).unwrap(),
        None,
    )
    .unwrap();
    let session = SshSession::connect(
        SessionId::new(),
        "interrupt gate".into(),
        crate::ssh_session::SshConnect {
            username: "root".into(),
            verifier,
            authentication: cshell_ssh::SshAuthentication::PrivateKey(key),
        },
        TerminalSize::cells(24, 100),
        &root.path().join("journal"),
        64,
    )
    .await
    .unwrap();
    let command = |text: &str| {
        session
            .send_input(&InputAction::Compose {
                text: text.into(),
                append_enter: true,
            })
            .unwrap()
    };
    let key = InputAction::Key(KeyEvent {
        code: KeyCode::Character("c".into()),
        modifiers: Modifiers {
            ctrl: true,
            ..Default::default()
        },
        pressed: true,
        repeated: false,
    });
    command("printf '\\137\\137READY\\137\\137\\n'");
    screen(&session, "__READY__").await;
    command("PS1='__CONTROL_PROMPT__ '");
    for (number, child) in [(1, "exec sleep 600"), (2, "exec tail -f /dev/null")] {
        command(&format!(
            "sh -c 'printf \"\\137\\137RUN{number}\\137\\137\\n\"; {child}'"
        ));
        screen(&session, &format!("__RUN{number}__")).await;
        session.send_input(&key).unwrap();
        control_prompt(&session).await;
        command(&format!("printf '\\137\\137DONE{number}\\137\\137\\n'"));
        screen(&session, &format!("__DONE{number}__")).await;
        assert!(
            session.running(),
            "interrupt must preserve SSH and its parent shell"
        );
    }
    // Query-producing applications must read their replies and restore the TTY.
    let producer = r#"import os, select, termios, time
old = termios.tcgetattr(0)
raw = old.copy()
raw[3] &= ~(termios.ECHO | termios.ICANON)
termios.tcsetattr(0, termios.TCSANOW, raw)
print("\137\137RUN3\137\137", flush=True)
try:
    while True:
        print("\033[6noutput", flush=True)
        if select.select([0], [], [], 0.01)[0]: os.read(0, 4096)
except KeyboardInterrupt:
    pass
finally:
    until = time.monotonic() + 0.2
    while time.monotonic() < until:
        if select.select([0], [], [], 0.01)[0]: os.read(0, 4096)
    termios.tcflush(0, termios.TCIFLUSH)
    termios.tcsetattr(0, termios.TCSANOW, old)
print("\137\137STREAM_INTERRUPTED\137\137", flush=True)
"#;
    command(&format!(
        "python3 -u -c '{}'",
        producer.replace('\'', "'\\''")
    ));
    screen(&session, "__RUN3__").await;
    session.send_input(&key).unwrap();
    screen(&session, "__STREAM_INTERRUPTED__").await;
    control_prompt(&session).await;
    command("printf '\\137\\137DONE3\\137\\137\\n'");
    screen(&session, "__DONE3__").await;
    for (number, reset) in [(4, "\\033c"), (5, "\\033[>m"), (6, "\\033[>4;m")] {
        command(&format!(
            "printf '\\033[>4;2m{reset}\\137\\137RESET{number}\\137\\137\\n'"
        ));
        screen(&session, &format!("__RESET{number}__")).await;
        assert_eq!(
            session
                .history_snapshots()
                .latest()
                .unwrap()
                .terminal_modes
                .modify_other_keys,
            0
        );
        command(&format!(
            "sh -c 'printf \"\\137\\137RUN{number}\\137\\137\\n\"; exec sleep 600'"
        ));
        screen(&session, &format!("__RUN{number}__")).await;
        session.send_input(&key).unwrap();
        control_prompt(&session).await;
        command(&format!("printf '\\137\\137DONE{number}\\137\\137\\n'"));
        screen(&session, &format!("__DONE{number}__")).await;
    }
    command("printf '\\033[>4;2m\\137\\137EXTENDED\\137\\137\\n'");
    screen(&session, "__EXTENDED__").await;
    assert_eq!(
        session
            .history_snapshots()
            .latest()
            .unwrap()
            .terminal_modes
            .modify_other_keys,
        2
    );
    command("sh -c 'printf \"\\137\\137FALLBACK\\137\\137\\n\"; exec tail -f /dev/null'");
    screen(&session, "__FALLBACK__").await;
    session
        .send_input(&InputAction::Control(ControlAction::Interrupt))
        .unwrap();
    control_prompt(&session).await;
    command("printf '\\033[>m\\137\\137RECOVERED\\137\\137\\n'");
    screen(&session, "__RECOVERED__").await;
    assert!(session.running());
    session.close().await.unwrap();
}
async fn control_prompt(session: &SshSession) {
    for _ in 0..200 {
        if session.history_snapshots().latest().is_some_and(|frame| {
            frame.row(frame.cursor_row).is_some_and(|row| {
                row.iter()
                    .map(|cell| cell.character)
                    .collect::<String>()
                    .trim()
                    == "__CONTROL_PROMPT__"
            })
        }) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "SSH did not return to the owned parent shell: {:?}",
        session.history_snapshots().latest().map(|f| f
            .cells
            .iter()
            .map(|c| c.character)
            .collect::<String>())
    );
}
async fn screen(session: &SshSession, expected: &str) {
    for _ in 0..200 {
        if session.history_snapshots().latest().is_some_and(|frame| {
            frame
                .cells
                .iter()
                .map(|cell| cell.character)
                .collect::<String>()
                .contains(expected)
        }) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "terminal did not report {expected}: {:?}",
        session.history_snapshots().latest().map(|f| f
            .cells
            .iter()
            .map(|c| c.character)
            .collect::<String>())
    );
}
async fn session(
    plan: &crate::ssh_route::SshConnectionPlan,
    known: &Path,
    vault: &Arc<crate::vault_control::VaultController>,
    journal: &Path,
) -> SshSession {
    vault.prepare_plan(plan).await.unwrap();
    let connection = crate::ssh_route::connect_profile(plan, known, vault)
        .await
        .unwrap_or_else(|failure| panic!("{}", failure.detail));
    SshSession::from_connection_configured(
        SessionId::new(),
        plan.title.clone(),
        connection,
        TerminalSize::cells(24, 100),
        journal,
        64,
        "xterm-256color",
        false,
        None,
    )
    .await
    .unwrap()
    .with_profile_id(plan.target.profile_id)
    .with_clipboard_host(ClipboardHost::new(&plan.target.host, plan.target.port).unwrap())
    .with_clipboard_trust(plan)
    .unwrap()
}
async fn upload(
    service: &crate::sftp_ipc::SftpIpcService,
    registry: &SshSessionRegistry,
    source: SessionId,
    local: &Path,
) -> TerminalUploadSnapshot {
    let id = SessionId::new();
    let reply = service
        .handle(
            SftpRequest {
                operation: SftpOperation::TerminalUploadStart as i32,
                session_id: source.as_uuid().as_bytes().to_vec(),
                transfer_id: id.as_uuid().as_bytes().to_vec(),
                confirmed: true,
                document_json: serde_json::to_vec(&TerminalUploadStart {
                    paths: vec![local.to_str().unwrap().into()],
                    protocol: TerminalUploadProtocol::Sftp,
                    verification: SftpVerification::Standard,
                    confirm_before_start: false,
                })
                .unwrap(),
                ..Default::default()
            },
            registry,
        )
        .await;
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    for _ in 0..400 {
        let reply = service
            .handle(
                SftpRequest {
                    operation: SftpOperation::TerminalUploadGet as i32,
                    session_id: source.as_uuid().as_bytes().to_vec(),
                    transfer_id: id.as_uuid().as_bytes().to_vec(),
                    ..Default::default()
                },
                registry,
            )
            .await;
        assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
        let result: TerminalUploadSnapshot = serde_json::from_slice(&reply.document_json).unwrap();
        if result.state.finished() {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("upload did not complete");
}

#[tokio::test]
#[ignore = "requires CSHELL_MANAGED_SSH_FIXTURE and two isolated local OpenSSH servers"]
async fn managed_peer_real_openssh_terminal_upload_metrics_and_foreign_directory() {
    let _fixture = fixture_guard().await;
    tokio::time::timeout(Duration::from_secs(100), async {
        let fixture: Fixture = serde_json::from_slice(
            &std::fs::read(
                std::env::var_os("CSHELL_MANAGED_SSH_FIXTURE").expect("explicit fixture path"),
            )
            .unwrap(),
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let known = root.path().join("known_hosts");
        // Native CShell trusts A only. B trust and its private key live on A.
        std::fs::write(
            &known,
            format!("[127.0.0.1]:{} {}\n", fixture.a_port, fixture.a_public),
        )
        .unwrap();
        assert!(!Path::new(&fixture.b_private_remote).exists());
        let vault = crate::vault_control::VaultController::new(root.path().join("vault"));
        let a = SshConnectionRecord {
            profile_id: ProfileId::new(),
            host: "127.0.0.1".into(),
            port: fixture.a_port,
            username: "root".into(),
            auth_method: SshAuthMethod::PrivateKey,
            private_key_path: Some(fixture.a_private_windows.clone()),
            certificate_path: None,
            agent_backend: Default::default(),
            agent_identity: None,
            route: SshRoute::Direct,
            options: Default::default(),
        };
        let mut b = a.clone();
        b.profile_id = ProfileId::new();
        b.port = fixture.b_port;
        b.auth_method = SshAuthMethod::Password; // deliberately no local B password
        b.private_key_path = Some(fixture.b_private_remote.clone());
        b.route = SshRoute::Jump {
            profile_id: a.profile_id,
        };
        b.options.authentication_on_jump = true;
        assert!(!b.uses_password() && !b.uses_private_key());
        let a_plan = crate::ssh_route::SshConnectionPlan::new("A".into(), a.clone(), &[]).unwrap();
        let b_plan = crate::ssh_route::SshConnectionPlan::new(
            "B via A".into(),
            b.clone(),
            std::slice::from_ref(&a),
        )
        .unwrap();
        let registry = SshSessionRegistry::default();
        let a_session =
            registry.insert(session(&a_plan, &known, &vault, &root.path().join("a.csjr")).await);
        a_session
            .send_input(&InputAction::Text(
                "printf '__NODE_%s__\\n' \"$CSHELL_FIXTURE_SIDE\"\n".into(),
            ))
            .unwrap();
        screen(&a_session, "__NODE_A__").await;
        assert_eq!(
            a_session.current_remote_directory().await.as_deref(),
            Some(fixture.a_files.as_str())
        );
        // Even identical paths cannot make a foreign OSC authority become A.
        a_session
            .send_input(&InputAction::Text(format!(
                "printf '\\033]7;file://server-b{}\\007'; printf '__FOREIGN_%s__\\n' HOST_READY\n",
                fixture.a_files
            )))
            .unwrap();
        screen(&a_session, "__FOREIGN_HOST_READY__").await;
        assert!(a_session.current_remote_directory().await.is_none());
        // A terminal carrying B's OSC path must not authorize A-side upload.
        a_session
            .send_input(&InputAction::Text(format!(
                "printf '\\033]7;file://server-b{}\\007'; printf '__FOREIGN_%s__\\n' DIR_READY\n",
                fixture.b_files
            )))
            .unwrap();
        screen(&a_session, "__FOREIGN_DIR_READY__").await;
        assert!(a_session.current_remote_directory().await.is_none());
        let local = root.path().join("terminal-upload-proof.bin");
        let bytes = b"CSHELL MANAGED B UPLOAD\0\xff\n";
        std::fs::write(&local, bytes).unwrap();
        let uploads = crate::sftp_ipc::SftpIpcService::default();
        let rejected = upload(&uploads, &registry, a_session.id(), &local).await;
        assert_eq!(rejected.state, TerminalUploadState::Failed);
        assert_eq!(rejected.bytes, 0);
        let mut earlier = Vec::new();
        let log_reader = a_session.open_sftp().await.unwrap();
        log_reader
            .download(
                "/tmp/cshell-managed-peer-fixture/b.log",
                &mut earlier,
                &cshell_sftp::TransferControl::default(),
                |_| {},
            )
            .await
            .unwrap();
        log_reader.abort_channel();
        let earlier_auths = String::from_utf8_lossy(&earlier)
            .matches("Accepted publickey")
            .count();
        a_session.close().await.unwrap();

        let b_session =
            registry.insert(session(&b_plan, &known, &vault, &root.path().join("b.csjr")).await);
        let route = b_session.summary().ssh_target.unwrap().route;
        assert_eq!(route.len(), 2);
        assert!(route[0].ends_with(&format!(":{}", fixture.a_port)));
        assert!(route[1].ends_with(&format!(":{}", fixture.b_port)));
        assert_eq!(
            b_session.clipboard_trust().unwrap().host.port,
            fixture.b_port
        );
        let b_public = russh::keys::ssh_key::PublicKey::from_openssh(&fixture.b_public).unwrap();
        let expected = b_public
            .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
            .to_string();
        assert_eq!(
            b_session.clipboard_trust().unwrap().host_key_sha256,
            expected
        );
        b_session
            .send_input(&InputAction::Text(
                "printf '__NODE_%s__\\n' \"$CSHELL_FIXTURE_SIDE\"\n".into(),
            ))
            .unwrap();
        screen(&b_session, "__NODE_B__").await;
        assert_eq!(
            b_session.current_remote_directory().await.as_deref(),
            Some(fixture.b_files.as_str())
        );
        let result = upload(&uploads, &registry, b_session.id(), &local).await;
        assert_eq!(
            result.state,
            TerminalUploadState::Succeeded,
            "{}",
            result.detail
        );
        assert_eq!(result.directory, fixture.b_files);
        assert!(result.host.ends_with(&format!(":{}", fixture.b_port)));
        let sftp = b_session.open_sftp().await.unwrap();
        let mut downloaded = Vec::new();
        sftp.download(
            format!("{}/terminal-upload-proof.bin", fixture.b_files),
            &mut downloaded,
            &cshell_sftp::TransferControl::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(downloaded, bytes);
        assert!(
            sftp.metadata(
                &format!("{}/terminal-upload-proof.bin", fixture.a_files),
                cshell_sftp::SymlinkPolicy::NoFollow
            )
            .await
            .is_err()
        );
        let resources = crate::server_resources::Service::default()
            .sample(&registry, b_session.id())
            .await
            .unwrap();
        assert_eq!(resources.session, b_session.id());
        assert!(resources.memory_total_kib > 0);
        let mut audit = Vec::new();
        sftp.download(
            fixture.b_audit.clone(),
            &mut audit,
            &cshell_sftp::TransferControl::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert!(String::from_utf8_lossy(&audit).contains("CSHELL-RESOURCES-1"));
        assert!(!String::from_utf8_lossy(&audit).contains(&fixture.b_private_remote));
        let mut a_audit = Vec::new();
        sftp.download(
            fixture.a_audit.clone(),
            &mut a_audit,
            &cshell_sftp::TransferControl::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert!(String::from_utf8_lossy(&a_audit).contains("ProxyCommand=false"));
        let mut b_log = Vec::new();
        sftp.download(
            "/tmp/cshell-managed-peer-fixture/b.log",
            &mut b_log,
            &cshell_sftp::TransferControl::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&b_log)
                .matches("Accepted publickey")
                .count(),
            earlier_auths + 1,
            "terminal, SFTP and metrics must reuse one B authentication"
        );
        sftp.remove_entry(
            &format!("{}/terminal-upload-proof.bin", fixture.b_files),
            false,
        )
        .await
        .unwrap();
        sftp.abort_channel();
        b_session
            .send_input(&InputAction::Compose {
                text:
                    "sh -c 'printf \"\\137\\137B_FOLLOWING\\137\\137\\n\"; exec tail -f /dev/null'"
                        .into(),
                append_enter: true,
            })
            .unwrap();
        screen(&b_session, "__B_FOLLOWING__").await;
        b_session
            .send_input(&InputAction::Key(KeyEvent {
                code: KeyCode::Character("c".into()),
                modifiers: Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
                pressed: true,
                repeated: false,
            }))
            .unwrap();
        b_session
            .send_input(&InputAction::Compose {
                text: "printf '\\137\\137B_INTERRUPTED\\137\\137\\n'".into(),
                append_enter: true,
            })
            .unwrap();
        screen(&b_session, "__B_INTERRUPTED__").await;
        assert!(
            b_session.running(),
            "interrupt must preserve the managed B connection"
        );
        b_session.close().await.unwrap();
        let gateway = crate::ssh_route::connect_profile(&a_plan, &known, &vault)
            .await
            .unwrap_or_else(|failure| panic!("{}", failure.detail));
        let rejected = cshell_ssh::SshConnection::connect_from_jump(
            &gateway.client,
            &b,
            Some("SHA256:incorrect-captured-key"),
        )
        .await;
        assert!(
            rejected
                .expect_err("changed captured B identity must stop")
                .to_string()
                .contains("不一致")
        );
        // Modify ONLY this fixture's public known_hosts file; restore before assertions.
        let empty = gateway
            .client
            .exec_with_pty(": > /tmp/cshell-managed-peer-fixture/known-b", 8, 100)
            .await
            .unwrap();
        assert_eq!(empty.exit_status, 0);
        let unknown = cshell_ssh::SshConnection::connect_from_jump(&gateway.client, &b, None).await;
        let line = format!("[127.0.0.1]:{} {}\n", fixture.b_port, fixture.b_public);
        let command = format!(
            "printf %s '{}' > /tmp/cshell-managed-peer-fixture/known-b",
            line.replace('\'', "'\\''")
        );
        let restored = gateway.client.exec_with_pty(command, 8, 100).await.unwrap();
        assert_eq!(restored.exit_status, 0);
        assert!(
            unknown
                .expect_err("unknown B key must not be accepted")
                .to_string()
                .contains("尚未信任")
        );
        // P2 only: supply an authenticated A lease explicitly, without a P3 registry.
        let a_lease = crate::shared_ssh::TransportLease::new(gateway);
        let actual_a = a_lease.client();
        let b_client = cshell_ssh::SshConnection::connect_from_jump(a_lease.connection(), &b, None)
            .await
            .unwrap();
        let b_lease =
            crate::shared_ssh::TransportLease::new(crate::ssh_route::RoutedSshConnection {
                client: b_client,
                jump: Some(crate::ssh_route::JumpConnection::from_leases(vec![
                    a_lease.fork().unwrap(),
                ])),
            });
        assert!(b_lease.release_confirmed().await);
        let alive = a_lease
            .connection()
            .exec_with_pty("printf '__P2_A_%s__' \"$CSHELL_FIXTURE_SIDE\"", 8, 100)
            .await
            .unwrap();
        assert_eq!(alive.stdout, b"__P2_A_A__");
        let b_client = cshell_ssh::SshConnection::connect_from_jump(a_lease.connection(), &b, None)
            .await
            .unwrap();
        let b_lease =
            crate::shared_ssh::TransportLease::new(crate::ssh_route::RoutedSshConnection {
                client: b_client,
                jump: Some(crate::ssh_route::JumpConnection::from_leases(vec![
                    a_lease.fork().unwrap(),
                ])),
            });
        assert!(a_lease.release_confirmed().await);
        assert!(!actual_a.transport_closed());
        let alive = b_lease
            .connection()
            .exec_with_pty("printf '__P2_B_%s__' \"$CSHELL_FIXTURE_SIDE\"", 8, 100)
            .await
            .unwrap();
        assert_eq!(alive.stdout, b"__P2_B_B__");
        assert!(b_lease.release_confirmed().await);
        assert!(actual_a.transport_closed());
    })
    .await
    .unwrap();
}
