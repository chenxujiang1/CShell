//! Tunnel definitions survive restart; native grants never restart implicitly.
use crate::{SshSession, SshSessionRegistry, tunnel_store as store};
use cshell_application::*;
use cshell_domain::{SessionId, TunnelId};
use cshell_ipc::{TunnelOperation, TunnelRequest, TunnelResponse, TunnelStatus};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
type Result<T> = std::result::Result<T, &'static str>;
mod dedicated;
pub(crate) use dedicated::start_forward;
#[derive(Clone)]
pub(crate) struct TunnelIpcService {
    catalog: Arc<tokio::sync::Mutex<()>>,
    runtime: Arc<Mutex<BTreeMap<TunnelId, Runner>>>,
    openers: Arc<tokio::sync::Semaphore>,
}
impl Default for TunnelIpcService {
    fn default() -> Self {
        Self {
            catalog: Default::default(),
            runtime: Default::default(),
            openers: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}
struct Runner {
    run: SessionId,
    stop: Option<oneshot::Sender<()>>,
}
impl std::fmt::Debug for TunnelIpcService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelIpcService").finish_non_exhaustive()
    }
}
struct Owner {
    service: TunnelIpcService,
    id: TunnelId,
    run: SessionId,
}
impl Drop for Owner {
    fn drop(&mut self) {
        let mut map = self
            .service
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.get(&self.id).is_some_and(|r| r.run == self.run) {
            map.remove(&self.id);
        }
    }
}
fn uuid(bytes: &[u8]) -> Result<[u8; 16]> {
    let value: [u8; 16] = bytes.try_into().map_err(|_| "invalid tunnel identity")?;
    if value == [0; 16] {
        return Err("nil tunnel identity");
    }
    Ok(value)
}
fn change(snapshot: &mut TunnelSnapshot) -> Result<u64> {
    let previous = snapshot.revision;
    snapshot.changed()?;
    Ok(previous)
}
impl TunnelIpcService {
    fn owned(&self, id: TunnelId, run: Option<SessionId>) -> bool {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .is_some_and(|r| Some(r.run) == run)
    }
    fn own(&self, id: TunnelId, run: SessionId) -> Result<(Owner, oneshot::Receiver<()>)> {
        let mut map = self
            .runtime
            .lock()
            .map_err(|_| "tunnel runtime unavailable")?;
        if map.contains_key(&id) {
            return Err("tunnel operation is still running");
        }
        let (stop, receiver) = oneshot::channel();
        map.insert(
            id,
            Runner {
                run,
                stop: Some(stop),
            },
        );
        Ok((
            Owner {
                service: self.clone(),
                id,
                run,
            },
            receiver,
        ))
    }
    pub(crate) async fn handle(
        &self,
        request: TunnelRequest,
        sessions: &SshSessionRegistry,
        profiles: Option<&Arc<crate::ProfileIpcService>>,
        dedicated_allowed: bool,
    ) -> TunnelResponse {
        if request.operation >= 6 && !dedicated_allowed {
            return TunnelResponse {
                status: TunnelStatus::Unsupported as i32,
                detail: "Dedicated tunnel capability was not negotiated".into(),
                ..Default::default()
            };
        }
        match self
            .handle_inner(request, sessions, profiles, dedicated_allowed)
            .await
        {
            Ok(snapshot) => match serde_json::to_vec(&snapshot) {
                Ok(document_json) if document_json.len() <= MAX_TUNNEL_DOCUMENT => TunnelResponse {
                    document_json,
                    ..Default::default()
                },
                _ => TunnelResponse {
                    status: TunnelStatus::Unavailable as i32,
                    detail: "Tunnel reply unavailable; reload before another mutation".into(),
                    ..Default::default()
                },
            },
            Err(detail) => TunnelResponse {
                status: TunnelStatus::Conflict as i32,
                detail: detail.into(),
                ..Default::default()
            },
        }
    }
    async fn handle_inner(
        &self,
        request: TunnelRequest,
        sessions: &SshSessionRegistry,
        profiles: Option<&Arc<crate::ProfileIpcService>>,
        dedicated_allowed: bool,
    ) -> Result<TunnelSnapshot> {
        let op =
            TunnelOperation::try_from(request.operation).map_err(|_| "unknown tunnel operation")?;
        let mutation = op != TunnelOperation::Get;
        let canonical = TunnelRequest {
            operation: op as i32,
            tunnel_id: if mutation {
                request.tunnel_id.clone()
            } else {
                Vec::new()
            },
            session_id: if matches!(
                op,
                TunnelOperation::Start | TunnelOperation::ConfigureDedicated
            ) {
                request.session_id.clone()
            } else {
                Vec::new()
            },
            expected_revision: if mutation {
                request.expected_revision
            } else {
                0
            },
            confirmed: mutation,
            document_json: if matches!(
                op,
                TunnelOperation::Create
                    | TunnelOperation::Update
                    | TunnelOperation::ConfigureDedicated
            ) {
                request.document_json.clone()
            } else {
                Vec::new()
            },
        };
        if request != canonical
            || mutation && request.expected_revision == 0
            || request.document_json.len() > 16 * 1024
        {
            return Err("invalid tunnel request shape");
        }
        let _catalog = self.catalog.lock().await;
        let mut snapshot = store::read().await?;
        if !dedicated_allowed
            && snapshot
                .items
                .iter()
                .any(|r| r.dedicated.is_some() || r.dedicated_run)
        {
            return Err("catalog requires dedicated tunnel capability; use an updated client");
        }
        if op == TunnelOperation::Get {
            let mut changed = false;
            for row in &mut snapshot.items {
                if matches!(
                    row.state,
                    TunnelState::Starting | TunnelState::Running | TunnelState::Stopping
                ) && !self.owned(row.id, row.run)
                {
                    row.state = TunnelState::Unknown;
                    row.detail =
                        "Native owner interrupted; inspect and explicitly stop the saved binding"
                            .into();
                    changed = true;
                }
            }
            if changed {
                let previous = change(&mut snapshot)?;
                store::write(&snapshot, previous).await?;
            }
            return Ok(snapshot);
        }
        if snapshot.revision != request.expected_revision {
            return Err("tunnel catalog changed; reload before confirming");
        }
        let id = TunnelId::from_bytes(uuid(&request.tunnel_id)?);
        if op == TunnelOperation::Create {
            if snapshot.items.len() >= MAX_TUNNELS || snapshot.items.iter().any(|r| r.id == id) {
                return Err("tunnel catalog full or identity already saved");
            }
            let definition: TunnelDefinition = serde_json::from_slice(&request.document_json)
                .map_err(|_| "invalid tunnel definition")?;
            definition.validate()?;
            snapshot.items.push(TunnelRecord {
                dedicated: None,
                dedicated_run: false,
                reconnect_attempt: 0,
                id,
                definition,
                state: TunnelState::Stopped,
                session: None,
                run: None,
                peer: None,
                bound_address: None,
                bound_port: None,
                detail: "Saved; explicit start required".into(),
            });
        } else {
            let index = snapshot
                .items
                .iter()
                .position(|r| r.id == id)
                .ok_or("tunnel does not exist")?;
            let row = snapshot.items[index].clone();
            match op {
                TunnelOperation::ConfigureDedicated => {
                    if row.state != TunnelState::Stopped || self.owned(id, row.run) {
                        return Err("stop the tunnel before freezing a dedicated connection");
                    }
                    let config: TunnelDedicatedConfiguration =
                        serde_json::from_slice(&request.document_json)
                            .map_err(|_| "invalid dedicated configuration")?;
                    config.validate()?;
                    let reference = SessionId::from_bytes(uuid(&request.session_id)?);
                    let reference = sessions
                        .get(reference)
                        .map_err(|_| "verified reference session unavailable")?;
                    if !reference.running() || reference.profile_id() != Some(config.profile_id) {
                        return Err("reference session must use this saved SSH Profile");
                    }
                    let peer = reference
                        .clipboard_trust()
                        .cloned()
                        .ok_or("verified reference peer unavailable")?;
                    if row.peer.as_ref().is_some_and(|p| p != &peer) {
                        return Err("existing tunnel peer changed; create a separate definition");
                    }
                    let profiles = profiles.ok_or("Profile service unavailable")?;
                    let catalog = profiles
                        .service
                        .load()
                        .await
                        .map_err(|_| "Profile catalog unavailable")?
                        .snapshot();
                    if catalog.revision != config.profile_revision {
                        return Err("Profile catalog changed; reload before freezing");
                    }
                    let target = catalog
                        .ssh_connections
                        .iter()
                        .find(|c| c.profile_id == config.profile_id)
                        .cloned()
                        .ok_or("SSH Profile missing")?;
                    let plan = crate::ssh_route::SshConnectionPlan::new(
                        String::new(),
                        target,
                        &catalog.ssh_connections,
                    )
                    .map_err(|_| "invalid frozen route")?;
                    if !plan.matches_captured_peer(&peer) {
                        return Err("saved route differs from the verified reference session");
                    }
                    let frozen = TunnelDedicatedPlan {
                        configuration: config,
                        reference_session: reference.id(),
                        target: plan.target,
                        jumps: plan.jumps,
                    };
                    frozen.validate()?;
                    snapshot.items[index].peer = Some(peer);
                    snapshot.items[index].dedicated = Some(frozen);
                    snapshot.items[index].reconnect_attempt = 0;
                    snapshot.items[index].detail="Dedicated route frozen; inspect it and explicitly start, no authentication performed".into();
                }
                TunnelOperation::ClearDedicated => {
                    if row.state != TunnelState::Stopped || self.owned(id, row.run) {
                        return Err("stop dedicated work before clearing its plan");
                    }
                    snapshot.items[index].dedicated = None;
                    snapshot.items[index].reconnect_attempt = 0;
                    snapshot.items[index].detail =
                        "Dedicated plan removed; verified peer retained".into();
                }
                TunnelOperation::StartDedicated => {
                    if row.state != TunnelState::Stopped || self.owned(id, row.run) {
                        return Err(
                            "dedicated tunnel must be stopped; unknown starts are never replayed",
                        );
                    }
                    row.dedicated
                        .as_ref()
                        .ok_or("freeze a dedicated plan first")?
                        .validate()?;
                    let profiles = profiles.cloned().ok_or("Profile service unavailable")?;
                    profiles
                        .vault
                        .ensure_owned()
                        .map_err(|_| "credential storage is owned by another daemon")?;
                    let mut locked = profiles.vault.cancellation();
                    let epoch = *locked.borrow_and_update();
                    if snapshot
                        .items
                        .iter()
                        .filter(|r| r.state != TunnelState::Stopped)
                        .count()
                        >= 32
                    {
                        return Err("active/unknown tunnel budget reached");
                    }
                    let run = SessionId::new();
                    let (owner, stop) = self.own(id, run)?;
                    let saved = &mut snapshot.items[index];
                    saved.state = TunnelState::Starting;
                    saved.session = Some(SessionId::new());
                    saved.run = Some(run);
                    saved.dedicated_run = true;
                    saved.reconnect_attempt = 0;
                    saved.bound_address = None;
                    saved.bound_port = None;
                    saved.detail =
                        "Dedicated start intent saved; new SSH transport authentication pending"
                            .into();
                    let previous = change(&mut snapshot)?;
                    store::write(&snapshot, previous).await?;
                    let service = self.clone();
                    let record = snapshot.items[index].clone();
                    tokio::spawn(async move {
                        let _owner = owner;
                        service
                            .run_dedicated(record, profiles, locked, epoch, stop)
                            .await;
                    });
                    return Ok(snapshot);
                }
                TunnelOperation::Update | TunnelOperation::Remove => {
                    if row.state != TunnelState::Stopped || self.owned(id, row.run) {
                        return Err("stop and resolve the tunnel before editing or removing");
                    }
                    if op == TunnelOperation::Remove {
                        snapshot.items.remove(index);
                    } else {
                        let definition: TunnelDefinition =
                            serde_json::from_slice(&request.document_json)
                                .map_err(|_| "invalid tunnel definition")?;
                        definition.validate()?;
                        snapshot.items[index].definition = definition;
                        snapshot.items[index].detail =
                            "Updated; same peer binding retained, explicit start required".into();
                    }
                }
                TunnelOperation::Start => {
                    if row.dedicated.is_some() {
                        return Err("clear the dedicated plan before using an interactive session");
                    }
                    if row.state != TunnelState::Stopped || self.owned(id, row.run) {
                        return Err("tunnel is not stopped; unknown starts are never replayed");
                    }
                    let session = SessionId::from_bytes(uuid(&request.session_id)?);
                    let target = sessions
                        .get(session)
                        .map_err(|_| "SSH session unavailable")?;
                    if !target.running() {
                        return Err("SSH session is not running");
                    }
                    let peer = target
                        .clipboard_trust()
                        .cloned()
                        .ok_or("verified SSH peer unavailable")?;
                    if row.peer.as_ref().is_some_and(|p| p != &peer) {
                        return Err(
                            "saved tunnel peer changed; create a separately reviewed definition",
                        );
                    }
                    if snapshot
                        .items
                        .iter()
                        .filter(|r| r.state != TunnelState::Stopped)
                        .count()
                        >= 32
                        || snapshot
                            .items
                            .iter()
                            .filter(|r| {
                                r.state != TunnelState::Stopped && r.session == Some(session)
                            })
                            .count()
                            >= 8
                    {
                        return Err(
                            "tunnel active/unknown budget reached; explicitly stop old bindings",
                        );
                    }
                    if row.definition.kind == TunnelKind::Remote
                        && snapshot.items.iter().any(|r| {
                            r.id != id
                                && r.session == Some(session)
                                && r.state != TunnelState::Stopped
                                && r.definition.kind == TunnelKind::Remote
                                && (r.definition.bind_port == 0 && r.bound_port.is_none()
                                    || r.definition.bind_address.parse::<std::net::IpAddr>().ok()
                                        == row.definition.bind_address.parse().ok()
                                        && (row.definition.bind_port != 0
                                            && (r.definition.bind_port
                                                == row.definition.bind_port
                                                || r.bound_port == Some(row.definition.bind_port))))
                        })
                    {
                        return Err("remote binding overlaps an active or unconfirmed grant");
                    }
                    let run = SessionId::new();
                    let (owner, stop) = self.own(id, run)?;
                    let saved = &mut snapshot.items[index];
                    saved.state = TunnelState::Starting;
                    saved.session = Some(session);
                    saved.run = Some(run);
                    saved.peer = Some(peer);
                    saved.bound_address = None;
                    saved.bound_port = None;
                    saved.detail = "Start intent saved; native acknowledgment pending".into();
                    let previous = change(&mut snapshot)?;
                    store::write(&snapshot, previous).await?;
                    let service = self.clone();
                    let record = snapshot.items[index].clone();
                    tokio::spawn(async move {
                        let _owner = owner;
                        service.run(record, target, stop).await;
                    });
                    return Ok(snapshot);
                }
                TunnelOperation::Stop => {
                    if row.state == TunnelState::Stopped {
                        return Err("tunnel already stopped");
                    }
                    if row.state == TunnelState::Stopping && self.owned(id, row.run) {
                        return Err("stop is already pending; query without replaying");
                    }
                    snapshot.items[index].state = TunnelState::Stopping;
                    snapshot.items[index].detail = "Stop intent saved; confirmation pending".into();
                    let previous = change(&mut snapshot)?;
                    store::write(&snapshot, previous).await?;
                    if self.owned(id, row.run) {
                        if let Some(stop) = self
                            .runtime
                            .lock()
                            .map_err(|_| "tunnel runtime unavailable")?
                            .get_mut(&id)
                            .and_then(|r| r.stop.take())
                        {
                            let _ = stop.send(());
                        }
                    } else {
                        let run = row.run.ok_or("missing old tunnel run")?;
                        let (owner, _stop) = self.own(id, run)?;
                        let target = row.session.and_then(|s| sessions.get(s).ok());
                        let service = self.clone();
                        tokio::spawn(async move {
                            let _owner = owner;
                            service.cleanup(row, target).await;
                        });
                    }
                    return Ok(snapshot);
                }
                _ => return Err("unsupported tunnel mutation"),
            }
        }
        let previous = change(&mut snapshot)?;
        store::write(&snapshot, previous).await?;
        Ok(snapshot)
    }
    async fn running(&self, row: &TunnelRecord, handle: &cshell_ssh::ForwardHandle) -> Result<()> {
        let _catalog = self.catalog.lock().await;
        let mut snapshot = store::read().await?;
        let saved = snapshot
            .items
            .iter_mut()
            .find(|r| {
                r.id == row.id
                    && r.run == row.run
                    && r.session == row.session
                    && r.state == TunnelState::Starting
            })
            .ok_or("tunnel start interrupted")?;
        saved.state = TunnelState::Running;
        saved.bound_address = Some(handle.bound_address().into());
        saved.bound_port = Some(handle.bound_port());
        saved.detail =
            "Native listener acknowledged; closing the SSH session stops this tunnel".into();
        let previous = change(&mut snapshot)?;
        store::write(&snapshot, previous).await
    }
    async fn finish(&self, row: &TunnelRecord, stopped: bool, detail: &str) {
        let _catalog = self.catalog.lock().await;
        let result = async {
            let mut snapshot = store::read().await?;
            let saved = snapshot
                .items
                .iter_mut()
                .find(|r| r.id == row.id && r.run == row.run)
                .ok_or("tunnel ownership changed")?;
            saved.state = if stopped {
                TunnelState::Stopped
            } else {
                TunnelState::Unknown
            };
            saved.detail = detail.into();
            if stopped {
                saved.session = None;
                saved.run = None;
                saved.dedicated_run = false;
            }
            let previous = change(&mut snapshot)?;
            store::write(&snapshot, previous).await
        };
        let _ = result.await; // A failed final save stays orphan/Unknown on the next read.
    }
    async fn run(
        &self,
        row: TunnelRecord,
        target: Arc<SshSession>,
        mut stop: oneshot::Receiver<()>,
    ) {
        let start = tokio::select! {
            biased;
            _ = &mut stop => Err(("Start cancelled before acknowledgment", row.definition.kind != TunnelKind::Remote)),
            result = tokio::time::timeout(Duration::from_secs(30), target.start_tunnel(&row.definition)) => match result {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(error)) if error.is_request_denied() => Err(("SSH server explicitly rejected the listener request", true)),
                _ => Err(("Start failed or unconfirmed; do not replay", row.definition.kind != TunnelKind::Remote)),
            }
        };
        let mut handle = match start {
            Ok(h) => h,
            Err((detail, stopped)) => {
                self.finish(&row, stopped, detail).await;
                return;
            }
        };
        if self.running(&row, &handle).await.is_err() {
            let stopped = close(handle).await;
            self.finish(
                &row,
                stopped,
                "Start receipt unavailable; native cleanup attempted, inspect before continuing",
            )
            .await;
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {biased;
                _=&mut stop=>{let stopped=close(handle).await;self.finish(&row,stopped,if stopped{"Tunnel stopped; listener and active bridges closed"}else{"Stop unconfirmed; inspect and explicitly clean the original remote grant"}).await;return;}
                result=handle.wait()=>{let stopped=result.is_ok() || row.definition.kind!=TunnelKind::Remote;self.finish(&row,stopped,if stopped{"Tunnel worker ended; no automatic restart"}else{"Remote worker ended without confirmed cancellation"}).await;return;}
                _=tick.tick()=>{if !target.running(){let stopped=close(handle).await;self.finish(&row,stopped,if stopped{"SSH session ended; tunnel stopped"}else{"SSH session ended; remote cancellation remains unconfirmed"}).await;return;}}
            }
        }
    }
    async fn cleanup(&self, row: TunnelRecord, target: Option<Arc<SshSession>>) {
        let result=async{
            if row.dedicated_run { return Ok(()); } // No owned transport is rebound through the interactive registry.
            if row.definition.kind!=TunnelKind::Remote {return Ok(());}
            let Some(target)=target else{return Ok(());}; // Old session IDs are never rebound to another transport.
            if target.clipboard_trust()!=row.peer.as_ref(){return Err("original tunnel peer changed");}
            let port=row.bound_port.or_else(||(row.definition.bind_port!=0).then_some(row.definition.bind_port)).ok_or("allocated remote port unknown; close the original SSH session before clearing the binding")?;
            let address=row.bound_address.as_deref().unwrap_or(&row.definition.bind_address);
            tokio::time::timeout(Duration::from_secs(30),target.cancel_remote_tunnel(address,port)).await.map_err(|_| "remote cancellation timed out")?.map_err(|_| "remote cancellation unconfirmed")
        }.await;
        self.finish(
            &row,
            result.is_ok(),
            match result {
                Ok(()) => {
                    "Old binding stopped or its original session is absent; no reconnect performed"
                }
                Err(e) => e,
            },
        )
        .await;
    }
}
async fn close(handle: cshell_ssh::ForwardHandle) -> bool {
    dedicated::close_forward(handle).await.0
}
