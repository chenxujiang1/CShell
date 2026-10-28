use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::{SshProviderReport, decode_ssh_provider_report};
use cshell_domain::{SshAlgorithmPolicy, SshCapability, SshProviderId, SshProviderRequirements};
use cshell_ipc::{
    Envelope, Handshake, SshProviderRequest, SshProviderStatus, client_handshake, envelope,
    features, read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
struct View {
    report: Option<Arc<SshProviderReport>>,
    busy: bool,
    generation: u64,
    error: Option<String>,
}
struct Connection {
    view: Arc<Mutex<View>>,
    sender: Option<tokio::sync::mpsc::Sender<Option<SshProviderRequirements>>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Connection {
    fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let view = Arc::new(Mutex::new(View::default()));
        let shared = view.clone();
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<Option<SshProviderRequirements>>(1);
        let (shutdown, mut stop) = tokio::sync::watch::channel(false);
        let worker = std::thread::Builder::new().name("cshell-ssh-provider-ipc".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(_) => { shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error = Some("SSH 能力查询线程无法启动".into()); return; }
            };
            runtime.block_on(async {
                loop {
                    let input = tokio::select! { biased; _ = stop.changed() => break,
                        input = receiver.recv() => match input { Some(input) => input, None => break } };
                    let result = tokio::select! { biased; _ = stop.changed() => break,
                        result = query(&config, input.as_ref()) => result };
                    let mut view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.busy = false;
                    view.generation = view.generation.wrapping_add(1);
                    match result {
                        Ok(report) => { view.report = Some(Arc::new(report)); view.error = None; },
                        Err(error) => { view.report = None; view.error = Some(error); },
                    }
                }
            });
        })?;
        Ok(Self {
            view,
            sender: Some(sender),
            shutdown,
            worker: Some(worker),
        })
    }
    fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn request(&self, input: Option<SshProviderRequirements>) -> bool {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        let sent = self
            .sender
            .as_ref()
            .is_some_and(|s| s.try_send(input).is_ok());
        if sent {
            view.busy = true;
            view.error = None;
        }
        sent
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
async fn query(
    config: &DesktopConnectionConfig,
    input: Option<&SshProviderRequirements>,
) -> Result<SshProviderReport, String> {
    let resolved = config.resolve().map_err(|_| "daemon 发现失败".to_owned())?;
    tokio::time::timeout(Duration::from_secs(20), async {
        #[cfg(windows)]
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| "daemon 连接失败".to_owned())?;
        #[cfg(unix)]
        let mut stream = transport::connect(std::path::Path::new(&resolved.endpoint))
            .await
            .map_err(|_| "daemon 连接失败".to_owned())?;
        let mut hello = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        hello.feature_bits = features::SSH_PROVIDER_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon 认证失败".to_owned())?;
        if negotiated.feature_bits & features::SSH_PROVIDER_CONTROL == 0 {
            return Err("daemon 不支持 SSH 能力查询".into());
        }
        let requirements_json = input
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| "SSH 需求编码失败".to_owned())?
            .unwrap_or_default();
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::SshProviderRequest(SshProviderRequest {
                    requirements_json,
                })),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "SSH 能力查询发送失败".to_owned())?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "SSH 能力查询回复失败".to_owned())?;
        if response.request_id != 2 {
            return Err("SSH 能力回复身份不匹配".into());
        }
        let Some(envelope::Payload::SshProviderResponse(response)) = response.payload else {
            return Err("SSH 能力回复类型不匹配".into());
        };
        if response.detail.len() > 1024 {
            return Err("SSH 能力回复超限".into());
        }
        if response.status != SshProviderStatus::Ok as i32 {
            return Err("SSH 能力查询被拒绝或不可用".into());
        }
        decode_ssh_provider_report(&response.document_json, input).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "SSH 能力查询超时".to_owned())?
}

