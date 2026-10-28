//! A bounded ZMODEM side channel. Binary transfer data never enters the terminal
//! parser, clipboard observers, output journal, or synchronized keyboard input.
use cshell_ipc::{ZmodemRequest, ZmodemTransfer};
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

const CANCEL: &[u8] =
    b"\x18\x18\x18\x18\x18\x18\x18\x18\x18\x18\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08";
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

enum Protocol {
    Send(Sender),
    Receive(Receiver),
}
impl Protocol {
    fn poll(&mut self) -> Action<'_> {
        match self {
            Self::Send(p) => p.poll(),
            Self::Receive(p) => p.poll(),
        }
    }
    fn submit(&mut self, data: &[u8]) -> Result<usize, zmodem2::Error> {
        match self {
            Self::Send(p) => p.submit_wire(data),
            Self::Receive(p) => p.submit_wire(data),
        }
    }
    fn written(&mut self, n: usize) {
        match self {
            Self::Send(p) => p.wire_written(n),
            Self::Receive(p) => p.wire_written(n),
        }
    }
    fn timeout(&mut self) -> Result<(), zmodem2::Error> {
        match self {
            Self::Send(p) => p.timeout(),
            Self::Receive(p) => p.timeout(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Transfer {
    pub status: Option<ZmodemTransfer>,
    candidate: Vec<u8>,
    candidate_since: Option<Instant>,
    incoming: Vec<u8>,
    protocol: Option<Protocol>,
    outgoing: Vec<u8>,
    uploads: VecDeque<(File, String, u32)>,
    source: Option<File>,
    destination: Option<tempfile::NamedTempFile>,
    directory: PathBuf,
    last_activity: Option<Instant>,
    began: Option<Instant>,
    timeouts: u8,
    expected: Option<u64>,
    receive_trailer: Option<(Instant, u8)>,
}

impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZmodemTransfer")
            .field("busy", &self.busy())
            .finish_non_exhaustive()
    }
}

impl Transfer {
    pub fn busy(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.state == 1 || s.state == 2)
    }
    pub fn input_epoch(&self) -> u64 {
        self.status.as_ref().map_or(0, |s| s.transfer_id)
    }
    pub fn needs_service(&self) -> bool {
        self.busy() || !self.outgoing.is_empty() || !self.candidate.is_empty()
    }
    pub fn buffered_bytes(&self)->usize {self.incoming.len()}

    /// Returns only ordinary terminal bytes. Verify the complete hex header,
    /// including CRC, before recognizing a remote transfer request.
    pub fn incoming(&mut self, data: &[u8]) -> Vec<u8> {
        // Flush the previous frame's trailer before examining the next frame.
        // OO may arrive in a later SSH packet than the ZFIN line ending.
        let mut terminal = self.terminal_tail();
        let data = self.strip_receive_trailer(data);
        if self.busy() {
            if self.incoming.len().saturating_add(data.len()) > 1024 * 1024 {
                self.fail("ZMODEM 接收缓冲超限，传输已停止");
            } else {
                self.incoming.extend_from_slice(data);
                self.last_activity = Some(Instant::now());
            }
            return Vec::new();
        }
        for (offset, &byte) in data.iter().enumerate() {
            self.candidate.push(byte);
            while !self.candidate.is_empty() && !header_prefix(&self.candidate) {
                terminal.push(self.candidate.remove(0));
            }
            if self.candidate.len() == 18 {
                if let Some(direction) = recognize(&self.candidate) {
                    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.status = Some(ZmodemTransfer {
                        transfer_id: id,
                        direction,
                        state: 1,
                        detail: if direction == 1 {
                            "请选择上传文件"
                        } else {
                            "等待下载目录"
                        }
                        .into(),
                        ..Default::default()
                    });
                    self.incoming = std::mem::take(&mut self.candidate);
                    self.incoming.extend_from_slice(&data[offset + 1..]);
                    self.last_activity = Some(Instant::now());
                    self.began = self.last_activity;
                    self.timeouts = 0;
                    break;
                }
                terminal.append(&mut self.candidate);
            }
        }
        self.candidate_since = (!self.candidate.is_empty()).then(Instant::now);
        terminal
    }

    pub fn flush_candidate(&mut self) -> Vec<u8> {
        let timeout=if self.candidate.len()>=4{Duration::from_secs(2)}else{Duration::from_millis(250)};
        if self
            .candidate_since
            .is_some_and(|t| t.elapsed() >= timeout)
        {
            self.candidate_since = None;
            return std::mem::take(&mut self.candidate);
        }
        Vec::new()
    }

    pub fn control(&mut self, request: &ZmodemRequest) -> Result<(), String> {
        let s = self.status.as_ref().ok_or("没有待处理的 ZMODEM 请求")?;
        if s.transfer_id != request.transfer_id || !self.busy() {
            return Err("传输请求已过期".into());
        }
        if request.operation == 3 {
            self.stop(4, "已取消传输");
            return Ok(());
        }
        if s.state != 1 {
            return Err("传输已经开始".into());
        }
        if request.operation != s.direction {
            return Err("传输方向不匹配".into());
        }
        if request.operation == 1 {
            if request.paths.is_empty() || request.paths.len() > 128 {
                return Err("请选择 1 至 128 个普通文件".into());
            }
            let mut files = VecDeque::new();
            for path in &request.paths {
                let path = Path::new(path);
                if !path.is_absolute() {
                    return Err("上传路径必须为绝对路径".into());
                }
                let file = File::open(path).map_err(|e| format!("无法打开上传文件：{e}"))?;
                let meta = file.metadata().map_err(|e| e.to_string())?;
                if !meta.is_file() || meta.len() > u64::from(u32::MAX) {
                    return Err("ZMODEM 仅支持小于 4 GiB 的普通文件".into());
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or("上传文件名必须是有效 Unicode")?
                    .to_owned();
                safe_name(name.as_bytes())?;
                files.push_back((file, name, meta.len() as u32));
            }
            self.uploads = files;
            self.protocol = Some(Protocol::Send(Sender::new().map_err(|e| e.to_string())?));
            self.start_next_upload()?;
        } else {
            if request.paths.len() != 1 {
                return Err("请选择下载目录".into());
            }
            let path = Path::new(&request.paths[0]);
            if !path.is_absolute() || !path.is_dir() {
                return Err("下载目录必须是已存在文件夹的绝对路径".into());
            }
            self.directory = path.canonicalize().map_err(|e| e.to_string())?;
            let mut receiver = Receiver::new().map_err(|e| e.to_string())?;
            receiver.set_manual_file_accept(true);
            self.protocol = Some(Protocol::Receive(receiver));
        }
        let s = self.status.as_mut().ok_or("传输已结束")?;
        s.state = 2;
        s.detail = "正在传输".into();
        self.last_activity = Some(Instant::now());
        Ok(())
    }

    fn start_next_upload(&mut self) -> Result<(), String> {
        let Some(Protocol::Send(sender)) = self.protocol.as_mut() else {
            return Err("上传状态错误".into());
        };
        if let Some((file, name, size)) = self.uploads.pop_front() {
            sender
                .start_file(
                    FileInfo::new(name.as_bytes(), Some(Position::new(size))).with_mode(0o100644),
                )
                .map_err(|e| e.to_string())?;
            self.source = Some(file);
            if let Some(s) = &mut self.status {
                s.name = name;
                s.bytes = 0;
                s.size = u64::from(size);
            }
        } else {
            sender.finish().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Called on a blocking worker; limits each pump to 64 KiB of outgoing
    /// traffic so file I/O and protocol parsing cannot monopolize Tokio.
    pub fn pump(&mut self) -> Vec<u8> {
        if let Err(error) = self.drive() {
            self.fail(&format!("ZMODEM 传输失败：{error}"));
        }
        std::mem::take(&mut self.outgoing)
    }

    fn drive(&mut self) -> Result<(), String> {
        if !self.busy() {
            return Ok(());
        }
        let elapsed = self.last_activity.map(|t| t.elapsed()).unwrap_or_default();
        if self.protocol.is_none() {
            if self
                .began
                .is_some_and(|t| t.elapsed() > Duration::from_secs(180))
            {
                self.fail("选择文件或目录超时，传输已取消");
            }
            return Ok(());
        }
        if elapsed > Duration::from_secs(10) {
            self.timeouts += 1;
            if self.timeouts >= 6 {
                self.fail("ZMODEM 传输超时");
                return Ok(());
            }
            self.protocol
                .as_mut()
                .ok_or("协议已结束")?
                .timeout()
                .map_err(|e| e.to_string())?;
            self.last_activity = Some(Instant::now());
        }
        for _ in 0..4096 {
            if self.outgoing.len() >= 64 * 1024 {
                break;
            }
            let Some(protocol) = self.protocol.as_mut() else {
                break;
            };
            match protocol.poll() {
                Action::WriteWire(bytes) => {
                    let n = bytes.len();
                    self.outgoing.extend_from_slice(bytes);
                    protocol.written(n);
                    self.last_activity=Some(Instant::now());
                }
                Action::ReadFile { offset, max_len } => {
                    let file = self.source.as_mut().ok_or("上传文件未打开")?;
                    file.seek(SeekFrom::Start(u64::from(offset.get())))
                        .map_err(|e| e.to_string())?;
                    let mut bytes = vec![0; max_len.min(8192)];
                    let n = file.read(&mut bytes).map_err(|e| e.to_string())?;
                    if let Protocol::Send(sender) = protocol {
                        sender.submit_file(&bytes[..n]).map_err(|e| e.to_string())?;
                    }
                    if let Some(s) = &mut self.status {
                        s.bytes = u64::from(offset.get()) + n as u64;
                    }
                }
                Action::WriteFile(bytes) => {
                    let n = bytes.len();
                    let file = self.destination.as_mut().ok_or("下载文件未打开")?;
                    if let Some(s) = &mut self.status {
                        if s.bytes.saturating_add(n as u64) > u64::from(u32::MAX)
                            || self
                                .expected
                                .is_some_and(|size| s.bytes.saturating_add(n as u64) > size)
                        {
                            return Err("收到的数据超过声明的文件大小".into());
                        }
                        file.write_all(bytes).map_err(|e| e.to_string())?;
                        s.bytes += n as u64;
                        self.last_activity=Some(Instant::now());
                    }
                    if let Protocol::Receive(receiver) = protocol {
                        receiver.file_written(n).map_err(|e| e.to_string())?;
                    }
                }
                Action::Event(Event::FileStarted(info)) => {
                    let name = safe_name(info.name)?;
                    self.expected = info.size.map(|p| u64::from(p.get()));
                    self.destination = Some(
                        tempfile::Builder::new()
                            .prefix(".cshell-zmodem-")
                            .tempfile_in(&self.directory)
                            .map_err(|e| e.to_string())?,
                    );
                    if let Some(s) = &mut self.status {
                        s.name = name;
                        s.bytes = 0;
                        s.size = self.expected.unwrap_or(0);
                    }
                    if let Protocol::Receive(receiver) = protocol {
                        receiver.accept_file_at(0).map_err(|e| e.to_string())?;
                    }
                }
                Action::Event(Event::FileCompleted) => {
                    if let Some(mut file) = self.destination.take() {
                        let s = self.status.as_mut().ok_or("传输状态丢失")?;
                        if self.expected.is_some_and(|size| s.bytes != size) {
                            return Err("下载文件大小校验失败".into());
                        }
                        file.flush().map_err(|e| e.to_string())?;
                        file.as_file().sync_all().map_err(|e| e.to_string())?;
                        let name = publish_unique(file, &self.directory, &s.name)?;
                        s.name = name;
                    }
                    if let Some(s) = &mut self.status {
                        s.files_completed += 1;
                    }
                    if matches!(self.protocol, Some(Protocol::Send(_))) {
                        self.start_next_upload()?;
                    }
                }
                Action::Event(Event::SessionCompleted) => {
                    // The library emits its completion event before the final
                    // ZFIN/OO bytes. Drain those before releasing the channel.
                    while let Action::WriteWire(bytes) = protocol.poll() {
                        let n = bytes.len();
                        self.outgoing.extend_from_slice(bytes);
                        protocol.written(n);
                    }
                    if matches!(self.protocol, Some(Protocol::Receive(_))) {
                        self.receive_trailer = Some((Instant::now(), 0));
                    }
                    if let Some(s) = &mut self.status {
                        s.state = 3;
                        s.detail = format!("传输完成，共 {} 个文件", s.files_completed);
                    }
                    self.protocol = None;
                    self.source = None;
                    self.uploads.clear();
                    // The unconsumed suffix may already contain a shell prompt.
                    break;
                }
                Action::Event(Event::Aborted) => {
                    self.stop(4, "对端取消了传输");
                    break;
                }
                Action::Idle => {
                    if self.incoming.is_empty() {
                        break;
                    }
                    let n = protocol.submit(&self.incoming).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    self.incoming.drain(..n);
                    self.timeouts = 0;
                }
                _ => return Err("不支持的 ZMODEM 协议事件".into()),
            }
        }
        Ok(())
    }

    pub fn terminal_tail(&mut self) -> Vec<u8> {
        if self.status.as_ref().is_some_and(|s| s.state == 3) {
            let tail = std::mem::take(&mut self.incoming);
            self.strip_receive_trailer(&tail).to_vec()
        } else {
            Vec::new()
        }
    }
    fn strip_receive_trailer<'a>(&mut self, data: &'a [u8]) -> &'a [u8] {
        let Some((started, mut count)) = self.receive_trailer else {
            return data;
        };
        if started.elapsed() > Duration::from_secs(1) {
            self.receive_trailer = None;
            return data;
        }
        let mut consumed = 0;
        for &byte in data {
            if count < 2 && byte == b'O' {
                count += 1;
                consumed += 1;
                if count == 2 {
                    self.receive_trailer = None;
                    return &data[consumed..];
                }
            } else if count == 0 && matches!(byte, b'\r' | b'\n' | 0x11) {
                consumed += 1;
            } else {
                self.receive_trailer = None;
                return &data[consumed..];
            }
        }
        self.receive_trailer = Some((started, count));
        &data[consumed..]
    }
    pub fn disconnected(&mut self) {
        if self.busy() {
            self.stop(5, "SSH 已断开，传输停止；请重新发起传输");
        }
        self.outgoing.clear();
        self.candidate.clear();
        self.incoming.clear();
        self.receive_trailer = None;
    }
    fn fail(&mut self, message: &str) {
        self.stop(5, message);
    }
    fn stop(&mut self, state: u32, message: &str) {
        self.protocol = None;
        self.source = None;
        self.destination = None;
        self.uploads.clear();
        self.incoming.clear();
        self.outgoing.clear();
        self.outgoing.extend_from_slice(CANCEL);
        if let Some(s) = &mut self.status {
            s.state = state;
            s.detail = message.into();
        }
    }
}

fn header_prefix(bytes: &[u8]) -> bool {
    let prefix = b"**\x18B";
    if bytes.len() <= 4 {
        return prefix.starts_with(bytes);
    }
    bytes.len() <= 18 && bytes.starts_with(prefix) && bytes[4..].iter().all(u8::is_ascii_hexdigit)
}
fn recognize(bytes: &[u8]) -> Option<u32> {
    let mut header = [0u8; 7];
    for (i, pair) in bytes.get(4..18)?.chunks_exact(2).enumerate() {
        header[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    let mut crc = 0u16;
    for byte in &header {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    if crc != 0 {
        return None;
    }
    match header[0] {
        0 => Some(2),
        1 => Some(1),
        _ => None,
    }
}

fn safe_name(bytes: &[u8]) -> Result<String, String> {
    let name = std::str::from_utf8(bytes).map_err(|_| "文件名不是有效 UTF-8")?;
    if name.is_empty()
        || name.len() > 240
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:<>\"|?*".contains(c))
    {
        return Err("远端提供了不安全或不兼容的文件名".into());
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&stem.as_str())
    {
        return Err("文件名为系统保留名称".into());
    }
    Ok(name.into())
}
fn publish_unique(
    mut file: tempfile::NamedTempFile,
    directory: &Path,
    name: &str,
) -> Result<String, String> {
    for index in 0..10000 {
        let candidate = if index == 0 {
            name.into()
        } else {
            let path = Path::new(name);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| format!(".{s}"))
                .unwrap_or_default();
            format!("{stem} ({index}){ext}")
        };
        match file.persist_noclobber(directory.join(&candidate)) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => file = e.file,
            Err(e) => return Err(e.error.to_string()),
        }
    }
    Err("同名文件过多，无法保存下载文件".into())
}

#[cfg(test)]
mod tests;
