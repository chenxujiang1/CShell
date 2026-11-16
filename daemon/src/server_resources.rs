//! Explicit, bounded read-only samples; no timers, disk persistence or reconnects.
mod parse;
use crate::{SshSession, SshSessionRegistry};
use cshell_domain::{ServerResources, SessionId};
use cshell_ssh::{SshConnection, TerminalDataStream, TerminalEvent};
use std::{
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MIN_INTERVAL: Duration = Duration::from_secs(5);
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(8);
const SCRIPT: &str = include_str!("server_resources/collect.sh");

#[derive(Debug, Default)]
struct State {
    last_started: Option<Instant>,
    cached: Option<ServerResources>,
    uncertain: bool,
}
#[derive(Debug)]
struct Entry {
    client: Weak<SshConnection>,
    state: Arc<tokio::sync::Mutex<State>>,
}
#[derive(Clone, Debug)]
pub(crate) struct Service {
    entries: Arc<Mutex<Vec<Entry>>>,
    workers: Arc<tokio::sync::Semaphore>,
}
impl Default for Service {
    fn default() -> Self {
        Self {
            entries: Arc::default(),
            workers: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}
impl Service {
    pub(crate) async fn sample(
        &self,
        registry: &SshSessionRegistry,
        id: SessionId,
    ) -> Result<ServerResources, &'static str> {
        let session = registry.get(id).map_err(|_| "请先连接 SSH 服务器")?;
        let generation = session.transport_generation();
        let client = session
            .checksum_connection()
            .ok_or("SSH 已断开，请先重新连接")?;
        let state = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.retain(|e| {
                e.client
                    .upgrade()
                    .is_some_and(|client| !client.transport_closed())
            });
            let weak = Arc::downgrade(&client);
            if let Some(entry) = entries.iter().find(|e| e.client.ptr_eq(&weak)) {
                entry.state.clone()
            } else {
                if entries.len() >= 32 {
                    return Err("资源采集连接数已达上限，请关闭不用的 SSH 连接");
                }
                let state = Arc::new(tokio::sync::Mutex::new(State::default()));
                entries.push(Entry {
                    client: weak,
                    state: state.clone(),
                });
                state
            }
        };
        let mut state = tokio::time::timeout(Duration::from_secs(3), state.lock_owned())
            .await
            .map_err(|_| "该 SSH 连接正在采集，请稍后刷新")?;
        if !current(&session, generation, &client) {
            return Err("SSH 连接已变化，请重新刷新");
        }
        if state.uncertain {
            return Err("上次采集结束状态无法确认，已停止采集；重新连接 SSH 后可再尝试");
        }
        if state
            .last_started
            .is_some_and(|at| at.elapsed() < MIN_INTERVAL)
        {
            let mut sample = state
                .cached
                .clone()
                .ok_or("采集间隔至少 5 秒，请稍后刷新")?;
            sample.session = id;
            sample.transport_generation = generation;
            return Ok(sample);
        }
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| "资源采集繁忙，请稍后刷新")?;
        state.last_started = Some(Instant::now());
        state.cached = None;
        // If a caller disappears, the owned worker still closes its channel and
        // records uncertainty before releasing the per-transport gate.
        state.uncertain = true;
        tokio::spawn(async move {
            let _permit = permit;
            let (result, stopped) = collect(&client).await;
            state.uncertain = !stopped;
            let mut sample = result?;
            if !current(&session, generation, &client) {
                return Err("SSH 连接已变化，已丢弃旧采集结果");
            }
            sample.session = id;
            sample.transport_generation = generation;
            sample.sampled_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "本机时钟无效")?
                .as_millis()
                .try_into()
                .map_err(|_| "本机时钟无效")?;
            state.cached = Some(sample.clone());
            Ok(sample)
        })
        .await
        .map_err(|_| "资源采集任务未完成，请重新连接后重试")?
    }
}

fn current(session: &SshSession, generation: u64, client: &Arc<SshConnection>) -> bool {
    session.running()
        && session.transport_generation() == generation
        && session
            .checksum_connection()
            .is_some_and(|now| Arc::ptr_eq(&now, client))
}

async fn collect(client: &SshConnection) -> (Result<ServerResources, &'static str>, bool) {
    let channel = tokio::time::timeout(Duration::from_secs(3), client.open_exec()).await;
    let mut channel = match channel {
        Ok(Ok(channel)) => channel,
        _ => return (Err("服务器不允许采集通道，或打开通道超时"), true),
    };
    let mut stopped = false;
    let mut output = Vec::new();
    let command = format!("/bin/sh -c '{}'", SCRIPT.replace('\'', "'\\''"));
    let result = tokio::time::timeout(SAMPLE_TIMEOUT, async {
        channel
            .request(command.as_bytes())
            .await
            .map_err(|_| "资源采集请求失败")?;
        let mut status = None;
        while let Some(event) = channel.next_event().await {
            match event {
                TerminalEvent::Data {
                    stream: TerminalDataStream::Stdout,
                    data,
                } => {
                    if output.len().saturating_add(data.len()) > 4096 {
                        return Err("资源采集输出超限，已停止");
                    }
                    output.extend_from_slice(&data);
                }
                TerminalEvent::Data { data, .. } if !data.is_empty() => {
                    return Err("采集命令遇到额外错误输出；请检查 Linux 与 shell 环境");
                }
                TerminalEvent::ExitStatus(code) => {
                    stopped = true;
                    status = Some(code);
                }
                TerminalEvent::ExitSignal { .. } => {
                    stopped = true;
                    return Err("采集命令已被服务器终止");
                }
                TerminalEvent::RequestFailed => {
                    stopped = true;
                    return Err("服务器拒绝资源采集命令");
                }
                TerminalEvent::Closed => break,
                _ => {}
            }
        }
        if status != Some(0) {
            return Err("资源面板需要支持 /proc 的 Linux 服务器及可用的 /bin/sh");
        }
        parse::parse(&output).ok_or("服务器指标格式不支持或数值异常，未显示误导数据")
    })
    .await
    .unwrap_or(Err("资源采集超时，已停止自动刷新"));
    let _ = tokio::time::timeout(Duration::from_secs(1), channel.close()).await;
    (result, stopped)
}

#[cfg(test)]
mod tests;