#[derive(Default)]
pub(super) struct SshProviderPanel {
    pub open: bool,
    connection: Option<Connection>,
    requirements: SshProviderRequirements,
    error: Option<String>,
}
impl SshProviderPanel {
    pub fn generation(&self) -> Option<u64> {
        self.connection.as_ref().map(|c| c.view().generation)
    }
    pub fn draw(&mut self, context: &egui::Context, config: Option<&DesktopConnectionConfig>) {
        if !self.open {
            return;
        }
        if self.connection.is_none()
            && self.error.is_none()
            && let Some(config) = config
        {
            match Connection::start(config.clone()) {
                Ok(c) => {
                    c.request(None);
                    self.connection = Some(c);
                }
                Err(_) => self.error = Some("SSH 能力查询线程无法启动".into()),
            }
        }
        context.request_repaint_after(Duration::from_millis(250));
        let view = self
            .connection
            .as_ref()
            .map(Connection::view)
            .unwrap_or_default();
        let ready = self.connection.is_some() && !view.busy;
        let mut open = self.open;
        let mut query_input = None;
        cshell_ui::form::dialog(&context, "SSH 能力").open(&mut open).default_size([700.0, 640.0]).show(context, |ui| {
            ui.weak("查看本机已启用的 SSH 后端，并检查连接需求。此处不连接服务器或读取凭据；支持的功能仍需服务器允许。");
            if ui.add_enabled(!view.busy, egui::Button::new("重新读取能力")).clicked() {
                if self.connection.is_none() { self.error = None; } else { query_input = Some(None); }
            }
            if let Some(error) = self.error.as_ref().or(view.error.as_ref()) { ui.colored_label(cshell_ui::theme::WARNING, error); }
            if view.busy { ui.spinner(); }
            if let Some(report) = &view.report {
                for provider in &report.catalog.providers {
                    ui.heading(format!("{:?} · 最多 {} 个跳板", provider.id, provider.max_jump_hops));
                    ui.label(provider.capabilities.iter().map(|c| c.label()).collect::<Vec<_>>().join(", "));
                }
            }
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label("后端需求");
                ui.selectable_value(&mut self.requirements.provider, None, "自动选择");
                for (id, name) in [(SshProviderId::Russh, "russh"), (SshProviderId::SystemOpenSsh, "系统 OpenSSH"), (SshProviderId::Libssh, "libssh")] {
                    ui.selectable_value(&mut self.requirements.provider, Some(id), name);
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("跳板数量"); ui.add(egui::DragValue::new(&mut self.requirements.jump_hops).range(0..=255));
                ui.label("算法策略");
                for (policy, label) in [(SshAlgorithmPolicy::Modern, "现代"), (SshAlgorithmPolicy::Compatible, "兼容"), (SshAlgorithmPolicy::Legacy, "旧式")] { ui.selectable_value(&mut self.requirements.algorithm_policy, policy, label); }
            });
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                let columns = if ui.available_width() < 640.0 { 2 } else { 3 };
                egui::Grid::new("ssh-capability-requirements").num_columns(columns).show(ui, |ui| {
                    for (index, capability) in SshCapability::ALL.iter().copied().enumerate() {
                        let mut selected = self.requirements.capabilities.contains(&capability);
                        if ui.checkbox(&mut selected, capability.label()).changed() {
                            if selected { self.requirements.capabilities.insert(capability); } else { self.requirements.capabilities.remove(&capability); }
                        }
                        if index % columns == columns - 1 { ui.end_row(); }
                    }
                });
            });
            if ui.add_enabled(ready, egui::Button::new("检查当前需求")).clicked() { query_input = Some(Some(self.requirements.clone())); }
            if let Some(report) = &view.report && let Some(evaluation) = &report.evaluation {
                if evaluation.requirements != self.requirements { ui.weak("需求已变化，请重新检查。"); }
                else {
                    match evaluation.selected {
                        Some(provider) => { ui.label(format!("需求匹配：{provider:?}")); },
                        None => { ui.colored_label(cshell_ui::theme::WARNING, "当前没有满足需求的后端"); },
                    }
                    for refusal in &evaluation.refusals {
                        use cshell_application::SshProviderRefusal as R;
                        let detail = match refusal {
                            R::Unregistered { provider } => format!("{provider:?} 未启用"),
                            R::MissingCapabilities { provider, capabilities } => format!("{provider:?} 未实现：{}", capabilities.iter().map(|c| c.label()).collect::<Vec<_>>().join(", ")),
                            R::AlgorithmPolicy { provider, policy } => format!("{provider:?} 不支持 {policy:?} 算法策略"),
                            R::JumpDepth { provider, requested, maximum } => format!("{provider:?} 支持最多 {maximum} 跳，当前要求 {requested} 跳"),
                        };
                        ui.label(detail);
                    }
                }
            }
        });
        self.open = open;
        if let Some(input) = query_input
            && let Some(c) = &self.connection
        {
            c.request(input);
        }
    }
}
