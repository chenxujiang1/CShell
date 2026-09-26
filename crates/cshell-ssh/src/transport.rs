//! Bounded unauthenticated SOCKS5 (RFC 1928) and HTTP CONNECT (RFC 9110) transports.
use crate::{SSH_CONNECT_TIMEOUT, SshError};
use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub trait SshStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> SshStream for T {}
pub type SshTransport = Box<dyn SshStream>;

#[derive(Clone, Copy, Debug)]
pub enum ProxyProtocol {
    Socks5,
    HttpConnect,
}

pub async fn open_tcp_stream(host: &str, port: u16) -> Result<SshTransport, SshError> {
    if host.is_empty() || port == 0 {
        return Err(SshError::ProxyRejected("invalid transport endpoint"));
    }
    let stream = tokio::time::timeout(SSH_CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| SshError::ConnectionTimeout)?
        .map_err(|error| SshError::Protocol(russh::Error::IO(error)))?;
    Ok(Box::new(stream))
}

pub async fn open_proxy_stream(
    protocol: ProxyProtocol,
    proxy_host: &str,
    proxy_port: u16,
    host: &str,
    port: u16,
) -> Result<SshTransport, SshError> {
    let mut stream = open_tcp_stream(proxy_host, proxy_port).await?;
    tokio::time::timeout(SSH_CONNECT_TIMEOUT, async {
        match protocol {
            ProxyProtocol::Socks5 => socks5(&mut stream, host, port).await,
            ProxyProtocol::HttpConnect => http_connect(&mut stream, host, port).await,
        }
    })
    .await
    .map_err(|_| SshError::ConnectionTimeout)??;
    Ok(stream)
}
fn io_error(error: std::io::Error) -> SshError {
    SshError::Protocol(russh::Error::IO(error))
}

async fn socks5<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
) -> Result<(), SshError> {
    if port == 0
        || host.is_empty()
        || !host.is_ascii()
        || host.chars().any(char::is_whitespace)
        || host.chars().any(char::is_control)
    {
        return Err(SshError::ProxyRejected("invalid SOCKS5 target"));
    }
    stream.write_all(&[5, 1, 0]).await.map_err(io_error)?;
    let mut method = [0; 2];
    stream.read_exact(&mut method).await.map_err(io_error)?;
    if method != [5, 0] {
        return Err(SshError::ProxyRejected(
            "SOCKS5 proxy requires unsupported authentication",
        ));
    }
    let mut request = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let length = u8::try_from(host.len())
                .map_err(|_| SshError::ProxyRejected("SOCKS5 hostname exceeds 255 bytes"))?;
            request.extend_from_slice(&[3, length]);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(io_error)?;
    let mut header = [0; 4];
    stream.read_exact(&mut header).await.map_err(io_error)?;
    if header[0..3] != [5, 0, 0] {
        return Err(SshError::ProxyRejected(
            "SOCKS5 CONNECT was refused or malformed",
        ));
    }
    let length = match header[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await.map_err(io_error)?),
        _ => return Err(SshError::ProxyRejected("invalid SOCKS5 reply address")),
    };
    if length == 0 {
        return Err(SshError::ProxyRejected("empty SOCKS5 reply address"));
    }
    let mut tail = vec![0; length + 2];
    stream.read_exact(&mut tail).await.map_err(io_error)?;
    Ok(())
}

