use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::{
    ClipboardPolicyDocument, ClipboardPolicySnapshot, decode_clipboard_policy,
    encode_clipboard_policy,
};
use cshell_domain::{ClipboardHost, SessionId};
use cshell_ipc::{
    ClipboardPolicyOperation, ClipboardPolicyRequest, ClipboardPolicyResponse,
    ClipboardPolicyStatus, Envelope, Handshake, client_handshake, envelope, features,
    read_envelope, transport, write_envelope,
};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
struct View {
    generation: u64,
    snapshot: Option<ClipboardPolicySnapshot>,
    selected_host: Option<ClipboardHost>,
    error: Option<String>,
    busy: bool,
}

pub struct DesktopClipboardPolicy {
    pub open: bool,
    shared: Arc<Mutex<View>>,
    target: tokio::sync::watch::Sender<Option<SessionId>>,
    commands: tokio::sync::mpsc::Sender<ClipboardPolicyRequest>,
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
    host_input: String,
    port_input: String,
}
impl DesktopClipboardPolicy {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(View::default()));
        let worker_shared = shared.clone();
        let (target, mut targets) = tokio::sync::watch::channel(None);
        let (commands, mut receiver) = tokio::sync::mpsc::channel(4);
        let (shutdown, mut shutdown_receiver) = tokio::sync::watch::channel(false);
        let worker = std::thread::Builder::new()
            .name("cshell-clipboard-policy".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    let mut initial = true;
                    loop {
                        let request = if initial {
                            initial = false;
                            load_request(*targets.borrow())
                        } else {
                            tokio::select! {
                                _ = shutdown_receiver.changed() => break,
                                changed = targets.changed() => {
                                    if changed.is_err() { break; }
                                    load_request(*targets.borrow())
                                }
                                command = receiver.recv() => {
                                    let Some(command) = command else { break; };
                                    command
                                }
                            }
                        };
                        let selected = request.selected_session_id.clone();
                        let saving = request.operation == ClipboardPolicyOperation::Save as i32;
                        {
                            let mut view = worker_shared
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = true;
                            view.generation = view.generation.wrapping_add(1);
                        }
                        let result = tokio::select! {
                            result = send(&config, request) => result,
                            _ = shutdown_receiver.changed() => break,
                        };
                        let mut view = worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        view.busy = false;
                        view.selected_host = None;
                        match result {
                            Ok(response) if response.status == ClipboardPolicyStatus::Ok as i32 => {
                                match decode_clipboard_policy(&response.document_json) {
                                    Ok(document) => {
                                        view.snapshot = Some(ClipboardPolicySnapshot {
                                            revision: response.revision,
                                            document,
                                        });
                                        let current = targets
                                            .borrow()
                                            .map(|id| id.as_uuid().as_bytes().to_vec())
                                            .unwrap_or_default();
                                        if current == selected {
                                            view.selected_host =
                                                u16::try_from(response.selected_port)
                                                    .ok()
                                                    .and_then(|port| {
                                                        ClipboardHost::new(
                                                            &response.selected_host,
                                                            port,
                                                        )
                                                    });
                                        }
                                        view.error = None;
                                    }
                                    Err(_) => {
                                        view.snapshot = None;
                                        view.error = Some("主机策略响应无效；请刷新。".into());
                                    }
                                }
                            }
                            Ok(response) => {
                                view.snapshot = None;
                                view.error = Some(
                                    match ClipboardPolicyStatus::try_from(response.status) {
                                        Ok(ClipboardPolicyStatus::Conflict) => {
                                            "策略已被其他窗口修改；刷新后再编辑。"
                                        }
                                        Ok(ClipboardPolicyStatus::Unsupported) => {
                                            "daemon 不支持主机剪贴板策略；需要更新 daemon。"
                                        }
                                        Ok(ClipboardPolicyStatus::Invalid) => {
                                            "主机策略无效或超出容量限制。"
                                        }
                                        _ => "主机策略存储不可用；请刷新。",
                                    }
                                    .into(),
                                );
                            }
                            Err(_) => {
                                view.snapshot = None;
                                view.error = Some(
                                    if saving {
                                        "保存结果未知；请刷新核对，不能自动重试。"
                                    } else {
                                        "无法读取主机策略；请刷新。"
                                    }
                                    .into(),
                                );
                            }
                        }
                        view.generation = view.generation.wrapping_add(1);
                    }
                });
            })?;
        Ok(Self {
            open: false,
            shared,
            target,
            commands,
            shutdown,
            worker: Some(worker),
            host_input: String::new(),
            port_input: "22".into(),
        })
    }
    pub fn select(&self, session: Option<SessionId>) {
        let changed = self.target.send_if_modified(|value| {
            if *value == session {
                return false;
            }
            *value = session;
            true
        });
        if changed {
            let mut view = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            view.selected_host = None;
            view.generation = view.generation.wrapping_add(1);
        }
    }
    pub fn generation(&self) -> u64 {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }
    pub fn refresh(&self) {
        self.submit(load_request(*self.target.borrow()));
    }
    fn submit(&self, request: ClipboardPolicyRequest) {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return;
        }
        if self.commands.try_send(request).is_ok() {
            view.busy = true;
        } else {
            view.error = Some("主机策略请求队列不可用；请刷新。".into());
        }
        view.generation = view.generation.wrapping_add(1);
    }
    pub fn draw(&mut self, context: &egui::Context) {
        if !self.open {
            return;
        }
        let view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut open = self.open;
        let mut replacement: Option<ClipboardPolicyDocument> = None;
        let mut refresh = false;
        let mut input_error = None;
        egui::Window::new("剪贴板安全策略")
            .open(&mut open)
            .default_width(560.0)
            .show(context, |ui| {
                ui.label("SSH 远端写剪贴板默认每次询问；远端读取始终禁止。");
                ui.label("禁用列表按主机与端口保存，影响该目标的全部 SSH 会话。");
                if let Some(error) = &view.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if view.busy {
                    ui.label("正在读取或保存策略…");
                }
                refresh = ui
                    .add_enabled(!view.busy, egui::Button::new("刷新策略"))
                    .clicked();
                let Some(snapshot) = &view.snapshot else {
                    return;
                };
                ui.add_enabled_ui(!view.busy, |ui| {
                    if let Some(host) = &view.selected_host {
                        ui.horizontal(|ui| {
                            ui.label(format!("当前 SSH 目标：{}", endpoint_label(host)));
                            if ui
                                .add_enabled(
                                    !snapshot.document.blocked_hosts.contains(host),
                                    egui::Button::new("禁用此主机"),
                                )
                                .clicked()
                            {
                                replacement =
                                    Some(with_blocked_host(&snapshot.document, host.clone()));
                            }
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("主机");
                        ui.add(egui::TextEdit::singleline(&mut self.host_input).char_limit(255));
                        ui.label("端口");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.port_input)
                                .char_limit(5)
                                .desired_width(55.0),
                        );
                        if ui.button("添加禁用").clicked() {
                            let host = self
                                .port_input
                                .parse::<u16>()
                                .ok()
                                .and_then(|port| ClipboardHost::new(&self.host_input, port));
                            match host {
                                Some(host) => {
                                    replacement = Some(with_blocked_host(&snapshot.document, host))
                                }
                                None => {
                                    input_error = Some("请输入有效主机和 1–65535 端口。".into())
                                }
                            }
                        }
                    });
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            if snapshot.document.blocked_hosts.is_empty() {
                                ui.weak("没有持续禁用的主机。");
                            }
                            for host in &snapshot.document.blocked_hosts {
                                ui.horizontal(|ui| {
                                    ui.label(endpoint_label(host));
                                    if ui.button("恢复每次询问").clicked() {
                                        let mut document = snapshot.document.clone();
                                        document.blocked_hosts.retain(|entry| entry != host);
                                        replacement = Some(document);
                                    }
                                });
                            }
                        });
                });
                ui.weak("撤销禁用只影响之后的新请求，已丢弃内容不会重放。");
            });
        self.open = open;
        if refresh {
            self.refresh();
        }
        if let Some(error) = input_error {
            self.shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .error = Some(error);
        }
        if let (Some(document), Some(snapshot)) = (replacement, view.snapshot) {
            match encode_clipboard_policy(&document) {
                Ok(document_json) => self.submit(ClipboardPolicyRequest {
                    operation: ClipboardPolicyOperation::Save as i32,
                    expected_revision: snapshot.revision,
                    document_json,
                    selected_session_id: self
                        .target
                        .borrow()
                        .map(|id| id.as_uuid().as_bytes().to_vec())
                        .unwrap_or_default(),
                }),
                Err(_) => {
                    self.shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .error = Some("主机策略无法编码。".into())
                }
            }
        }
    }
}
fn with_blocked_host(
    document: &ClipboardPolicyDocument,
    host: ClipboardHost,
) -> ClipboardPolicyDocument {
    let mut document = document.clone();
    if !document.blocked_hosts.contains(&host) {
        document.blocked_hosts.push(host);
        document.blocked_hosts.sort();
    }
    document
}
fn endpoint_label(host: &ClipboardHost) -> String {
    if host.host.contains(':') {
        format!("[{}]:{}", host.host, host.port)
    } else {
        format!("{}:{}", host.host, host.port)
    }
}
fn load_request(selected: Option<SessionId>) -> ClipboardPolicyRequest {
    ClipboardPolicyRequest {
        selected_session_id: selected
            .map(|id| id.as_uuid().as_bytes().to_vec())
            .unwrap_or_default(),
        ..Default::default()
    }
}
impl Drop for DesktopClipboardPolicy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
async fn send(
    config: &DesktopConnectionConfig,
    request: ClipboardPolicyRequest,
) -> Result<ClipboardPolicyResponse, String> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let resolved = config.resolve().map_err(|error| error.to_string())?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|error| error.to_string())?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        handshake.feature_bits = features::HOST_CLIPBOARD_POLICY;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|error| error.to_string())?;
        if negotiated.feature_bits & features::HOST_CLIPBOARD_POLICY == 0 {
            return Ok(ClipboardPolicyResponse::with_status(
                ClipboardPolicyStatus::Unsupported,
            ));
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::ClipboardPolicyRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|error| error.to_string())?;
        if response.request_id != 2 {
            return Err("mismatched policy response".into());
        }
        match response.payload {
            Some(envelope::Payload::ClipboardPolicyResponse(response)) => Ok(response),
            _ => Err("unexpected policy response".into()),
        }
    })
    .await
    .map_err(|_| "policy response timed out".to_owned())?
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn safety_panel_buttons_queue_revision_checked_changes_and_preserve_other_hosts() {
        let selected = SessionId::new();
        let selected_host = ClipboardHost::new("b.example.test", 2222).unwrap();
        let other_host = ClipboardHost::new("a.example.test", 22).unwrap();
        let (target, _targets) = tokio::sync::watch::channel(Some(selected));
        let (commands, mut requests) = tokio::sync::mpsc::channel(4);
        let (shutdown, _shutdown_receiver) = tokio::sync::watch::channel(false);
        let shared = Arc::new(Mutex::new(View {
            snapshot: Some(ClipboardPolicySnapshot {
                revision: 17,
                document: ClipboardPolicyDocument {
                    blocked_hosts: vec![other_host.clone()],
                    ..Default::default()
                },
            }),
            selected_host: Some(selected_host.clone()),
            ..Default::default()
        }));
        let mut panel = DesktopClipboardPolicy {
            open: true,
            shared: shared.clone(),
            target,
            commands,
            shutdown,
            worker: None,
            host_input: String::new(),
            port_input: "22".into(),
        };
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut draw = |events| {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 760.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| panel.draw(ui.ctx()),
            );
            output.textures_delta.clear();
            output.platform_output.accesskit_update.unwrap()
        };
        let click = |update: &egui::accesskit::TreeUpdate, label: &str| {
            let bounds = update
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == egui::accesskit::Role::Button && node.label() == Some(label)
                })
                .unwrap()
                .1
                .bounds()
                .unwrap();
            let position = egui::pos2(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            );
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        draw(vec![]);
        let update = draw(vec![]);
        draw(click(&update, "禁用此主机"));
        let request = requests.try_recv().unwrap();
        assert_eq!(request.operation, ClipboardPolicyOperation::Save as i32);
        assert_eq!(request.expected_revision, 17);
        assert_eq!(request.selected_session_id, selected.as_uuid().as_bytes());
        let document = decode_clipboard_policy(&request.document_json).unwrap();
        assert_eq!(
            document.blocked_hosts,
            vec![other_host, selected_host.clone()]
        );
        assert!(requests.try_recv().is_err());
        {
            let mut view = shared.lock().unwrap();
            view.busy = false;
            view.snapshot = Some(ClipboardPolicySnapshot {
                revision: 18,
                document,
            });
        }
        let update = draw(vec![]);
        draw(click(&update, "恢复每次询问"));
        let request = requests.try_recv().unwrap();
        assert_eq!(request.expected_revision, 18);
        assert_eq!(
            decode_clipboard_policy(&request.document_json)
                .unwrap()
                .blocked_hosts,
            vec![selected_host]
        );
        assert!(requests.try_recv().is_err());
    }
}
