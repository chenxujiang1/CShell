use crate::{LocalSessionRegistry, ProfileIpcService, SshSessionRegistry};
use cshell_application::*;
use cshell_domain::{InputAction, SessionId, TaskRunId, TriggerAction};
use cshell_ipc::{
    TerminalToolsOperation as Op, TerminalToolsRequest as Request,
    TerminalToolsResponse as Response, TerminalToolsStatus as Status,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Debug)]
struct Armed {
    view: TriggerRuntimeView,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    script_ids: std::collections::BTreeSet<String>,
}
impl Drop for Armed {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
    }
}
#[derive(Debug, Default)]
struct Events {
    last: u64,
    rows: VecDeque<TriggerEvent>,
    script_claims: std::collections::BTreeSet<u64>,
}
#[derive(Clone, Debug)]
pub(crate) struct TerminalToolsService {
    mutation: Arc<tokio::sync::Mutex<()>>,
    armed: Arc<Mutex<BTreeMap<SessionId, Armed>>>,
    events: Arc<Mutex<Events>>,
    readers: Arc<tokio::sync::Semaphore>,
    script_events: tokio::sync::broadcast::Sender<ScriptTriggerSignal>,
}
#[derive(Clone, Debug)]
pub(crate) struct ScriptTriggerSignal {
    pub event: u64,
    pub session: SessionId,
    pub run: TaskRunId,
    pub script_id: String,
}
impl Default for TerminalToolsService {
    fn default() -> Self {
        Self {
            mutation: Arc::new(tokio::sync::Mutex::new(())),
            armed: Arc::new(Mutex::new(BTreeMap::new())),
            events: Arc::new(Mutex::new(Events::default())),
            readers: Arc::new(tokio::sync::Semaphore::new(4)),
            script_events: tokio::sync::broadcast::channel(128).0,
        }
    }
}
impl TerminalToolsService {
    pub(crate) fn script_event_consumed(&self, event: u64) {
        if let Some(row) = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rows
            .iter_mut()
            .find(|e| e.id == event)
        {
            row.disposition = "script-grant-consumed".into();
        }
    }
    pub(crate) fn script_events(&self) -> tokio::sync::broadcast::Receiver<ScriptTriggerSignal> {
        self.script_events.subscribe()
    }
    pub(crate) fn script_arm_current(&self, session: SessionId, script: &str) -> Option<TaskRunId> {
        self.armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session)
            .filter(|a| !a.task.is_finished() && a.script_ids.contains(script))
            .map(|a| a.view.run)
    }
    pub(crate) fn script_event_current(
        &self,
        event: u64,
        session: SessionId,
        script: &str,
    ) -> bool {
        let armed = self
            .armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = armed.get(&session).filter(|a| !a.task.is_finished()) else {
            return false;
        };
        let log = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.rows.iter().any(|row| {
            row.id == event
                && row.session == session
                && row.run == active.view.run
                && matches!(&row.action, TriggerAction::Script { script_id } if script_id == script)
        })
    }
    pub(crate) fn claim_script_event(&self, event: u64, session: SessionId, script: &str) -> bool {
        if !self.script_event_current(event, session, script) {
            return false;
        }
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .script_claims
            .insert(event)
    }
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn handle(
        &self,
        request: &Request,
        local: &Arc<LocalSessionRegistry>,
        ssh: &Arc<SshSessionRegistry>,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Response {
        let op = match Op::try_from(request.operation) {
            Ok(op) => op,
            Err(_) => return response(Status::Invalid, "invalid terminal tools operation"),
        };
        let session = if request.session_id.is_empty() {
            None
        } else {
            match request.session_id.as_slice().try_into() {
                Ok(bytes) => Some(SessionId::from_bytes(bytes)),
                Err(_) => return response(Status::Invalid, "invalid session ID"),
            }
        };
        let expected = Request {
            operation: request.operation,
            expected_revision: if matches!(op, Op::Save | Op::Arm) {
                request.expected_revision
            } else {
                0
            },
            document_json: if op == Op::Save {
                request.document_json.clone()
            } else {
                Vec::new()
            },
            session_id: if matches!(op, Op::Arm | Op::Disarm | Op::RawJournal | Op::ShellHistory) {
                request.session_id.clone()
            } else {
                Vec::new()
            },
            cursor: if matches!(op, Op::Poll | Op::RawJournal | Op::ShellHistory) {
                request.cursor
            } else {
                0
            },
            offset: if op == Op::RawJournal {
                request.offset
            } else {
                0
            },
            allow_text: op == Op::Arm && request.allow_text,
            confirmed: matches!(op, Op::Save | Op::Arm) && request.confirmed,
        };
        if request != &expected
            || session.is_some_and(|s| s.as_uuid().is_nil())
            || request.document_json.len() > MAX_TERMINAL_TOOLS_BYTES
        {
            return response(Status::Invalid, "noncanonical terminal tools request");
        }
        if matches!(op, Op::Save | Op::Arm) && !request.confirmed {
            return response(Status::Invalid, "explicit confirmation required");
        }
        let result = self
            .execute(op, request, session, local, ssh, profiles)
            .await;
        match result {
            Ok(reply) => match serde_json::to_vec(&reply) {
                Ok(document_json) if document_json.len() <= 512 * 1024 => Response {
                    status: Status::Ok as i32,
                    document_json,
                    detail: String::new(),
                },
                _ => response(Status::Unavailable, "terminal tools reply unavailable"),
            },
            Err(detail) => response(
                if detail.contains("revision") {
                    Status::Conflict
                } else {
                    Status::Unavailable
                },
                detail,
            ),
        }
    }
    async fn execute(
        &self,
        op: Op,
        request: &Request,
        session: Option<SessionId>,
        local: &Arc<LocalSessionRegistry>,
        ssh: &Arc<SshSessionRegistry>,
        profiles: Option<&Arc<ProfileIpcService>>,
    ) -> Result<TerminalToolsReply, &'static str> {
        if op == Op::ShellHistory {
            let id=session.ok_or("missing session scope")?;
            let snapshots=if let Ok(s)=ssh.get(id){s.history_snapshots()}else{local.attach(id).map_err(|_|"terminal session unavailable")?.snapshots()};
            let data=snapshots.shell_history(id,request.cursor).ok_or("terminal history unavailable")?;
            if !data.validate(id,request.cursor){return Err("command is no longer retained; refresh history");}
            return Ok(TerminalToolsReply::Shell(data));
        }
        if op == Op::RawJournal {
            let id = session.ok_or("missing session scope")?;
            if let Some(profiles) = profiles {
                profiles
                    .vault
                    .ensure_owned()
                    .map_err(|_| "credential storage is owned by another daemon")?;
            }
            let index = journal(local, ssh, id)?;
            if !index.recording_enabled() {
                return Err("output recording is disabled for this session");
            }
            let sequence = request.cursor;
            let offset = request.offset;
            let _permit = self
                .readers
                .clone()
                .try_acquire_owned()
                .map_err(|_| "raw journal reader budget")?;
            let raw = tokio::task::spawn_blocking(move || {
                let _permit = _permit;
                index.read_raw_slice(sequence, offset)
            })
            .await
            .map_err(|_| "raw journal worker failed")?
            .map_err(|_| "raw journal record/range unavailable")?;
            return Ok(TerminalToolsReply::Raw(RawJournalSlice {
                session: id,
                sequence,
                offset,
                record_bytes: raw.record_bytes,
                total_records: raw.total_records,
                data: raw.data,
                elapsed_us: raw.timing.map(|t| t.elapsed_us),
                rows: raw.timing.map(|t| t.rows),
                cols: raw.timing.map(|t| t.cols),
            }));
        }
        let _gate = self.mutation.lock().await;
        let mut catalog = crate::terminal_tools_store::read().await?;
        if matches!(op, Op::Save | Op::Arm) && catalog.revision != request.expected_revision {
            return Err("terminal tools revision conflict");
        }
        match op {
            Op::Save => {
                let access = profiles.ok_or("profile authority unavailable")?;
                access
                    .vault
                    .ensure_owned()
                    .map_err(|_| "credential storage is owned by another daemon")?;
                let bytes = request.document_json.clone();
                let mut proposed =
                    tokio::task::spawn_blocking(move || decode_terminal_tools(&bytes))
                        .await
                        .map_err(|_| "terminal tools compiler failed")??;
                if proposed.revision != catalog.revision {
                    return Err("terminal tools document revision mismatch");
                }
                self.stop_all().await;
                proposed.revision = catalog
                    .revision
                    .checked_add(1)
                    .ok_or("terminal tools revision exhausted")?;
                crate::terminal_tools_store::write(&proposed, catalog.revision).await?;
                catalog = proposed;
            }
            Op::Arm => {
                let access = profiles.ok_or("profile authority unavailable")?;
                access
                    .vault
                    .ensure_owned()
                    .map_err(|_| "credential storage is owned by another daemon")?;
                let id = session.ok_or("missing session scope")?;
                let index = journal(local, ssh, id)?;
                if !index.recording_enabled() {
                    return Err("triggers require a session with output recording enabled");
                }
                let profile = if let Ok(s) = ssh.get(id) {
                    s.profile_id()
                } else {
                    local
                        .session_info(id)
                        .map_err(|_| "session unavailable")?
                        .profile_id
                };
                self.stop(id).await;
                {
                    let mut armed = self
                        .armed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    armed.retain(|_, a| !a.task.is_finished());
                    if armed.len() >= 16 {
                        return Err("trigger session budget");
                    }
                }
                let rules = catalog.triggers.clone();
                let script_ids = rules
                    .iter()
                    .filter(|r| {
                        r.enabled
                            && (r.profiles.is_empty()
                                || profile.is_some_and(|p| r.profiles.contains(&p)))
                    })
                    .filter_map(|r| match &r.action {
                        TriggerAction::Script { script_id } => Some(script_id.clone()),
                        _ => None,
                    })
                    .collect();
                let engine = tokio::task::spawn_blocking(move || {
                    cshell_automation::TriggerEngine::new(&rules, profile)
                })
                .await
                .map_err(|_| "trigger compiler failed")??;
                let first_sequence = index.record_count();
                let run = TaskRunId::new();
                let view = TriggerRuntimeView {
                    session: id,
                    run,
                    catalog_revision: catalog.revision,
                    allow_text: request.allow_text,
                };
                let mut locked = access.vault.cancellation();
                let generation = *locked.borrow_and_update();
                access
                    .vault
                    .commit_auth(generation, || ())
                    .map_err(|_| "authentication generation unavailable")?;
                let vault = access.vault.clone();
                let events = self.events.clone();
                let script_events = self.script_events.clone();
                let local = local.clone();
                let ssh = ssh.clone();
                let allow_text = request.allow_text;
                let readers = self.readers.clone();
                let (stop, mut stopped) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(async move {
                    let mut engine = engine;
                    let (mut sequence, mut offset) = (first_sequence, 0);
                    let mut minute = Instant::now();
                    let mut budget = 0u16;
                    loop {
                        if *stopped.borrow() || *locked.borrow() != generation {
                            break;
                        }
                        let reader = index.clone();
                        let permit = tokio::select! {biased;_=stopped.changed()=>break,_=locked.changed()=>break,r=readers.clone().acquire_owned()=>match r{Ok(p)=>p,Err(_)=>break}};
                        let raw = tokio::select! {biased;_=stopped.changed()=>break,_=locked.changed()=>break,
                        r=tokio::task::spawn_blocking(move||{let _permit=permit;reader.read_raw_slice(sequence,offset)})=>match r{Ok(Ok(raw))=>raw,_=>break}};
                        if raw.sequence >= raw.total_records {
                            let ended = ssh.get(id).map(|s| !s.running()).unwrap_or_else(|_| {
                                local.session_info(id).map(|s| !s.running).unwrap_or(true)
                            });
                            if ended {
                                break;
                            }
                            tokio::select! {biased;_=stopped.changed()=>break,_=locked.changed()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{}}
                            continue;
                        }
                        let data = raw.data;
                        let end = offset + data.len() as u64;
                        let last = end == raw.record_bytes;
                        let permit = tokio::select! {biased;_=stopped.changed()=>break,_=locked.changed()=>break,r=readers.clone().acquire_owned()=>match r{Ok(p)=>p,Err(_)=>break}};
                        let compiled = tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            let matches = engine.feed(sequence, offset, &data, Instant::now());
                            (engine, matches)
                        });
                        let (next_engine, matches) = tokio::select! {biased;_=stopped.changed()=>break,_=locked.changed()=>break,r=compiled=>match r{Ok(value)=>value,_=>break}};
                        engine = next_engine;
                        if minute.elapsed() >= Duration::from_secs(60) {
                            minute = Instant::now();
                            budget = 0;
                        }
                        for matched in matches {
                            if budget >= 120 {
                                break;
                            }
                            budget += 1;
                            if *stopped.borrow() || *locked.borrow() != generation {
                                break;
                            }
                            let disposition = match &matched.action {
                                TriggerAction::AppendText { text } if allow_text => {
                                    match vault.commit_auth(generation, || {
                                        ssh.get(id)
                                            .map(|s| {
                                                s.send_input(&InputAction::Text(text.clone()))
                                                    .is_ok()
                                            })
                                            .unwrap_or_else(|_| {
                                                local
                                                    .attach(id)
                                                    .map(|s| {
                                                        s.send_input(&InputAction::Text(
                                                            text.clone(),
                                                        ))
                                                        .is_ok()
                                                    })
                                                    .unwrap_or(false)
                                            })
                                    }) {
                                        Ok(true) => "accepted-once",
                                        Ok(false) => "dispatch-uncertain",
                                        Err(_) => "blocked",
                                    }
                                }
                                TriggerAction::AppendText { .. } => "text-permission-required",
                                TriggerAction::Script { .. } => "script-review-required",
                                _ => "recorded",
                            };
                            let mut log = events
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let Some(next) = log.last.checked_add(1) else {
                                break;
                            };
                            log.last = next;
                            let script = match &matched.action {
                                TriggerAction::Script { script_id } => Some(script_id.clone()),
                                _ => None,
                            };
                            log.rows.push_back(TriggerEvent {
                                id: next,
                                session: id,
                                run,
                                rule_id: matched.rule_id,
                                output_sequence: matched.sequence,
                                action: matched.action,
                                disposition: disposition.into(),
                            });
                            while log.rows.len() > 512 {
                                log.rows.pop_front();
                            }
                            if let Some(first) = log.rows.front().map(|r| r.id) {
                                log.script_claims.retain(|id| *id >= first);
                            }
                            drop(log);
                            if let Some(script_id) = script {
                                let _ = script_events.send(ScriptTriggerSignal {
                                    event: next,
                                    session: id,
                                    run,
                                    script_id,
                                });
                            }
                        }
                        if last {
                            sequence = sequence.saturating_add(1);
                            offset = 0;
                        } else {
                            offset = end;
                        }
                        tokio::task::yield_now().await;
                    }
                });
                let armed = Armed {
                    view,
                    stop,
                    task,
                    script_ids,
                };
                access
                    .vault
                    .commit_auth(generation, || {
                        self.armed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(id, armed)
                    })
                    .map_err(|_| "trigger arm generation changed")?;
            }
            Op::Disarm => self.stop(session.ok_or("missing session scope")?).await,
            Op::Get | Op::Poll => {}
            Op::RawJournal | Op::ShellHistory => {}
        }
        let armed = {
            let mut active = self
                .armed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.retain(|_, a| !a.task.is_finished());
            active.values().map(|a| a.view.clone()).collect()
        };
        let log = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let after = if op == Op::Poll { request.cursor } else { 0 };
        if after > log.last {
            return Err("trigger event cursor unavailable");
        }
        let rows = log
            .rows
            .iter()
            .filter(|e| e.id > after)
            .take(128)
            .cloned()
            .collect();
        Ok(TerminalToolsReply::View(TerminalToolsView {
            catalog,
            armed,
            events: rows,
            next_event: log.last,
            lost_before: log
                .rows
                .front()
                .map_or(log.last, |e| e.id.saturating_sub(1)),
        }))
    }
    async fn stop(&self, id: SessionId) {
        let row = self
            .armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if let Some(mut row) = row {
            let _ = row.stop.send(true);
            if tokio::time::timeout(Duration::from_secs(2), &mut row.task)
                .await
                .is_err()
            {
                row.task.abort();
                let _ = (&mut row.task).await;
            }
        }
    }
    async fn stop_all(&self) {
        let ids = self
            .armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            self.stop(id).await;
        }
    }
}
fn journal(
    local: &LocalSessionRegistry,
    ssh: &SshSessionRegistry,
    id: SessionId,
) -> Result<cshell_output_store::JournalLineIndex, &'static str> {
    ssh.get(id)
        .map(|s| s.line_index())
        .or_else(|_| {
            local
                .journal_index(id)
                .map_err(|_| crate::SshSessionError::Unknown(id))
        })
        .map_err(|_| "session journal unavailable")
}
fn response(status: Status, detail: &str) -> Response {
    Response {
        status: status as i32,
        detail: detail.into(),
        ..Default::default()
    }
}