async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
) -> Result<(), SshError> {
    if port == 0
        || host.is_empty()
        || host.len() > 253
        || !host.is_ascii()
        || host
            .bytes()
            .any(|byte| byte <= 32 || byte == 127 || b"/?#@[]".contains(&byte))
    {
        return Err(SshError::ProxyRejected("invalid HTTP CONNECT target"));
    }
    let authority = if matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
        format!("[{host}]:{port}")
    } else {
        if host.contains(':') {
            return Err(SshError::ProxyRejected("invalid HTTP CONNECT hostname"));
        }
        format!("{host}:{port}")
    };
    stream
        .write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await
        .map_err(io_error)?;
    let mut response = Vec::with_capacity(256);
    // Read exactly through the header boundary; buffered SSH banner bytes must remain in the stream.
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() >= 16 * 1024 {
            return Err(SshError::ProxyRejected(
                "HTTP CONNECT response headers exceed 16 KiB",
            ));
        }
        response.push(stream.read_u8().await.map_err(io_error)?);
    }
    let status = response
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let mut parts = status.split(|byte| *byte == b' ');
    let version = parts.next().unwrap_or_default();
    let code = parts.next().unwrap_or_default();
    if !matches!(version, b"HTTP/1.1" | b"HTTP/1.0")
        || code.len() != 3
        || code[0] != b'2'
        || !code.iter().all(u8::is_ascii_digit)
    {
        return Err(SshError::ProxyRejected(
            "HTTP CONNECT refused or requires unsupported authentication",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn socks5_fragmented_replies_preserve_the_tunnel_banner_for_all_address_types() {
        for (host, address) in [
            ("127.0.0.1", vec![1, 127, 0, 0, 1]),
            ("::1", {
                let mut value = vec![4];
                value.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
                value
            }),
            ("target.internal", {
                let mut value = vec![3, 15];
                value.extend_from_slice(b"target.internal");
                value
            }),
        ] {
            let (mut client, mut server) = tokio::io::duplex(4096);
            let work = async {
                let mut greeting = [0; 3];
                server.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 1, 0]);
                for byte in [5, 0] {
                    server.write_u8(byte).await.unwrap();
                    tokio::task::yield_now().await;
                }
                let mut request = vec![0; 3 + address.len() + 2];
                server.read_exact(&mut request).await.unwrap();
                let mut expected = vec![5, 1, 0];
                expected.extend_from_slice(&address);
                expected.extend_from_slice(&2222u16.to_be_bytes());
                assert_eq!(request, expected);
                for byte in [5, 0, 0, 3, 4, b'h', b'o', b's', b't', 0, 22] {
                    server.write_u8(byte).await.unwrap();
                    tokio::task::yield_now().await;
                }
                server.write_all(b"SSH-BANNER").await.unwrap();
            };
            let (result, ()) = tokio::join!(socks5(&mut client, host, 2222), work);
            result.unwrap();
            let mut banner = [0; 10];
            client.read_exact(&mut banner).await.unwrap();
            assert_eq!(&banner, b"SSH-BANNER");
        }
    }
    #[tokio::test]
    async fn http_connect_preserves_ssh_banner_and_uses_bracketed_ipv6_authority() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let work = async {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(server.read_u8().await.unwrap());
            }
            assert_eq!(
                request,
                b"CONNECT [::1]:2222 HTTP/1.1\r\nHost: [::1]:2222\r\n\r\n"
            );
            server
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\nSSH-BANNER")
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(http_connect(&mut client, "::1", 2222), work);
        result.unwrap();
        let mut banner = [0; 10];
        client.read_exact(&mut banner).await.unwrap();
        assert_eq!(&banner, b"SSH-BANNER");
    }
    #[tokio::test]
    async fn proxy_authentication_refusal_malformed_replies_and_header_limit_fail_closed() {
        for reply in [vec![5, 2], vec![4, 0], vec![5, 255]] {
            let (mut client, mut server) = tokio::io::duplex(4096);
            let work = async {
                let mut greeting = [0; 3];
                server.read_exact(&mut greeting).await.unwrap();
                server.write_all(&reply).await.unwrap();
            };
            let (result, ()) = tokio::join!(socks5(&mut client, "target.internal", 22), work);
            assert!(matches!(result, Err(SshError::ProxyRejected(_))));
        }
        for reply in [
            b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n".to_vec(),
            b"bad 200 OK\r\n\r\n".to_vec(),
            vec![b'x'; 16 * 1024],
        ] {
            let (mut client, mut server) = tokio::io::duplex(32 * 1024);
            let work = async {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(server.read_u8().await.unwrap());
                }
                server.write_all(&reply).await.unwrap();
            };
            let (result, ()) = tokio::join!(http_connect(&mut client, "target.internal", 22), work);
            assert!(matches!(result, Err(SshError::ProxyRejected(_))));
        }
        let (mut client, _server) = tokio::io::duplex(32);
        assert!(matches!(
            http_connect(&mut client, "bad\r\ninjected", 22).await,
            Err(SshError::ProxyRejected(_))
        ));
    }
}
