use super::*;
use std::error::Error;

const MARKER: &str = "SECRET-DIAGNOSTIC-MARKER";

#[test]
fn backend_errors_redact_display_debug_and_source_chains() {
    let errors = [
        SshError::Protocol(russh::Error::IO(std::io::Error::other(MARKER))),
        SshError::ForwardIo(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            MARKER,
        )),
        SshError::AgentAuthentication(MARKER.into()),
        SshError::AgentBackendsUnavailable(MARKER.into()),
        SshError::InvalidForwardConfig(MARKER.into()),
        SshError::Socks5(MARKER.into()),
        SshError::ForwardTask(MARKER.into()),
        SshError::KnownHosts(KnownHostsError::Io(std::io::Error::other(MARKER))),
        SshError::Sftp(cshell_sftp::SftpError::LocalIo(std::io::Error::other(
            MARKER,
        ))),
    ];
    for error in errors {
        assert!(!error.to_string().contains(MARKER));
        assert!(!format!("{error:?}").contains(MARKER));
        assert!(
            error.source().is_none(),
            "raw backend sources must stop at the adapter"
        );
        assert!(
            error.to_string().contains("SSH")
                || error.to_string().contains("SFTP")
                || error.to_string().contains("SOCKS5")
        );
    }
    let error = KnownHostsError::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        MARKER,
    ));
    assert!(error.to_string().contains("PermissionDenied"));
    assert!(!format!("{error:?}").contains(MARKER));
    assert!(error.source().is_none());
    assert!(
        SshError::Protocol(russh::Error::IO(std::io::Error::other(MARKER))).is_transport_failure()
    );
    assert!(!SshError::AgentAuthentication(MARKER.into()).is_transport_failure());
    assert!(
        KnownHostsError::InvalidEntry { line: 17 }
            .to_string()
            .contains("17")
    );
}

#[test]
fn terminal_event_diagnostics_hide_output_and_peer_exit_text() {
    let events = [
        TerminalEvent::Data {
            stream: TerminalDataStream::Stdout,
            data: MARKER.as_bytes().to_vec(),
        },
        TerminalEvent::ExitSignal {
            signal: MARKER.into(),
            core_dumped: false,
            message: MARKER.into(),
            language: MARKER.into(),
        },
    ];
    for event in &events {
        let diagnostic = format!("{event:?}");
        assert!(!diagnostic.contains(MARKER));
        assert!(!diagnostic.contains("83, 69, 67"));
    }
    assert!(format!("{:?}", events[0]).contains("byte_count"));
    assert!(matches!(&events[0], TerminalEvent::Data { data, .. } if data == MARKER.as_bytes()));
    assert_eq!(
        format!("{:?}", TerminalEvent::ExitStatus(42)),
        "ExitStatus(42)"
    );
}

#[tokio::test]
async fn host_key_scan_failure_does_not_echo_transport_payload() -> Result<(), Box<dyn Error>> {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    struct ErrorStream;
    impl AsyncRead for ErrorStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other(MARKER)))
        }
    }
    impl AsyncWrite for ErrorStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    let result =
        tokio::time::timeout(Duration::from_secs(5), scan_host_key_stream(ErrorStream)).await?;
    assert_eq!(
        result.err().ok_or("scan unexpectedly succeeded")?,
        "SSH host-key scan failed before receiving a key"
    );
    Ok(())
}
