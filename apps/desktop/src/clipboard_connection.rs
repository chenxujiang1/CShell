use crate::daemon_connection::DesktopConnectionConfig;
use cshell_domain::SessionId;
use cshell_ipc::{
    ClipboardOperation, ClipboardRequest, ClipboardResponse, ClipboardStatus, Envelope, Handshake,
    client_handshake, envelope, features, read_envelope, transport, write_envelope,
};
use std::sync::{Arc, Mutex};
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct PendingClipboard {
    pub session: SessionId,
    pub token: Zeroizing<Vec<u8>>,
    pub byte_count: u32,
    pub automatic: bool,
}
impl std::fmt::Debug for PendingClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingClipboard")
            .field("session", &self.session)
            .field("byte_count", &self.byte_count)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    generation: u64,
    pending: Option<PendingClipboard>,
    approved: Option<(SessionId, Zeroizing<String>)>,
    attempted_auto: std::collections::VecDeque<AutomaticAttempt>,
    error: Option<String>,
}
struct AutomaticAttempt {
    session: SessionId,
    token: Zeroizing<Vec<u8>>,
    at: std::time::Instant,
}
impl State {
    fn prune_attempts(&mut self) {
        self.attempted_auto
            .retain(|entry| entry.at.elapsed() < std::time::Duration::from_secs(31));
    }
    fn attempted(&self, session: SessionId, token: &[u8]) -> bool {
        self.attempted_auto
            .iter()
            .any(|entry| entry.session == session && entry.token.as_slice() == token)
    }
}

