use super::{ForwardRouteKey, RusshClient, SshError};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::{JoinHandle, JoinSet};

const OPEN_CHANNEL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForwardLimits {
    pub max_connections: usize,
    pub pending_remote_channels: usize,
    pub socks_handshake_timeout: Duration,
}

impl Default for ForwardLimits {
    fn default() -> Self {
        Self {
            max_connections: 128,
            pending_remote_channels: 128,
            socks_handshake_timeout: Duration::from_secs(10),
        }
    }
}

impl ForwardLimits {
    fn validate(self) -> Result<Self, SshError> {
        if self.max_connections == 0 || self.pending_remote_channels == 0 {
            return Err(SshError::InvalidForwardConfig(
                "connection and queue limits must be greater than zero".to_owned(),
            ));
        }
        if self.socks_handshake_timeout.is_zero() {
            return Err(SshError::InvalidForwardConfig(
                "SOCKS5 handshake timeout must be greater than zero".to_owned(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteForwardTarget {
    pub host: String,
    pub port: u16,
}

pub struct ForwardHandle {
    bound_address: String,
    bound_port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), SshError>>>,
}

impl std::fmt::Debug for ForwardHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ForwardHandle")
            .field("bound_address", &self.bound_address)
            .field("bound_port", &self.bound_port)
            .finish_non_exhaustive()
    }
}

impl ForwardHandle {
    #[must_use]
    pub fn bound_address(&self) -> &str {
        &self.bound_address
    }

    #[must_use]
    pub const fn bound_port(&self) -> u16 {
        self.bound_port
    }

    pub async fn shutdown(mut self) -> Result<(), SshError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task
            .take()
            .ok_or_else(|| SshError::ForwardTask("forwarding task is missing".to_owned()))?
            .await
            .map_err(|error| SshError::ForwardTask(error.to_string()))?
    }
}

impl Drop for ForwardHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl RusshClient {
    pub async fn start_local_forward(
        self: &Arc<Self>,
        bind: SocketAddr,
        target_host: impl Into<String>,
        target_port: u16,
        limits: ForwardLimits,
    ) -> Result<ForwardHandle, SshError> {
        let limits = limits.validate()?;
        let target_host = target_host.into();
        validate_target(&target_host, target_port)?;
        let listener = TcpListener::bind(bind).await.map_err(SshError::ForwardIo)?;
        let local = listener.local_addr().map_err(SshError::ForwardIo)?;
        let (shutdown, stopped) = oneshot::channel();
        let client = Arc::clone(self);
        let task = tokio::spawn(async move {
            run_direct_listener(listener, client, target_host, target_port, limits, stopped).await
        });
        Ok(ForwardHandle {
            bound_address: local.ip().to_string(),
            bound_port: local.port(),
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub async fn start_dynamic_forward(
        self: &Arc<Self>,
        bind: SocketAddr,
        limits: ForwardLimits,
    ) -> Result<ForwardHandle, SshError> {
        let limits = limits.validate()?;
        let listener = TcpListener::bind(bind).await.map_err(SshError::ForwardIo)?;
        let local = listener.local_addr().map_err(SshError::ForwardIo)?;
        let (shutdown, stopped) = oneshot::channel();
        let client = Arc::clone(self);
        let task =
            tokio::spawn(
                async move { run_dynamic_listener(listener, client, limits, stopped).await },
            );
        Ok(ForwardHandle {
            bound_address: local.ip().to_string(),
            bound_port: local.port(),
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub async fn start_remote_forward(
        self: &Arc<Self>,
        remote_bind_address: impl Into<String>,
        remote_bind_port: u16,
        target: RemoteForwardTarget,
        limits: ForwardLimits,
    ) -> Result<ForwardHandle, SshError> {
        let limits = limits.validate()?;
        let address = remote_bind_address.into();
        validate_target(&target.host, target.port)?;
        let assigned = self
            .session
            .tcpip_forward(address.clone(), u32::from(remote_bind_port))
            .await?;
        let port = if remote_bind_port == 0 {
            let assigned = u16::try_from(assigned).map_err(|_| {
                SshError::InvalidForwardConfig("server returned an invalid remote port".to_owned())
            })?;
            if assigned == 0 {
                return Err(SshError::InvalidForwardConfig(
                    "server did not allocate a remote port".to_owned(),
                ));
            }
            assigned
        } else {
            remote_bind_port
        };
        let key: ForwardRouteKey = (address.clone(), u32::from(port));
        let (sender, receiver) = tokio::sync::mpsc::channel(limits.pending_remote_channels);
        let route_replaced = {
            let mut routes = self.forward_routes.write().map_err(|_| {
                SshError::ForwardTask("remote forwarding route table was poisoned".to_owned())
            })?;
            if routes.contains_key(&key) {
                true
            } else {
                routes.insert(key.clone(), sender);
                false
            }
        };
        if route_replaced {
            let _ = self
                .session
                .cancel_tcpip_forward(address.clone(), u32::from(port))
                .await;
            return Err(SshError::InvalidForwardConfig(
                "remote forwarding address and port are already active".to_owned(),
            ));
        }
        let (shutdown, stopped) = oneshot::channel();
        let client = Arc::clone(self);
        let task_address = address.clone();
        let task = tokio::spawn(async move {
            let result = run_remote_forward(receiver, target, limits, stopped).await;
            if let Ok(mut routes) = client.forward_routes.write() {
                routes.remove(&key);
            }
            let cancel = client
                .session
                .cancel_tcpip_forward(task_address, u32::from(port))
                .await;
            result.and_then(|()| cancel.map_err(SshError::Protocol))
        });
        Ok(ForwardHandle {
            bound_address: address,
            bound_port: port,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }
}

fn validate_target(host: &str, port: u16) -> Result<(), SshError> {
    if host.is_empty() {
        return Err(SshError::InvalidForwardConfig(
            "forward target host must not be empty".to_owned(),
        ));
    }
    if port == 0 {
        return Err(SshError::InvalidForwardConfig(
            "forward target port must not be zero".to_owned(),
        ));
    }
    Ok(())
}

async fn run_direct_listener(
    listener: TcpListener,
    client: Arc<RusshClient>,
    target_host: String,
    target_port: u16,
    limits: ForwardLimits,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), SshError> {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (socket, peer) = accepted.map_err(SshError::ForwardIo)?;
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else { continue };
                let client = Arc::clone(&client);
                let host = target_host.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let channel = tokio::time::timeout(
                        OPEN_CHANNEL_TIMEOUT,
                        client.session.channel_open_direct_tcpip(
                            host,
                            u32::from(target_port),
                            peer.ip().to_string(),
                            u32::from(peer.port()),
                        ),
                    )
                    .await
                    .map_err(|_| SshError::ConnectionTimeout)??;
                    bridge(socket, channel.into_stream()).await
                });
            }
            Some(_completed) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
    Ok(())
}

async fn run_dynamic_listener(
    listener: TcpListener,
    client: Arc<RusshClient>,
    limits: ForwardLimits,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), SshError> {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (mut socket, peer) = accepted.map_err(SshError::ForwardIo)?;
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else { continue };
                let client = Arc::clone(&client);
                connections.spawn(async move {
                    let _permit = permit;
                    let (host, port) = tokio::time::timeout(
                        limits.socks_handshake_timeout,
                        read_socks5_connect(&mut socket),
                    )
                    .await
                    .map_err(|_| SshError::Socks5("handshake timed out".to_owned()))??;
                    let channel = match tokio::time::timeout(
                        OPEN_CHANNEL_TIMEOUT,
                        client.session.channel_open_direct_tcpip(
                            host,
                            u32::from(port),
                            peer.ip().to_string(),
                            u32::from(peer.port()),
                        ),
                    ).await {
                        Ok(Ok(channel)) => channel,
                        _ => {
                            let _ = write_socks5_reply(&mut socket, 0x05).await;
                            return Err(SshError::Socks5("SSH server rejected the destination".to_owned()));
                        }
                    };
                    write_socks5_reply(&mut socket, 0x00).await?;
                    bridge(socket, channel.into_stream()).await
                });
            }
            Some(_completed) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
    Ok(())
}

async fn run_remote_forward(
    mut receiver: tokio::sync::mpsc::Receiver<russh::Channel<russh::client::Msg>>,
    target: RemoteForwardTarget,
    limits: ForwardLimits,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), SshError> {
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            channel = receiver.recv() => {
                let Some(channel) = channel else { break };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else { continue };
                let target = target.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let socket = tokio::time::timeout(
                        OPEN_CHANNEL_TIMEOUT,
                        TcpStream::connect((target.host.as_str(), target.port)),
                    )
                    .await
                    .map_err(|_| SshError::ConnectionTimeout)?
                    .map_err(SshError::ForwardIo)?;
                    bridge(socket, channel.into_stream()).await
                });
            }
            Some(_completed) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
    Ok(())
}

async fn bridge<A, B>(mut left: A, mut right: B) -> Result<(), SshError>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional(&mut left, &mut right)
        .await
        .map_err(SshError::ForwardIo)?;
    Ok(())
}

async fn read_socks5_connect<S>(stream: &mut S) -> Result<(String, u16), SshError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut greeting = [0_u8; 2];
    stream
        .read_exact(&mut greeting)
        .await
        .map_err(SshError::ForwardIo)?;
    if greeting[0] != 5 || greeting[1] == 0 {
        return Err(SshError::Socks5("invalid greeting".to_owned()));
    }
    let mut methods = vec![0_u8; usize::from(greeting[1])];
    stream
        .read_exact(&mut methods)
        .await
        .map_err(SshError::ForwardIo)?;
    if !methods.contains(&0) {
        stream
            .write_all(&[5, 0xff])
            .await
            .map_err(SshError::ForwardIo)?;
        return Err(SshError::Socks5(
            "no supported authentication method".to_owned(),
        ));
    }
    stream
        .write_all(&[5, 0])
        .await
        .map_err(SshError::ForwardIo)?;
    let mut request = [0_u8; 4];
    stream
        .read_exact(&mut request)
        .await
        .map_err(SshError::ForwardIo)?;
    if request[0] != 5 || request[1] != 1 || request[2] != 0 {
        write_socks5_reply(stream, 0x07).await?;
        return Err(SshError::Socks5("only CONNECT is supported".to_owned()));
    }
    let host = match request[3] {
        1 => {
            let mut octets = [0_u8; 4];
            stream
                .read_exact(&mut octets)
                .await
                .map_err(SshError::ForwardIo)?;
            std::net::Ipv4Addr::from(octets).to_string()
        }
        3 => {
            let length = stream.read_u8().await.map_err(SshError::ForwardIo)?;
            if length == 0 {
                return Err(SshError::Socks5("empty domain name".to_owned()));
            }
            let mut domain = vec![0_u8; usize::from(length)];
            stream
                .read_exact(&mut domain)
                .await
                .map_err(SshError::ForwardIo)?;
            String::from_utf8(domain)
                .map_err(|_| SshError::Socks5("domain name is not UTF-8".to_owned()))?
        }
        4 => {
            let mut octets = [0_u8; 16];
            stream
                .read_exact(&mut octets)
                .await
                .map_err(SshError::ForwardIo)?;
            std::net::Ipv6Addr::from(octets).to_string()
        }
        _ => {
            write_socks5_reply(stream, 0x08).await?;
            return Err(SshError::Socks5("unsupported address type".to_owned()));
        }
    };
    let port = stream.read_u16().await.map_err(SshError::ForwardIo)?;
    if port == 0 {
        return Err(SshError::Socks5(
            "destination port must not be zero".to_owned(),
        ));
    }
    Ok((host, port))
}

async fn write_socks5_reply<S>(stream: &mut S, status: u8) -> Result<(), SshError>
where
    S: AsyncWrite + Unpin,
{
    stream
        .write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
        .map_err(SshError::ForwardIo)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn limits_reject_zero_capacity_and_timeout() {
        let zero_connections = ForwardLimits {
            max_connections: 0,
            ..ForwardLimits::default()
        };
        assert!(zero_connections.validate().is_err());
        let zero_timeout = ForwardLimits {
            socks_handshake_timeout: Duration::ZERO,
            ..ForwardLimits::default()
        };
        assert!(zero_timeout.validate().is_err());
        assert!(validate_target("", 22).is_err());
        assert!(validate_target("localhost", 0).is_err());
    }

    #[tokio::test]
    async fn socks5_parses_fragmented_domain_connect() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let peer = tokio::spawn(async move {
            client.write_all(&[5, 1]).await?;
            client.write_all(&[0]).await?;
            let mut method = [0_u8; 2];
            client.read_exact(&mut method).await?;
            client.write_all(&[5, 1, 0, 3, 11]).await?;
            client.write_all(b"example.com").await?;
            client.write_all(&443_u16.to_be_bytes()).await?;
            Ok::<_, std::io::Error>(method)
        });
        let destination = read_socks5_connect(&mut server).await.unwrap();
        assert_eq!(destination, ("example.com".to_owned(), 443));
        assert_eq!(peer.await.unwrap().unwrap(), [5, 0]);
    }

    #[tokio::test]
    async fn socks5_rejects_udp_associate() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let peer = tokio::spawn(async move {
            client.write_all(&[5, 1, 0]).await?;
            let mut method = [0_u8; 2];
            client.read_exact(&mut method).await?;
            client.write_all(&[5, 3, 0, 1, 127, 0, 0, 1, 0, 53]).await?;
            let mut reply = [0_u8; 10];
            client.read_exact(&mut reply).await?;
            Ok::<_, std::io::Error>(reply)
        });
        assert!(read_socks5_connect(&mut server).await.is_err());
        assert_eq!(peer.await.unwrap().unwrap()[1], 0x07);
    }

