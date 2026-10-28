use crate::{
    ProfileIpcService,
    ssh_route::{JumpConnection, SshConnectionPlan, connect_frozen_target},
    task_output::TaskOutputStore,
};
use cshell_application::{
    TaskCancellation, TaskConnection, TaskExecutor, TaskSnapshot, TaskStopReason,
};
use cshell_domain::{HostId, TaskRunId};
use cshell_ssh::{SshConnection, SshExecChannel, TerminalDataStream, TerminalEvent};
use std::{fmt, sync::Arc};

pub struct SshTaskExecutor {
    profiles: Arc<ProfileIpcService>,
    output: TaskOutputStore,
}
impl fmt::Debug for SshTaskExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTaskExecutor").finish_non_exhaustive()
    }
}
impl SshTaskExecutor {
    pub fn new(profiles: Arc<ProfileIpcService>, output: TaskOutputStore) -> Self {
        Self { profiles, output }
    }
}
#[async_trait::async_trait]
impl TaskExecutor for SshTaskExecutor {
    async fn connect(
        &self,
        snapshot: TaskSnapshot,
        host: HostId,
        cancel: TaskCancellation,
    ) -> Result<Box<dyn TaskConnection>, TaskStopReason> {
        let targets = snapshot.targets();
        let target = targets
            .resolved_connection(host)
            .ok_or(TaskStopReason::ConnectionFailure)?;
        let original = targets
            .credential_template(host)
            .ok_or(TaskStopReason::ConnectionFailure)?;
        let plan = SshConnectionPlan::new(String::new(), target, targets.connections())
            .map_err(|_| TaskStopReason::ConnectionFailure)?;
        let path = self
            .profiles
            .known_hosts_path()
            .map_err(|_| TaskStopReason::ConnectionFailure)?;
        let mut locked = self.profiles.vault.cancellation();
        let generation = *locked.borrow_and_update();
        let connected = tokio::select! {
            biased;
            reason = cancel.cancelled() => return Err(reason),
            _ = locked.changed() => return Err(TaskStopReason::ConnectionFailure),
            connected = connect_frozen_target(&plan, original, &path, &self.profiles.vault) => connected.map_err(|_| TaskStopReason::ConnectionFailure)?,
        };
        let connection = SshTaskConnection {
            client: Arc::new(connected.client),
            jump: connected.jump,
            channel: None,
            output: self.output.clone(),
            run: snapshot.id(),
            host,
            next_step: 0,
        };
        if cancel.reason().is_some() || *locked.borrow() != generation {
            return Err(TaskStopReason::ConnectionFailure);
        }
        Ok(Box::new(connection))
    }
}
struct SshTaskConnection {
    client: Arc<SshConnection>,
    jump: Option<JumpConnection>,
    channel: Option<SshExecChannel>,
    output: TaskOutputStore,
    run: TaskRunId,
    host: HostId,
    next_step: usize,
}
impl fmt::Debug for SshTaskConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTaskConnection").finish_non_exhaustive()
    }
}
impl Drop for SshTaskConnection {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let channel = self.channel.take();
            let jump = self.jump.take();
            runtime.spawn(async move {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    if let Some(channel) = channel {
                        let _ = channel.close().await;
                    }
                    let _ = client.disconnect().await;
                    if let Some(jump) = jump {
                        jump.disconnect().await;
                    }
                })
                .await;
            });
        }
    }
}
#[async_trait::async_trait]
impl TaskConnection for SshTaskConnection {
    async fn execute(
        &mut self,
        step: usize,
        command: &str,
        cancel: TaskCancellation,
    ) -> Result<u32, TaskStopReason> {
        if step != self.next_step
            || self.channel.is_some()
            || command.len() > cshell_application::MAX_TASK_COMMAND_BYTES
        {
            return Err(TaskStopReason::ExecRejected);
        }
        if let Some(reason) = cancel.reason() {
            return Err(reason);
        }
        let mut output = self
            .output
            .create(self.run, self.host, step)
            .await
            .map_err(|_| TaskStopReason::OutputFailure)?;
        self.channel = Some(
            self.client
                .open_exec()
                .await
                .map_err(|_| TaskStopReason::ExecRejected)?,
        );
        let channel = self.channel.as_mut().ok_or(TaskStopReason::TransportLost)?;
        if let Some(reason) = cancel.reason() {
            return Err(reason);
        }
        // Errors during send are uncertain; the request may already be on wire.
        channel
            .request(command.as_bytes())
            .await
            .map_err(|_| TaskStopReason::TransportLost)?;
        let mut accepted = false;
        let mut exit = None;
        loop {
            let event = tokio::select! {
                biased;
                reason = cancel.cancelled() => return Err(reason),
                event = channel.next_event() => event,
            };
            match event {
                Some(TerminalEvent::RequestSucceeded) => accepted = true,
                Some(TerminalEvent::RequestFailed) if !accepted => {
                    return Err(TaskStopReason::ExecRejected);
                }
                Some(TerminalEvent::RequestFailed | TerminalEvent::ExitSignal { .. }) => {
                    return Err(TaskStopReason::TransportLost);
                }
                Some(TerminalEvent::Data { stream, data }) => {
                    accepted = true;
                    let stderr = match stream {
                        TerminalDataStream::Stdout => false,
                        TerminalDataStream::Extended(1) => true,
                        _ => return Err(TaskStopReason::OutputFailure),
                    };
                    // One received packet is bounded by the provider. Disk I/O
                    // applies backpressure before another event is consumed.
                    for chunk in data.chunks(crate::task_output::MAX_TASK_OUTPUT_PAGE) {
                        output
                            .write(stderr, chunk)
                            .await
                            .map_err(|_| TaskStopReason::OutputFailure)?;
                    }
                }
                Some(TerminalEvent::ExitStatus(code)) => {
                    accepted = true;
                    if exit.replace(code).is_some() {
                        return Err(TaskStopReason::TransportLost);
                    }
                }
                Some(TerminalEvent::Closed) => break,
                None => return Err(TaskStopReason::TransportLost),
                Some(_) => {}
            }
        }
        let code = exit.ok_or(TaskStopReason::TransportLost)?;
        output
            .finish()
            .await
            .map_err(|_| TaskStopReason::OutputFailure)?;
        self.channel = None;
        self.next_step += 1;
        Ok(code)
    }
    async fn close(&mut self) {
        if let Some(channel) = self.channel.take() {
            let _ = channel.close().await;
        }
        let _ = self.client.disconnect().await;
        if let Some(jump) = self.jump.take() {
            jump.disconnect().await;
        }
    }
}
