//! Explicit, one-use live grants. No source, parameter, output or diagnostic
//! enters durable state, tracing, permission receipts or run reports.
use crate::{LocalSessionRegistry, ProfileIpcService, SshSessionRegistry, TaskIpcService};
use cshell_domain::*;
use cshell_ipc::{
    ScriptControlRequest, ScriptControlResponse, ScriptControlStatus as Status, TaskOperation,
    TaskRequest, TaskStatus,
};
use cshell_script_runtime::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};
mod broker;

struct Review {
    proposal: ScriptLiveProposal,
    receipt: ScriptLiveReview,
    expires: Instant,
    session: Option<BoundSession>,
}
impl Drop for Review {
    fn drop(&mut self) {
        self.proposal.document.source.zeroize();
    }
}
struct Run {
    revoke: ScriptRevocation,
    view: ScriptLiveView,
    trigger_key: Option<(SessionId, String)>,
}
impl Drop for Run {
    fn drop(&mut self) {
        self.revoke.revoke();
    }
}
#[derive(Default)]
struct State {
    reviews: BTreeMap<ScriptRunId, Review>,
    runs: BTreeMap<ScriptRunId, Run>,
}
#[derive(Clone, Default)]
pub(crate) struct ScriptControlService {
    state: Arc<Mutex<State>>,
    mutation: Arc<tokio::sync::Mutex<()>>,
}
impl std::fmt::Debug for ScriptControlService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptControlService(..)")
    }
}
#[derive(Clone)]
enum BoundSession {
    Ssh(Arc<crate::SshSession>),
    Local(crate::LocalSessionAttachment),
}
impl BoundSession {
    fn capture(
        id: SessionId,
        local: &LocalSessionRegistry,
        ssh: &SshSessionRegistry,
    ) -> Result<Self, Status> {
        if let Ok(s) = ssh.get(id) {
            return if s.running() {
                Ok(Self::Ssh(s))
            } else {
                Err(Status::Conflict)
            };
        }
        let s = local.attach(id).map_err(|_| Status::Unavailable)?;
        if s.is_closed() {
            return Err(Status::Conflict);
        }
        Ok(Self::Local(s))
    }
    fn current(
        &self,
        id: SessionId,
        local: &LocalSessionRegistry,
        ssh: &SshSessionRegistry,
    ) -> bool {
        match self {
            Self::Ssh(s) => s.running() && ssh.get(id).is_ok_and(|now| Arc::ptr_eq(s, &now)),
            Self::Local(s) => {
                !s.is_closed()
                    && local.attach(id).is_ok_and(|now| {
                        !now.is_closed() && s.snapshots().same_instance(&now.snapshots())
                    })
            }
        }
    }
    fn journal(
        &self,
        local: &LocalSessionRegistry,
    ) -> Result<cshell_output_store::JournalLineIndex, Status> {
        match self {
            Self::Ssh(s) => Ok(s.line_index()),
            Self::Local(s) => local
                .journal_index(s.session_id())
                .map_err(|_| Status::Unavailable),
        }
    }
    fn profile(&self, local: &LocalSessionRegistry) -> Result<Option<ProfileId>, Status> {
        match self {
            Self::Ssh(s) => Ok(s.profile_id()),
            Self::Local(s) => local
                .session_info(s.session_id())
                .map(|s| s.profile_id)
                .map_err(|_| Status::Unavailable),
        }
    }
    fn send(&self, text: String) -> bool {
        let action = InputAction::Text(text);
        match self {
            Self::Ssh(s) => s.send_input(&action).is_ok(),
            Self::Local(s) => s.send_input(&action).is_ok(),
        }
    }
}
impl ScriptControlService {
    pub(crate) async fn handle(
        &self,
        request: &ScriptControlRequest,
        local: &Arc<LocalSessionRegistry>,
        ssh: &Arc<SshSessionRegistry>,
        profiles: Option<&Arc<ProfileIpcService>>,
        tasks: Option<&TaskIpcService>,
        tools: &crate::terminal_tools_ipc::TerminalToolsService,
    ) -> ScriptControlResponse {
        let result = self
            .execute(request, local, ssh, profiles, tasks, tools)
            .await;
        match result {
            Ok(reply) => match serde_json::to_vec(&reply) {
                Ok(document_json) if document_json.len() <= 256 * 1024 => ScriptControlResponse {
                    status: Status::Ok as i32,
                    document_json,
                },
                _ => failure(Status::Unavailable),
            },
            Err(status) => failure(status),
        }
    }
    async fn execute(
        &self,
        request: &ScriptControlRequest,
        local: &Arc<LocalSessionRegistry>,
        ssh: &Arc<SshSessionRegistry>,
        profiles: Option<&Arc<ProfileIpcService>>,
        tasks: Option<&TaskIpcService>,
        tools: &crate::terminal_tools_ipc::TerminalToolsService,
    ) -> Result<ScriptControlReply, Status> {
        if request.document_json.is_empty() || request.document_json.len() > 256 * 1024 {
            return Err(Status::Invalid);
        }
        let command: ScriptControlCommand =
            serde_json::from_slice(&request.document_json).map_err(|_| Status::Invalid)?;
        if command.operation() != request.operation {
            return Err(Status::Invalid);
        }
        // Revocation/list/recovery remain available while the vault is locked.
        match &command {
            ScriptControlCommand::Get { run } => {
                return self.get(*run).map(ScriptControlReply::Run);
            }
            ScriptControlCommand::List => {
                return Ok(ScriptControlReply::Runs(
                    self.state
                        .lock()
                        .map_err(|_| Status::Unavailable)?
                        .runs
                        .values()
                        .map(|r| r.view.clone())
                        .collect(),
                ));
            }
            ScriptControlCommand::Revoke { run } => {
                let mut state = self.state.lock().map_err(|_| Status::Unavailable)?;
                let row = state.runs.get_mut(run).ok_or(Status::Conflict)?;
                row.revoke.revoke();
                if row.view.report.is_none() {
                    row.view.state = if row.view.state == ScriptLiveState::Armed {
                        ScriptLiveState::Cancelled
                    } else {
                        ScriptLiveState::Revoked
                    };
                }
                return Ok(ScriptControlReply::Run(row.view.clone()));
            }
            ScriptControlCommand::Recover { confirmed: true } => {
                return bounded_recovery().await.map(ScriptControlReply::Recovery);
            }
            ScriptControlCommand::Recover { .. } => return Err(Status::Invalid),
            _ => {}
        }
        let _gate = self.mutation.lock().await;
        let profiles = profiles.ok_or(Status::Unavailable)?;
        profiles
            .vault
            .ensure_owned()
            .map_err(|_| Status::Unavailable)?;
        match command {
            ScriptControlCommand::Preview { proposal } => {
                let plan = validate_live_proposal(&proposal).map_err(|_| Status::Invalid)?;
                let session = proposal
                    .scope
                    .session
                    .map(|id| BoundSession::capture(id, local, ssh))
                    .transpose()?;
                let mut auth = profiles.vault.cancellation();
                let generation = *auth.borrow_and_update();
                profiles
                    .vault
                    .commit_auth(generation, || ())
                    .map_err(|_| Status::Conflict)?;
                let _catalog_gate = profiles.catalog_mutation.lock().await;
                let catalog = profiles
                    .service
                    .load()
                    .await
                    .map_err(|_| Status::Unavailable)?
                    .snapshot();
                let profile_id = session
                    .as_ref()
                    .map(|s| s.profile(local))
                    .transpose()?
                    .flatten();
                let profile = match profile_id {
                    Some(id) => Some(
                        catalog
                            .profiles
                            .iter()
                            .find(|p| p.id == id)
                            .map(|p| ScriptProfileSummary {
                                id,
                                name: p.name.clone(),
                                kind: p.kind,
                            })
                            .ok_or(Status::Conflict)?,
                    ),
                    None => None,
                };
                if proposal.selected.contains(&ScriptCapability::ProfileRead) && profile.is_none() {
                    return Err(Status::Conflict);
                }
                let first_output_sequence = if let Some(session) = &session {
                    let index = session.journal(local)?;
                    if proposal.selected.contains(&ScriptCapability::TerminalRead)
                        && !index.recording_enabled()
                    {
                        return Err(Status::Conflict);
                    }
                    index.record_count()
                } else {
                    0
                };
                for task in &proposal.scope.tasks {
                    check_task(tasks, task).await?;
                }
                if let Some(event) = proposal.scope.trigger_event {
                    if !tools.script_event_current(
                        event,
                        proposal.scope.session.ok_or(Status::Invalid)?,
                        &proposal.document.script_id,
                    ) {
                        return Err(Status::Conflict);
                    }
                }
                let trigger_run = if proposal.scope.trigger_on_next_match {
                    Some(
                        tools
                            .script_arm_current(
                                proposal.scope.session.ok_or(Status::Invalid)?,
                                &proposal.document.script_id,
                            )
                            .ok_or(Status::Conflict)?,
                    )
                } else {
                    None
                };
                let receipt = ScriptLiveReview {
                    version: 1,
                    review: ScriptRunId::new(),
                    plan,
                    scope: proposal.scope.clone(),
                    profile,
                    catalog_revision: catalog.revision,
                    authentication_generation: generation,
                    first_output_sequence,
                    trigger_run,
                    expires_after_ms: 30_000,
                };
                let mut state = self.state.lock().map_err(|_| Status::Unavailable)?;
                state.reviews.retain(|_, r| r.expires > Instant::now());
                if state.reviews.len() >= 16 {
                    return Err(Status::Busy);
                }
                state.reviews.insert(
                    receipt.review,
                    Review {
                        proposal,
                        receipt: receipt.clone(),
                        expires: Instant::now() + Duration::from_secs(30),
                        session,
                    },
                );
                Ok(ScriptControlReply::Review(receipt))
            }
            ScriptControlCommand::Start {
                review,
                parameters,
                confirmed: true,
            } => {
                let frozen = {
                    let mut state = self.state.lock().map_err(|_| Status::Unavailable)?;
                    if state
                        .runs
                        .values()
                        .filter(|r| {
                            r.view.state.active() && r.view.report.is_none() && !r.view.failed
                        })
                        .count()
                        >= 2
                    {
                        return Err(Status::Busy);
                    }
                    if let Some(frozen) = state.reviews.get(&review)
                        && frozen.proposal.scope.trigger_on_next_match
                    {
                        let key = (
                            frozen.proposal.scope.session.ok_or(Status::Invalid)?,
                            frozen.proposal.document.script_id.clone(),
                        );
                        if state.runs.values().any(|r| {
                            r.view.state == ScriptLiveState::Armed
                                && r.trigger_key.as_ref() == Some(&key)
                        }) {
                            return Err(Status::Busy);
                        }
                    }
                    state.reviews.remove(&review).ok_or(Status::Conflict)?
                };
                if frozen.expires <= Instant::now() {
                    return Err(Status::Conflict);
                }
                let auth = frozen.receipt.authentication_generation;
                profiles
                    .vault
                    .commit_auth(auth, || ())
                    .map_err(|_| Status::Conflict)?;
                let catalog = profiles
                    .service
                    .load()
                    .await
                    .map_err(|_| Status::Unavailable)?
                    .snapshot();
                if catalog.revision != frozen.receipt.catalog_revision {
                    return Err(Status::Conflict);
                }
                parameters
                    .validate(&frozen.proposal.document)
                    .map_err(|_| Status::Invalid)?;
                for task in &frozen.proposal.scope.tasks {
                    check_task(tasks, task).await?;
                }
                let broker = Arc::new(broker::Broker {
                    local: local.clone(),
                    ssh: ssh.clone(),
                    profiles: profiles.clone(),
                    tasks: tasks.cloned(),
                    tools: tools.clone(),
                    session: frozen.session.clone(),
                    receipt: frozen.receipt.clone(),
                    script_id: frozen.proposal.document.script_id.clone(),
                    secrets: frozen
                        .proposal
                        .document
                        .parameters
                        .iter()
                        .filter(|(_, k)| **k == ScriptParameterKind::Secret)
                        .map(|(n, _)| n.clone())
                        .collect(),
                    consumed_tasks: Mutex::new(BTreeSet::new()),
                    cursor: tokio::sync::Mutex::new(broker::WaitCursor::new(
                        frozen.receipt.first_output_sequence,
                    )),
                    pending_effect: AtomicBool::new(false),
                    runtime: tokio::runtime::Handle::current(),
                });
                if !broker.authorized() {
                    return Err(Status::Conflict);
                }
                let grant = if frozen.proposal.scope.trigger_on_next_match {
                    ScriptGrant::issue_armed(&frozen.receipt.plan, true)
                } else {
                    ScriptGrant::issue(&frozen.receipt.plan, true)
                }
                .map_err(|_| Status::Invalid)?;
                let revoke = grant.revocation();
                let run = LiveScriptRun::review(
                    frozen.proposal.document.clone(),
                    frozen.receipt.plan.clone(),
                    parameters,
                    grant,
                    broker,
                )
                .map_err(|_| Status::Invalid)?;
                if let Some(event) = frozen.proposal.scope.trigger_event {
                    if !tools.claim_script_event(
                        event,
                        frozen.proposal.scope.session.ok_or(Status::Invalid)?,
                        &frozen.proposal.document.script_id,
                    ) {
                        return Err(Status::Conflict);
                    }
                }
                let id = run.run_id();
                let view = ScriptLiveView {
                    review,
                    run: id,
                    document_sha256: frozen.receipt.plan.document_sha256,
                    state: if frozen.proposal.scope.trigger_on_next_match {
                        ScriptLiveState::Armed
                    } else {
                        ScriptLiveState::Running
                    },
                    report: None,
                    failed: false,
                };
                {
                    let mut state = self.state.lock().map_err(|_| Status::Unavailable)?;
                    while state.runs.len() >= 64 {
                        let old = state
                            .runs
                            .iter()
                            .find(|(_, r)| {
                                !r.view.state.active() || r.view.report.is_some() || r.view.failed
                            })
                            .map(|(id, _)| *id)
                            .ok_or(Status::Busy)?;
                        state.runs.remove(&old);
                    }
                    state.runs.insert(
                        id,
                        Run {
                            revoke,
                            view: view.clone(),
                            trigger_key: if frozen.proposal.scope.trigger_on_next_match {
                                Some((
                                    frozen.proposal.scope.session.ok_or(Status::Invalid)?,
                                    frozen.proposal.document.script_id.clone(),
                                ))
                            } else {
                                None
                            },
                        },
                    );
                }
                let state = self.state.clone();
                let scope = frozen.proposal.scope.clone();
                let script_id = frozen.proposal.document.script_id.clone();
                let trigger_run = frozen.receipt.trigger_run;
                let tools = tools.clone();
                let mut trigger_events = tools.script_events();
                tokio::spawn(async move {
                    if scope.trigger_on_next_match {
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                        loop {
                            if !run.authorized() {
                                finish_unstarted(&state, id, ScriptLiveState::Cancelled);
                                return;
                            }
                            let signal = tokio::select! {
                                biased;
                                _ = tokio::time::sleep_until(deadline) => { finish_unstarted(&state, id, ScriptLiveState::Expired); return; },
                                signal = trigger_events.recv() => match signal { Ok(s) => s, Err(_) => { finish_unstarted(&state, id, ScriptLiveState::Expired); return; } },
                                _ = tokio::time::sleep(Duration::from_millis(50)) => continue,
                            };
                            if Some(signal.session) != scope.session
                                || Some(signal.run) != trigger_run
                                || signal.script_id != script_id
                            {
                                continue;
                            }
                            if !run.authorized()
                                || !tools.claim_script_event(
                                    signal.event,
                                    signal.session,
                                    &script_id,
                                )
                            {
                                finish_unstarted(&state, id, ScriptLiveState::Cancelled);
                                return;
                            }
                            tools.script_event_consumed(signal.event);
                            if let Ok(mut state) = state.lock()
                                && let Some(row) = state.runs.get_mut(&id)
                            {
                                row.view.state = ScriptLiveState::Running;
                            }
                            break;
                        }
                    }
                    let result = run_live(run).await;
                    if let Ok(mut state) = state.lock()
                        && let Some(row) = state.runs.get_mut(&id)
                    {
                        row.revoke.revoke();
                        match result {
                            Ok(report) => {
                                row.view.state = if report.status == ScriptRunStatus::Unknown
                                    || !report.exit_confirmed
                                    || !report.sandbox_cleanup_confirmed
                                {
                                    ScriptLiveState::Unknown
                                } else {
                                    ScriptLiveState::Finished
                                };
                                row.view.report = Some(report);
                            }
                            Err(_) => {
                                row.view.failed = true;
                                row.view.state = ScriptLiveState::Unknown;
                            }
                        }
                    }
                });
                Ok(ScriptControlReply::Run(view))
            }
            _ => Err(Status::Invalid),
        }
    }
    fn get(&self, id: ScriptRunId) -> Result<ScriptLiveView, Status> {
        self.state
            .lock()
            .map_err(|_| Status::Unavailable)?
            .runs
            .get(&id)
            .map(|r| r.view.clone())
            .ok_or(Status::Conflict)
    }
}
fn finish_unstarted(state: &Arc<Mutex<State>>, id: ScriptRunId, status: ScriptLiveState) {
    if let Ok(mut state) = state.lock()
        && let Some(row) = state.runs.get_mut(&id)
    {
        row.revoke.revoke();
        row.view.state = status;
    }
}
async fn check_task(
    tasks: Option<&TaskIpcService>,
    task: &ScriptTaskBinding,
) -> Result<(), Status> {
    let mut request = TaskRequest::default();
    request.operation = TaskOperation::Get as i32;
    request.run_id = task.run.as_uuid().as_bytes().to_vec();
    let reply = tasks.ok_or(Status::Unavailable)?.handle(&request).await;
    if reply.status != TaskStatus::Ok as i32
        || reply.revision != task.revision
        || reply.run_id != task.run.as_uuid().as_bytes().to_vec()
    {
        return Err(Status::Conflict);
    }
    Ok(())
}
async fn bounded_recovery() -> Result<ScriptRecoveryReport, Status> {
    static GATE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let permit = GATE
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| Status::Busy)?;
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("cshell-script-recovery".into())
        .spawn(move || {
            let _permit = permit;
            let _ = send.send(recover_script_resources());
        })
        .map_err(|_| Status::Unavailable)?;
    tokio::time::timeout(Duration::from_secs(5), receive)
        .await
        .map_err(|_| Status::Unavailable)?
        .map_err(|_| Status::Unavailable)?
        .map_err(|_| Status::Unavailable)
}
fn failure(status: Status) -> ScriptControlResponse {
    ScriptControlResponse {
        status: status as i32,
        document_json: Vec::new(),
    }
}