    #[tokio::test]
    async fn socks5_parses_ipv6_connect() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let address = std::net::Ipv6Addr::LOCALHOST;
        let peer = tokio::spawn(async move {
            client.write_all(&[5, 1, 0]).await?;
            let mut method = [0_u8; 2];
            client.read_exact(&mut method).await?;
            let mut request = vec![5, 1, 0, 4];
            request.extend_from_slice(&address.octets());
            request.extend_from_slice(&22_u16.to_be_bytes());
            client.write_all(&request).await?;
            Ok::<_, std::io::Error>(())
        });
        assert_eq!(
            read_socks5_connect(&mut server).await.unwrap(),
            (address.to_string(), 22)
        );
        peer.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bridge_preserves_tcp_style_half_close_in_both_directions() {
        let (mut left_peer, left_bridge) = tokio::io::duplex(64);
        let (right_bridge, mut right_peer) = tokio::io::duplex(64);
        let forwarding = tokio::spawn(bridge(left_bridge, right_bridge));

        left_peer.write_all(b"request").await.unwrap();
        left_peer.shutdown().await.unwrap();
        let mut request = Vec::new();
        right_peer.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");

        right_peer.write_all(b"response").await.unwrap();
        right_peer.shutdown().await.unwrap();
        let mut response = Vec::new();
        left_peer.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response");
        forwarding.await.unwrap().unwrap();
    }
}
