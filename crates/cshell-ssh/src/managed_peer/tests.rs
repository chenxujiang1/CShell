#![allow(clippy::unwrap_used)]
use super::*;
#[test]
fn pins_exactly_one_reported_sha256_key_and_never_accepts_arbitrary_stderr() {
    let line = format!(
        "debug1: Server host key: ssh-ed25519 SHA256:{}\n",
        "a".repeat(43)
    );
    assert_eq!(
        fingerprint(line.as_bytes()),
        Some(format!("SHA256:{}", "a".repeat(43)))
    );
    assert!(fingerprint(format!("{line}{line}").as_bytes()).is_none());
    assert!(fingerprint(b"password=secret").is_none());
    assert!(fingerprint(b"debug1: Server host key: ssh-ed25519 SHA256:bad;command\n").is_none());
}
#[test]
fn target_tokens_cannot_be_commands_or_options() {
    for value in [
        "-oProxyCommand=x",
        "b;id",
        "$(id)",
        "b\n",
        "a@b",
        "",
        "b name",
    ] {
        assert!(!target_field(value));
    }
    for value in ["server-b", "10.0.0.2", "::1", "b.example"] {
        assert!(target_field(value));
    }
    assert_eq!(quote("/a'b"), "'/a'\\''b'");
}
#[test]
fn startup_errors_are_actionable_and_never_echo_server_diagnostics() {
    let changed = startup_error(b"PRIVATE-PROBE REMOTE HOST IDENTIFICATION HAS CHANGED");
    assert!(changed.to_string().contains("指纹变化"));
    let unknown = startup_error(b"PRIVATE-PROBE Host key verification failed");
    assert!(unknown.to_string().contains("尚未信任"));
    let denied = startup_error(b"PRIVATE-PROBE Permission denied");
    assert!(denied.to_string().contains("免密认证失败"));
    for value in [changed, unknown, denied, startup_error(b"PRIVATE-PROBE")] {
        assert!(!format!("{value} {value:?}").contains("PRIVATE-PROBE"));
    }
}
#[test]
fn terminal_ready_marker_keeps_unicode_banner_and_first_prompt() {
    let marker = b"\x1eCSHELL_B_READY_nonce\x1f";
    let before = "欢迎\r\n".as_bytes();
    let after = b"user@B$ ";
    let mut bytes = before.to_vec();
    bytes.extend_from_slice(&marker[..10]);
    assert!(split_ready(&bytes, marker).is_none());
    bytes.extend_from_slice(&marker[10..]);
    bytes.extend_from_slice(after);
    assert_eq!(
        split_ready(&bytes, marker),
        Some((before, after.as_slice()))
    );
    assert!(split_ready(&bytes, b"different").is_none());
}