pub struct DesktopClipboardConnection {
    shared: Arc<Mutex<State>>,
    target: tokio::sync::watch::Sender<Option<SessionId>>,
    commands: tokio::sync::mpsc::Sender<ClipboardRequest>,
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DesktopClipboardConnection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(State::default()));
        let worker_shared = shared.clone();
        let (target, mut targets) = tokio::sync::watch::channel(None);
        let (commands, mut receiver) = tokio::sync::mpsc::channel::<ClipboardRequest>(4);
        let (shutdown, mut shutdown_receiver) = tokio::sync::watch::channel(false);
        let worker = std::thread::Builder::new().name("cshell-clipboard-ipc".into()).spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return; };
            runtime.block_on(async move {
                let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
                poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let request = tokio::select! {
                        _ = shutdown_receiver.changed() => break,
                        changed = targets.changed() => {
                            if changed.is_err() { break; }
                            let mut state = worker_shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.pending = None;
                            state.error = None;
                            state.generation = state.generation.wrapping_add(1);
                            continue;
                        }
                        command = receiver.recv() => {
                            let Some(command) = command else { break; };
                            command
                        }
                        _ = poll.tick() => {
                            let Some(session) = *targets.borrow() else { continue; };
                            request(session, ClipboardOperation::Poll, &[])
                        }
                    };
                    let Ok(session) = cshell_ipc::decode_id(&request.session_id).map(SessionId::from_bytes) else { continue; };
                    let approve = matches!(ClipboardOperation::try_from(request.operation), Ok(ClipboardOperation::Approve | ClipboardOperation::ApproveAutomatic));
                    let polling = request.operation == ClipboardOperation::Poll as i32;
                    let result = tokio::select! {
                        result = send(&config, request) => result,
                        _ = shutdown_receiver.changed() => break,
                    };
                    let mut state = worker_shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    // A response from a previously selected tab never becomes a new prompt.
                    if polling && *targets.borrow() != Some(session) { continue; }
                    match result {
                        Ok(mut response) if response.status == ClipboardStatus::Ok as i32 => {
                            state.error = None;
                            state.pending = if polling && response.token.len() == 16 && response.byte_count as usize <= cshell_ipc::MAX_CLIPBOARD_TEXT_BYTES {
                                Some(PendingClipboard { session, token: Zeroizing::new(std::mem::take(&mut response.token)), byte_count: response.byte_count, automatic: response.automatic })
                            } else { None };
                            if approve && response.text.len() <= cshell_ipc::MAX_CLIPBOARD_TEXT_BYTES {
                                state.approved = Some((session, Zeroizing::new(std::mem::take(&mut response.text))));
                            }
                        }
                        Ok(_) => {
                            state.pending = None;
                            if !polling { state.error = Some("剪贴板请求已过期或会话已关闭；未写入剪贴板。".into()); }
                        }
                        Err(_) => {
                            state.pending = None;
                            if !polling { state.error = Some("剪贴板确认结果未知；未写入，也不会自动重试。".into()); }
                        }
                    }
                    state.generation = state.generation.wrapping_add(1);
                }
            });
        })?;
        Ok(Self {
            shared,
            target,
            commands,
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn select(&self, session: Option<SessionId>) {
        self.target.send_if_modified(|value| {
            if *value == session {
                return false;
            }
            *value = session;
            true
        });
    }
    pub fn generation(&self) -> u64 {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }
    pub fn take_approved(
        &self,
        selected: Option<SessionId>,
        focused: bool,
    ) -> Option<Zeroizing<String>> {
        let result = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .approved
            .take();
        eligible_text(result, selected, focused)
    }
    pub fn take_error(&self) -> Option<String> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error
            .take()
    }
    pub fn draw(
        &self,
        context: &egui::Context,
        selected: Option<SessionId>,
        focused: bool,
        title: &str,
    ) -> Option<ClipboardRequest> {
        let (pending, error, attempted) = {
            let mut state = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.prune_attempts();
            let attempted = state
                .pending
                .as_ref()
                .is_some_and(|pending| state.attempted(pending.session, &pending.token))
                || state.attempted_auto.len() >= 64;
            (state.pending.clone(), state.error.clone(), attempted)
        };
        let pending = pending.filter(|pending| Some(pending.session) == selected)?;
        if pending.automatic && attempted {
            return None;
        }
        if pending.automatic && focused {
            return Some(request(
                pending.session,
                ClipboardOperation::ApproveAutomatic,
                &pending.token,
            ));
        }
        let mut command = None;
        cshell_ui::form::dialog(&context, "远端请求写入剪贴板")
            .collapsible(false)
            .resizable(false)
            .show(context, |ui| {
                ui.label(format!("SSH 会话：{title}"));
                ui.label(format!(
                    "远端希望替换本地剪贴板内容（{} 字节）。",
                    pending.byte_count
                ));
                ui.label("只允许本次写入；后续请求仍需确认。请求 30 秒后过期。");
                if let Some(error) = error {
                    ui.colored_label(cshell_ui::theme::DANGER, error);
                }
                ui.add_enabled_ui(focused, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (label, operation) in [
                            ("允许本次", ClipboardOperation::Approve),
                            ("拒绝本次", ClipboardOperation::Reject),
                            ("禁用本会话", ClipboardOperation::Block),
                        ] {
                            if ui.button(label).clicked() {
                                command = Some(request(pending.session, operation, &pending.token));
                            }
                        }
                    });
                });
            });
        command
    }
    pub fn decide(&self, request: ClipboardRequest) -> bool {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let automatic = request.operation == ClipboardOperation::ApproveAutomatic as i32;
        state.prune_attempts();
        let id = cshell_ipc::decode_id(&request.session_id)
            .ok()
            .map(SessionId::from_bytes);
        if automatic
            && (state.attempted_auto.len() >= 64
                || id.is_some_and(|session| state.attempted(session, &request.token)))
        {
            return false;
        }
        let attempted = if automatic {
            id.map(|session| AutomaticAttempt {
                session,
                token: Zeroizing::new(request.token.clone()),
                at: std::time::Instant::now(),
            })
        } else {
            None
        };
        if self.commands.try_send(request).is_err() {
            return false;
        }
        if let Some(attempt) = attempted {
            state.attempted_auto.push_back(attempt);
        }
        state.pending = None;
        state.generation = state.generation.wrapping_add(1);
        true
    }
}

