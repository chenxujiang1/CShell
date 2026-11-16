//! An explicit B endpoint authenticated by OpenSSH running on a dedicated A transport.
//! Only a private multiplexing socket is created on A; keys never leave A.
use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct ManagedPeer {
    gateway: Arc<RusshClient>,
    host: String,
    port: u16,
    user: String,
    root: String,
    fingerprint: String,
    closed: AtomicBool,
    cleanup_confirmed: AtomicBool,
    master_alive: Arc<AtomicBool>,
    master: Mutex<Option<Master>>,
}
struct Master {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Master {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}
impl std::fmt::Debug for ManagedPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedPeer")
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
fn error(detail: &'static str) -> SshError {
    SshError::ManagedPeer(detail)
}
pub(super) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn target_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-%[]".contains(&b))
}

impl ManagedPeer {
    pub(crate) async fn connect(
        gateway: Arc<RusshClient>,
        target: &cshell_domain::SshConnectionRecord,
        expected: Option<&str>,
    ) -> Result<Self, SshError> {
        let (host, port, user, identity) = (
            target.host.as_str(),
            target.port,
            target.username.as_str(),
            target.private_key_path.as_deref(),
        );
        if !target.options.is_valid()
            || !target.options.authentication_on_jump
            || !matches!(target.route, cshell_domain::SshRoute::Jump { .. })
            || !target_field(host)
            || !target_field(user)
            || port == 0
            || identity.is_some_and(|path| {
                !path.starts_with('/') || path.len() > 4096 || path.chars().any(char::is_control)
            })
        {
            return Err(error("B 的地址、用户名、端口或 A 上的私钥路径无效"));
        }
        let allocated = run(
            &gateway,
            "umask 077; mktemp -d /tmp/cshell-hop.XXXXXXXXXX",
            Duration::from_secs(5),
            256,
        )
        .await?;
        if allocated.status != 0 {
            return Err(error("无法在 A 创建私有 SSH 运行目录"));
        }
        let root = std::str::from_utf8(&allocated.stdout)
            .ok()
            .and_then(|s| s.strip_suffix('\n'))
            .filter(|s| {
                s.starts_with("/tmp/cshell-hop.")
                    && (25..=64).contains(&s.len())
                    && s[16..].bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .ok_or_else(|| error("A 返回了无效的 SSH 运行目录，未启动 B 连接"))?
            .to_owned();
        let mut peer = Self {
            gateway,
            host: host.into(),
            port,
            user: user.into(),
            root,
            fingerprint: String::new(),
            closed: AtomicBool::new(false),
            cleanup_confirmed: AtomicBool::new(false),
            master_alive: Arc::new(AtomicBool::new(false)),
            master: Mutex::new(None),
        };
        let identity = identity.map_or_else(String::new, |path| {
            format!(" -o IdentitiesOnly=yes -i {}", quote(path))
        });
        // Fixed algorithms preserve the application's modern policy even when
        // the user's A-side ssh_config enables legacy crypto.
        let algorithms = " -o HostKeyAlgorithms=ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256 -o KexAlgorithms=curve25519-sha256,curve25519-sha256@libssh.org,ecdh-sha2-nistp256,ecdh-sha2-nistp384,ecdh-sha2-nistp521,diffie-hellman-group16-sha512,diffie-hellman-group18-sha512,diffie-hellman-group14-sha256 -o Ciphers=chacha20-poly1305@openssh.com,aes256-gcm@openssh.com,aes128-gcm@openssh.com,aes256-ctr,aes192-ctr,aes128-ctr -o MACs=hmac-sha2-512-etm@openssh.com,hmac-sha2-256-etm@openssh.com,hmac-sha2-512,hmac-sha2-256";
        let keepalive_interval = if target.options.keepalive_enabled {
            target.options.keepalive_interval_secs
        } else {
            0
        };
        let keepalive_max = target.options.keepalive_max;
        let command = format!(
            "LC_ALL=C ssh -v -T -N -M -S {} -o ControlMaster=yes -o ControlPersist=60 -o BatchMode=yes -o PasswordAuthentication=no -o ChallengeResponseAuthentication=no -o PreferredAuthentications=publickey -o StrictHostKeyChecking=yes -o FingerprintHash=sha256 -o UpdateHostKeys=no -o ClearAllForwardings=yes -o PermitLocalCommand=no -o ForwardAgent=no -o ForwardX11=no -o ConnectTimeout=10 -o ConnectionAttempts=1 -o ServerAliveInterval={keepalive_interval} -o ServerAliveCountMax={keepalive_max}{algorithms}{identity} -p {} -l {} -- {}",
            quote(&peer.socket()),
            port,
            quote(user),
            quote(host)
        );
        let initialize = async {
            let fingerprint = peer.start_master(&command).await?;
            if expected.is_some_and(|pin| pin != fingerprint) {
                return Err(error(
                    "B 的主机指纹与原会话不一致；请核实服务器后重新建立连接",
                ));
            }
            if peer
                .read("printf 'CSHELL-B-READY\\n'", Duration::from_secs(5), 1024)
                .await
                .as_deref()
                != Some(b"CSHELL-B-READY\n")
            {
                return Err(error(
                    "B 的复用通道未就绪，未创建替代连接；请检查 A 上的 OpenSSH 配置",
                ));
            }
            Ok(fingerprint)
        }
        .await;
        match initialize {
            Ok(fingerprint) => peer.fingerprint = fingerprint,
            Err(error) => {
                let _ = peer.disconnect().await;
                return Err(error);
            }
        }
        Ok(peer)
    }
    fn socket(&self) -> String {
        format!("{}/control", self.root)
    }
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub(crate) fn gateway(&self) -> &Arc<RusshClient> {
        &self.gateway
    }
    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
            || !self.master_alive.load(Ordering::Acquire)
            || self.gateway.transport_closed()
    }
    async fn start_master(&self, command: &str) -> Result<String, SshError> {
        let mut owned = Closing(Some(self.gateway.open_exec().await?));
        let mut diagnostics = Vec::new();
        let fingerprint = tokio::time::timeout(Duration::from_secs(25), async {
            let channel = owned
                .0
                .as_mut()
                .ok_or_else(|| error("A 的控制通道已关闭"))?;
            channel.request(command.as_bytes()).await?;
            while let Some(event) = channel.next_event().await {
                match event {
                    TerminalEvent::Data {
                        stream: TerminalDataStream::Extended(_),
                        data,
                    } => {
                        if diagnostics.len().saturating_add(data.len()) > 65536 {
                            return Err(error("A 端 SSH 启动诊断超限"));
                        }
                        diagnostics.extend_from_slice(&data);
                        let text = String::from_utf8_lossy(&diagnostics);
                        if text.contains("Authenticated to ") && text.contains("new mux listener") {
                            return fingerprint(&diagnostics)
                                .ok_or_else(|| error("无法确认 B 的主机指纹"));
                        }
                    }
                    TerminalEvent::Data { data, .. } if !data.is_empty() => {
                        return Err(error("A 的启动环境产生额外输出，请检查非交互 SSH 配置"));
                    }
                    TerminalEvent::Closed
                    | TerminalEvent::RequestFailed
                    | TerminalEvent::ExitSignal { .. } => break,
                    TerminalEvent::ExitStatus(code) if code != 0 => break,
                    _ => {}
                }
            }
            Err(startup_error(&diagnostics))
        })
        .await
        .map_err(|_| error("A 端 B 认证或连接复用启动超时"))??;
        self.master_alive.store(true, Ordering::Release);
        let alive = self.master_alive.clone();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if let Some(channel) = owned.0.as_mut() {
                loop {
                    let event = tokio::select! { biased; _ = &mut stopped => break, event = channel.next_event() => event };
                    match event {
                        None
                        | Some(
                            TerminalEvent::Closed
                            | TerminalEvent::ExitSignal { .. }
                            | TerminalEvent::RequestFailed,
                        ) => break,
                        Some(TerminalEvent::ExitStatus(code)) if code != 0 => break,
                        _ => {} // Drain without retaining diagnostics or adding terminal output.
                    }
                }
            }
            alive.store(false, Ordering::Release);
        });
        *self
            .master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Master {
            stop: Some(stop),
            task,
        });
        Ok(fingerprint)
    }
    pub(crate) fn command(
        &self,
        command: &str,
        terminal: bool,
        subsystem: bool,
    ) -> Result<String, SshError> {
        if self.closed() {
            return Err(error("B 的受管理连接已关闭"));
        }
        if command.len() > 128 * 1024 || command.contains('\0') {
            return Err(error("B 命令格式无效或超限"));
        }
        // A missing master MUST fail, rather than silently reauthenticate B and
        // bypass the captured identity. ProxyCommand=false prevents fallback.
        Ok(format!(
            "LC_ALL=C ssh {} {} -S {} -o ControlMaster=no -o ControlPersist=no -o ProxyCommand=false -o BatchMode=yes -o StrictHostKeyChecking=yes -o ClearAllForwardings=yes -o PermitLocalCommand=no -o ForwardAgent=no -o ForwardX11=no -o LogLevel=ERROR -o EscapeChar=none -p {} -l {} -- {} {}",
            if terminal { "-tt" } else { "-T" },
            if subsystem { "-s" } else { "" },
            quote(&self.socket()),
            self.port,
            quote(&self.user),
            quote(&self.host),
            quote(command)
        ))
    }
    pub(crate) async fn terminal(
        &self,
        rows: u32,
        cols: u32,
        terminal_type: &str,
        marker: Option<&str>,
        command: Option<&str>,
    ) -> Result<SshTerminal, SshError> {
        let generated = cshell_domain::SessionId::new().to_string();
        let marker = marker.or(Some(generated.as_str()));
        let ready = command
            .is_none()
            .then(|| format!("\u{1e}CSHELL_B_READY_{generated}\u{1f}").into_bytes());
        let remote = if let Some(command) = command {
            command.to_owned()
        } else {
            let marker = marker
                .filter(|m| {
                    !m.is_empty()
                        && m.len() <= 64
                        && m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .ok_or_else(|| error("B 终端缺少有效的目录绑定标识"))?;
            format!(
                "/bin/sh -c {}",
                quote(&format!(
                    "LC_CSHELL_SESSION={}; export LC_CSHELL_SESSION; printf '\\036CSHELL_B_READY_{generated}\\037'; exec \"${{SHELL:-/bin/sh}}\" -l",
                    quote(marker)
                ))
            )
        };
        self.gateway
            .open_terminal_command(
                rows,
                cols,
                terminal_type,
                &self.command(&remote, true, false)?,
                ready.as_deref(),
            )
            .await
    }
    pub(crate) async fn exec(
        &self,
        command: &str,
        rows: u32,
        cols: u32,
    ) -> Result<ExecResult, SshError> {
        let terminal = self
            .terminal(rows, cols, "xterm-256color", None, Some(command))
            .await?;
        let (reader, writer) = terminal.split();
        let mut owned = Closing(Some(SshExecChannel {
            reader,
            writer,
            managed: None,
        }));
        tokio::time::timeout(Duration::from_secs(120), async {
            let channel = owned
                .0
                .as_mut()
                .ok_or_else(|| error("B 终端执行通道已关闭"))?;
            let mut result = ExecResult {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_status: u32::MAX,
            };
            while let Some(event) = channel.next_event().await {
                match event {
                    TerminalEvent::Data { stream, data } => {
                        if result
                            .stdout
                            .len()
                            .saturating_add(result.stderr.len())
                            .saturating_add(data.len())
                            > 8 * 1024 * 1024
                        {
                            return Err(error("B 终端命令输出超限"));
                        }
                        if stream == TerminalDataStream::Stdout {
                            result.stdout.extend_from_slice(&data);
                        } else {
                            result.stderr.extend_from_slice(&data);
                        }
                    }
                    TerminalEvent::ExitStatus(code) => result.exit_status = code,
                    TerminalEvent::Closed => break,
                    _ => {}
                }
            }
            if result.exit_status == u32::MAX {
                return Err(error("B 终端命令缺少结束回执"));
            }
            Ok(result)
        })
        .await
        .map_err(|_| error("B 终端命令超时"))?
    }
    pub(crate) async fn sftp(&self) -> Result<cshell_sftp::SftpClient, SshError> {
        let stream = self
            .gateway
            .open_command_stream(&self.command("sftp", false, true)?)
            .await?;
        cshell_sftp::SftpClient::connect(stream)
            .await
            .map_err(SshError::Sftp)
    }
    pub(crate) async fn read(
        &self,
        command: &str,
        limit: Duration,
        maximum: usize,
    ) -> Option<Vec<u8>> {
        let command = self.command(command, false, false).ok()?;
        let output = run(&self.gateway, &command, limit, maximum).await.ok()?;
        (output.status == 0 && output.stderr.is_empty()).then_some(output.stdout)
    }
    pub(crate) async fn disconnect(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return self.cleanup_confirmed.load(Ordering::Acquire);
        }
        let result = cleanup(
            self.gateway.clone(),
            self.root.clone(),
            self.host.clone(),
            self.port,
            self.user.clone(),
        )
        .await;
        self.master_alive.store(false, Ordering::Release);
        self.cleanup_confirmed.store(result, Ordering::Release);
        self.master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        result
    }
}
impl Drop for ManagedPeer {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::AcqRel)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let (gateway, root, host, port, user) = (
                self.gateway.clone(),
                self.root.clone(),
                self.host.clone(),
                self.port,
                self.user.clone(),
            );
            runtime.spawn(cleanup(gateway, root, host, port, user));
        }
    }
}
async fn cleanup(
    gateway: Arc<RusshClient>,
    root: String,
    host: String,
    port: u16,
    user: String,
) -> bool {
    // Never recursively delete anything. OpenSSH removes its own control socket;
    // rmdir only removes the exact private directory if it is empty.
    let command = format!(
        "LC_ALL=C ssh -S {} -O exit -p {} -l {} -- {} >/dev/null 2>&1; rmdir -- {} 2>/dev/null",
        quote(&format!("{root}/control")),
        port,
        quote(&user),
        quote(&host),
        quote(&root)
    );
    run(&gateway, &command, Duration::from_secs(3), 1024)
        .await
        .is_ok_and(|output| output.status == 0)
}
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: u32,
}
pub(super) struct Closing(pub(super) Option<SshExecChannel>);
impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(channel) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), channel.close()).await;
            });
        }
    }
}
async fn run(
    gateway: &RusshClient,
    command: &str,
    limit: Duration,
    maximum: usize,
) -> Result<Output, SshError> {
    tokio::time::timeout(limit, async {
        let mut owned = Closing(Some(gateway.open_exec().await?));
        let channel = owned
            .0
            .as_mut()
            .ok_or_else(|| error("A 的执行通道已关闭"))?;
        channel.request(command.as_bytes()).await?;
        let mut output = Output {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: u32::MAX,
        };
        while let Some(event) = channel.next_event().await {
            match event {
                TerminalEvent::Data { stream, data } => {
                    if output
                        .stdout
                        .len()
                        .saturating_add(output.stderr.len())
                        .saturating_add(data.len())
                        > maximum
                    {
                        return Err(error("A 端 SSH 回复超限"));
                    }
                    if stream == TerminalDataStream::Stdout {
                        output.stdout.extend_from_slice(&data);
                    } else {
                        output.stderr.extend_from_slice(&data);
                    }
                }
                TerminalEvent::ExitStatus(status) => output.status = status,
                TerminalEvent::ExitSignal { .. } | TerminalEvent::RequestFailed => {
                    return Err(error("A 拒绝或终止了 SSH 中转命令"));
                }
                TerminalEvent::Closed => break,
                _ => {}
            }
        }
        if output.status == u32::MAX {
            return Err(error("A 端 SSH 缺少结束回执"));
        }
        Ok(output)
    })
    .await
    .map_err(|_| error("A 端 SSH 操作超时"))?
}
fn fingerprint(stderr: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stderr).ok()?;
    let mut keys = text
        .lines()
        .filter_map(|line| line.strip_prefix("debug1: Server host key: "));
    let mut parts = keys.next()?.split_ascii_whitespace();
    parts.next()?;
    let fingerprint = parts.next()?;
    if keys.next().is_some()
        || parts.next().is_some()
        || !fingerprint.starts_with("SHA256:")
        || !(40..=96).contains(&fingerprint.len())
        || !fingerprint[7..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    {
        return None;
    }
    Some(fingerprint.to_owned())
}
pub(super) fn split_ready<'a>(bytes: &'a [u8], marker: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    if marker.is_empty() {
        return None;
    }
    let at = bytes
        .windows(marker.len())
        .position(|window| window == marker)?;
    Some((&bytes[..at], &bytes[at + marker.len()..]))
}
fn startup_error(diagnostics: &[u8]) -> SshError {
    let text = String::from_utf8_lossy(diagnostics);
    if text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        error(
            "A 检测到 B 主机指纹变化；请核实服务器身份后在 A 更新对应信任记录，程序没有自动修改或忽略指纹",
        )
    } else if text.contains("Host key verification failed")
        || text.contains("No ED25519 host key is known")
    {
        error("A 尚未信任 B 的主机指纹，或其信任校验失败；请先在 A 核实并确认 B，程序不会跳过校验")
    } else if text.contains("Permission denied") {
        error(
            "A 对 B 的免密认证失败；请检查 A 上的私钥、可用 Agent、B 用户名及授权，不会尝试本机的 B 密码",
        )
    } else {
        error("A 无法非交互连接 B；请检查 A 上的 OpenSSH、B 指纹和免密配置，不会自动跳过校验")
    }
}

#[cfg(test)]
mod tests;
