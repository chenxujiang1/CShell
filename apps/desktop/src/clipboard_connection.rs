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
    error: Option<String>,
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
                    let approve = request.operation == ClipboardOperation::Approve as i32;
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
                                Some(PendingClipboard { session, token: Zeroizing::new(std::mem::take(&mut response.token)), byte_count: response.byte_count })
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
        let (pending, error) = {
            let state = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.pending.clone(), state.error.clone())
        };
        let pending = pending.filter(|pending| Some(pending.session) == selected)?;
        let mut command = None;
        egui::Window::new("远端请求写入剪贴板")
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
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                ui.add_enabled_ui(focused, |ui| {
                    ui.horizontal(|ui| {
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
        if self.commands.try_send(request).is_err() {
            return false;
        }
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        handshake.feature_bits = features::CLIPBOARD_CONTROL;
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