fn eligible_text(
    result: Option<(SessionId, Zeroizing<String>)>,
    selected: Option<SessionId>,
    focused: bool,
) -> Option<Zeroizing<String>> {
    result.and_then(|(session, text)| (focused && selected == Some(session)).then_some(text))
}
fn request(session: SessionId, operation: ClipboardOperation, token: &[u8]) -> ClipboardRequest {
    ClipboardRequest {
        session_id: session.as_uuid().as_bytes().to_vec(),
        operation: operation as i32,
        token: token.to_vec(),
    }
}

impl Drop for DesktopClipboardConnection {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn send(
    config: &DesktopConnectionConfig,
    request: ClipboardRequest,
) -> Result<ClipboardResponse, String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let resolved = config.resolve().map_err(|error| error.to_string())?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|error| error.to_string())?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        handshake.feature_bits = features::CLIPBOARD_CONTROL | features::AUTOMATIC_CLIPBOARD;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|error| error.to_string())?;
        if negotiated.feature_bits & features::CLIPBOARD_CONTROL == 0 {
            return Err("clipboard control unavailable".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::ClipboardRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|error| error.to_string())?;
        if response.request_id != 2 {
            return Err("mismatched clipboard response".into());
        }
        match response.payload {
            Some(envelope::Payload::ClipboardResponse(response)) => Ok(response),
            _ => Err("unexpected clipboard response".into()),
        }
    })
    .await
    .map_err(|_| "clipboard response timed out".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_attempts_survive_session_switches_and_expire_after_token_lifetime() {
        let (target, _targets) = tokio::sync::watch::channel(None);
        let (commands, mut receiver) = tokio::sync::mpsc::channel(4);
        let (shutdown, _shutdown) = tokio::sync::watch::channel(false);
        let connection = DesktopClipboardConnection {
            shared: Arc::new(Mutex::new(State::default())),
            target,
            commands,
            shutdown,
            worker: None,
        };
        let first = SessionId::new();
        let second = SessionId::new();
        assert!(connection.decide(request(
            first,
            ClipboardOperation::ApproveAutomatic,
            &[1; 16]
        )));
        assert!(connection.decide(request(
            second,
            ClipboardOperation::ApproveAutomatic,
            &[2; 16]
        )));
        assert!(!connection.decide(request(
            first,
            ClipboardOperation::ApproveAutomatic,
            &[1; 16]
        )));
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        {
            let mut state = connection
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.attempted_auto[0].at =
                std::time::Instant::now() - std::time::Duration::from_secs(32);
        }
        assert!(connection.decide(request(
            first,
            ClipboardOperation::ApproveAutomatic,
            &[1; 16]
        )));
        let mut state = connection
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.attempted_auto.clear();
        for byte in 0..64 {
            state.attempted_auto.push_back(AutomaticAttempt {
                session: first,
                token: Zeroizing::new(vec![byte; 16]),
                at: std::time::Instant::now(),
            });
        }
        drop(state);
        assert!(!connection.decide(request(
            second,
            ClipboardOperation::ApproveAutomatic,
            &[255; 16]
        )));
        assert_eq!(
            connection
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .attempted_auto
                .len(),
            64
        );
    }
    #[test]
    fn late_approval_never_writes_to_another_or_unfocused_session() {
        let original = SessionId::new();
        let other = SessionId::new();
        let result = || Some((original, Zeroizing::new("clipboard".to_owned())));
        assert!(eligible_text(result(), Some(other), true).is_none());
        assert!(eligible_text(result(), Some(original), false).is_none());
        assert!(eligible_text(result(), None, true).is_none());
        assert_eq!(
            eligible_text(result(), Some(original), true)
                .as_deref()
                .map(|text| text.as_str()),
            Some("clipboard")
        );
    }
}
