use super::*;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(super) struct ActiveTransport {
    pub lease: crate::shared_ssh::TransportLease,
    pub client: Arc<SshConnection>,
    pub jump: Option<crate::ssh_route::JumpConnection>,
    pub writer: Arc<SshTerminalWriter>,
}

#[derive(Clone)]
pub(super) struct RecoveryPlan {
    pub plan: crate::ssh_route::SshConnectionPlan,
    pub known_hosts: PathBuf,
    pub vault: Arc<crate::vault_control::VaultController>,
    pub peer: cshell_domain::ClipboardTrust,
    pub terminal_type: String,
    pub directory_marker: String,
}

impl std::fmt::Debug for RecoveryPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryPlan")
            .field("profile", &self.plan.target.profile_id)
            .finish_non_exhaustive()
    }
}

pub(super) async fn restore(
    recovery: &RecoveryPlan,
    active: &RwLock<ActiveTransport>,
    size: &Mutex<TerminalSize>,
    ingress: &PipelineIngress,
    detail: &Mutex<String>,
    closed: &AtomicBool,
    epoch: &AtomicU64,
    input_generation: &AtomicU64,
    attempts_used: &mut u32,
) -> Option<cshell_ssh::SshTerminalReader> {
    let options = &recovery.plan.target.options;
    let mut cancellation = recovery.vault.cancellation();
    let vault_epoch = *cancellation.borrow();
    if *attempts_used >= options.reconnect_attempts {
        replace_terminal_detail(
            detail,
            format!(
                "自动重连已停止（{}/{}）：连接持续不稳定，已达到重试上限。",
                attempts_used, options.reconnect_attempts
            ),
        );
        return None;
    }
    while *attempts_used < options.reconnect_attempts {
        *attempts_used += 1;
        let attempt = *attempts_used;
        replace_terminal_detail(
            detail,
            format!(
                "SSH 意外断线，{} 秒后自动重连（{attempt}/{}）…",
                options.reconnect_delay_secs, options.reconnect_attempts
            ),
        );
        tokio::select! {
            biased;
            _ = cancellation.changed() => {
                replace_terminal_detail(detail, "自动重连已停止：凭据库已锁定。".into());
                return None;
            }
            _ = tokio::time::sleep(Duration::from_secs(u64::from(options.reconnect_delay_secs))) => {}
        }
        replace_terminal_detail(
            detail,
            format!(
                "正在自动重连 SSH（{attempt}/{}）…",
                options.reconnect_attempts
            ),
        );
        let connect = async {
            let connected = crate::ssh_route::connect_profile_pinned(
                &recovery.plan,
                &recovery.known_hosts,
                &recovery.vault,
                &recovery.peer,
            )
            .await?;
            let dimensions = *size
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let terminal = connected
                .client
                .open_terminal_tracked(
                    u32::from(dimensions.rows),
                    u32::from(dimensions.cols),
                    &recovery.terminal_type,
                    Some(&recovery.directory_marker),
                )
                .await
                .map_err(|error| {
                    let retryable = error.is_network_failure();
                    let mut failure =
                        crate::session_ipc::ssh_launch_failure(SshSessionError::Ssh(error));
                    failure.retryable_network = retryable;
                    failure
                })?;
            let (reader, writer) = terminal.split();
            let lease = crate::shared_ssh::TransportLease::new(connected);
            let writer=Arc::new(writer);
            lease.register_writer(writer.clone());
            Ok::<_, crate::session_ipc::SshLaunchFailure>((
                reader,
                ActiveTransport {
                    client: lease.client(),
                    jump: lease.jump(),
                    lease,
                    writer,
                },
            ))
        };
        let result = tokio::select! {
            biased;
            _ = cancellation.changed() => {
                replace_terminal_detail(detail, "自动重连已停止：凭据库已锁定。".into());
                return None;
            }
            result = connect => result,
        };
        match result {
            Ok((reader, transport)) => {
                // Reset old application modes before exposing the fresh shell.
                let ingress = ingress.clone();
                let dimensions = *size
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let generation = match tokio::task::spawn_blocking(move || {
                    ingress.reset_connection_blocking(dimensions)
                })
                .await
                {
                    Ok(Ok(generation)) => generation,
                    _ => {
                        transport.lease.release().await;
                        replace_terminal_detail(detail, "自动重连已停止：终端存储不可用。".into());
                        return None;
                    }
                };
                if recovery
                    .vault
                    .commit_auth(vault_epoch, || {
                        *active
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = transport.clone();
                        input_generation.store(generation, Ordering::Release);
                        epoch.fetch_add(1, Ordering::AcqRel);
                        closed.store(false, Ordering::Release);
                    })
                    .is_err()
                {
                    transport.lease.release().await;
                    replace_terminal_detail(detail, "自动重连已停止：凭据库已锁定。".into());
                    return None;
                }
                replace_terminal_detail(
                    detail,
                    "SSH 已自动重连；已打开新的远端 shell，未重放之前的输入。".into(),
                );
                return Some(reader);
            }
            Err(failure) => {
                if !failure.retryable_network || attempt == options.reconnect_attempts {
                    replace_terminal_detail(
                        detail,
                        format!(
                            "自动重连已停止（{attempt}/{}）：{:?}: {}",
                            options.reconnect_attempts, failure.code, failure.detail
                        ),
                    );
                    return None;
                }
            }
        }
    }
    None
}
