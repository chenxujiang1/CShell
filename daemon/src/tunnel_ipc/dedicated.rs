//! Independent, frozen SSH transports; recovery never reuses an uncertain old grant.
use super::*;
use crate::ssh_route::{JumpConnection, SshConnectionPlan, connect_profile_pinned};
use cshell_ssh::{ForwardHandle, ForwardLimits, RemoteForwardTarget, SshConnection, SshError};

struct DedicatedTransport {
    client: Arc<SshConnection>,
    jump: Option<JumpConnection>,
}
async fn recovery_expired(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}
impl DedicatedTransport {
    async fn close(&self) -> bool {
        tokio::time::timeout(Duration::from_secs(20), async {
            let leaf = self.client.disconnect_confirmed().await;
            let jumps = if let Some(jump) = &self.jump {
                jump.disconnect_confirmed().await
            } else {
                true
            };
            leaf && jumps
        })
        .await
        .unwrap_or(false)
    }
}
impl Drop for DedicatedTransport {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let jump = self.jump.clone();
            runtime.spawn(async move {
                let _ = client.disconnect_confirmed().await;
                if let Some(jump) = jump {
                    let _ = jump.disconnect_confirmed().await;
                }
            });
        }
    }
}
pub(crate) async fn start_forward(
    client: &Arc<SshConnection>,
    definition: &TunnelDefinition,
) -> std::result::Result<ForwardHandle, SshError> {
    let limits = ForwardLimits {
        max_connections: definition.max_connections as usize,
        pending_remote_channels: definition.max_connections as usize,
        socks_handshake_timeout: Duration::from_millis(u64::from(definition.socks_timeout_ms)),
    };
    let ip = definition
        .bind_address
        .parse()
        .map_err(|_| SshError::InvalidForwardConfig("invalid bind address".into()))?;
    match definition.kind {
        TunnelKind::Local => {
            client
                .start_local_forward(
                    std::net::SocketAddr::new(ip, definition.bind_port),
                    definition.target_host.clone(),
                    definition.target_port,
                    limits,
                )
                .await
        }
        TunnelKind::Dynamic => {
            client
                .start_dynamic_forward(std::net::SocketAddr::new(ip, definition.bind_port), limits)
                .await
        }
        TunnelKind::Remote => {
            client
                .start_remote_forward(
                    definition.bind_address.clone(),
                    definition.bind_port,
                    RemoteForwardTarget {
                        host: definition.target_host.clone(),
                        port: definition.target_port,
                    },
                    limits,
                )
                .await
        }
    }
}
pub(super) async fn close_forward(mut handle: ForwardHandle) -> (bool, bool) {
    let acknowledged = matches!(
        tokio::time::timeout(Duration::from_secs(5), handle.stop()).await,
        Ok(Ok(()))
    );
    if !acknowledged && !handle.is_finished() {
        if let Some(abort) = handle.abort_handle() {
            abort.abort();
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), handle.wait()).await;
    }
    (acknowledged, handle.is_finished())
}
impl TunnelIpcService {
    async fn dedicated_attempt(&self, row: &TunnelRecord, attempt: u32) -> Result<TunnelRecord> {
        let _catalog = self.catalog.lock().await;
        let mut snapshot = store::read().await?;
        let saved = snapshot
            .items
            .iter_mut()
            .find(|r| {
                r.id == row.id
                    && r.run == row.run
                    && r.dedicated_run
                    && r.state != TunnelState::Stopping
                    && r.definition == row.definition
                    && r.dedicated == row.dedicated
                    && r.peer == row.peer
            })
            .ok_or("dedicated authorization changed or stop is pending")?;
        saved.state = TunnelState::Starting;
        saved.session = Some(SessionId::new());
        saved.reconnect_attempt = attempt;
        saved.bound_address = None;
        saved.bound_port = None;
        saved.detail = format!(
            "Dedicated attempt {attempt} intent saved; old transport closed, waiting for the reviewed retry delay"
        );
        let record = saved.clone();
        let previous = change(&mut snapshot)?;
        store::write(&snapshot, previous).await?;
        Ok(record)
    }
    pub(super) async fn run_dedicated(
        &self,
        mut row: TunnelRecord,
        profiles: Arc<crate::ProfileIpcService>,
        mut locked: tokio::sync::watch::Receiver<u64>,
        epoch: u64,
        mut stop: oneshot::Receiver<()>,
    ) {
        let Some(frozen) = row.dedicated.clone() else {
            self.finish(
                &row,
                true,
                "Missing frozen dedicated plan; no connection opened",
            )
            .await;
            return;
        };
        let Some(peer) = row.peer.clone() else {
            self.finish(&row, true, "Missing frozen peer; no connection opened")
                .await;
            return;
        };
        let plan = SshConnectionPlan {
            title: String::new(),
            target: frozen.target.clone(),
            jumps: frozen.jumps.clone(),
        };
        if !plan.matches_captured_peer(&peer) {
            self.finish(
                &row,
                true,
                "Frozen route and captured peer differ; no connection opened",
            )
            .await;
            return;
        }
        let mut attempt = 0u32;
        let mut recovery_deadline = None;
        loop {
            let opening_deadline = tokio::time::Instant::now() + Duration::from_secs(300);
            if profiles.vault.commit_auth(epoch, || ()).is_err() {
                self.finish(
                    &row,
                    true,
                    "Authentication generation changed; explicitly start again after reviewing",
                )
                .await;
                return;
            }
            let permit = tokio::select! {biased;_=&mut stop=>None,_=locked.changed()=>None,_=tokio::time::sleep_until(opening_deadline)=>None,_=recovery_expired(recovery_deadline)=>None,p=self.openers.clone().acquire_owned()=>p.ok()};
            let Some(permit) = permit else {
                self.finish(&row, true, "Dedicated start stopped before authentication")
                    .await;
                return;
            };
            let connection = async {
                let path = profiles
                    .known_hosts_path()
                    .map_err(|_| ("known_hosts unavailable", false))?;
                let connected=tokio::time::timeout(Duration::from_secs(300),connect_profile_pinned(&plan,&path,&profiles.vault,&peer)).await.map_err(|_|("Dedicated authentication deadline exceeded; explicit restart required",false))?.map_err(|f|if f.retryable_network{("Dedicated SSH network connection failed",true)}else{("Dedicated SSH trust, credential or authentication check refused; review before restarting",false)})?;
                let actual = plan
                    .verified_peer(&connected.client, connected.jump.as_ref())
                    .map_err(|_| ("Dedicated peer unavailable", false))?;
                let owned = DedicatedTransport {
                    client: Arc::new(connected.client),
                    jump: connected.jump,
                };
                if actual != peer {
                    let _ = owned.close().await;
                    return Err((
                        "Verified dedicated peer changed; no listener created",
                        false,
                    ));
                }
                profiles.vault.commit_auth(epoch, || owned).map_err(|_| {
                    (
                        "Authentication generation changed; no listener created",
                        false,
                    )
                })
            };
            let connected = tokio::select! {biased;_=&mut stop=>Err(("Dedicated authentication stopped; no listener created",false)),_=locked.changed()=>Err(("Authentication generation changed; no listener created",false)),_=tokio::time::sleep_until(opening_deadline)=>Err(("Dedicated opening deadline expired; no listener created",false)),_=recovery_expired(recovery_deadline)=>Err(("Recovery window expired; no listener created",false)),r=connection=>r};
            drop(permit);
            let (retry, detail) = match connected {
                Err((detail, retry)) => (retry, detail),
                Ok(owned) => {
                    let handle = match tokio::select! {biased;_=&mut stop=>Err("Dedicated listener creation stopped"),_=locked.changed()=>Err("Authentication generation changed during listener creation"),_=tokio::time::sleep_until(opening_deadline)=>Err("Dedicated opening deadline expired during listener creation"),_=recovery_expired(recovery_deadline)=>Err("Recovery window expired during listener creation"),r=tokio::time::timeout(Duration::from_secs(30),start_forward(&owned.client,&row.definition))=>match r{Ok(Ok(h))=>Ok(h),_=>Err("Dedicated listener creation refused or unconfirmed")}}
                    {
                        Ok(handle) => handle,
                        Err(detail) => {
                            let closed = owned.close().await;
                            self.finish(&row, closed, detail).await;
                            return;
                        }
                    };
                    if self.running(&row, &handle).await.is_err() {
                        let (_, finished) = close_forward(handle).await;
                        let closed = owned.close().await;
                        self.finish(&row,finished&&closed,"Listener receipt unavailable; dedicated cleanup attempted, inspect before restarting").await;
                        return;
                    }
                    let mut tick = tokio::time::interval(Duration::from_millis(500));
                    let unexpected = loop {
                        tokio::select! {biased;
                            _=&mut stop=>break false,
                            _=locked.changed()=>break false,
                            _=tick.tick()=>{if owned.client.transport_closed() || handle.is_finished(){break true;}}
                        }
                    };
                    let (_, finished) = close_forward(handle).await;
                    let closed = owned.close().await;
                    if !finished || !closed {
                        self.finish(&row,false,"Dedicated transport/bridge closure unconfirmed; automatic recovery blocked").await;
                        return;
                    }
                    if !unexpected {
                        self.finish(&row,true,"Dedicated tunnel stopped; listener, bridges and private SSH path closed").await;
                        return;
                    }
                    (
                        true,
                        "Dedicated transport ended; old listener and private SSH path closed",
                    )
                }
            };
            let Some(policy) = frozen
                .configuration
                .reconnect
                .as_ref()
                .filter(|p| retry && attempt < p.max_retries)
            else {
                self.finish(&row, true, detail).await;
                return;
            };
            let deadline = *recovery_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(600));
            if tokio::time::Instant::now() >= deadline {
                self.finish(
                    &row,
                    true,
                    "Dedicated recovery window expired; explicit restart required",
                )
                .await;
                return;
            }
            attempt += 1;
            row = match self.dedicated_attempt(&row, attempt).await {
                Ok(row) => row,
                Err(_) => {
                    self.finish(
                        &row,
                        true,
                        "Recovery metadata unavailable or stop pending; no new transport opened",
                    )
                    .await;
                    return;
                }
            };
            let continue_run = tokio::select! {biased;_=&mut stop=>false,_=locked.changed()=>false,_=tokio::time::sleep_until(deadline)=>false,_=tokio::time::sleep(Duration::from_millis(u64::from(policy.delay_ms)))=>true};
            if !continue_run {
                self.finish(
                    &row,
                    true,
                    "Dedicated recovery stopped or expired; no new transport opened",
                )
                .await;
                return;
            }
        }
    }
}
