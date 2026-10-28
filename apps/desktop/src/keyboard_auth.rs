//! A separate IPC worker remains responsive while SSH/SFTP creation awaits MFA.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_ipc::{
    Envelope, Handshake, KeyboardAuthAnswer, KeyboardAuthChallenge, VaultControlStatus as Status,
    VaultOperation as Op, VaultRequest, VaultResponse, client_handshake, envelope, features,
    read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use zeroize::Zeroize;
#[derive(Clone, Default)]
struct View {
    challenge: Option<KeyboardAuthChallenge>,
    error: Option<String>,
    busy: bool,
    generation: u64,
    catalog_revision: u64,
}
struct Connection {
    shared: Arc<Mutex<View>>,
    sender: tokio::sync::mpsc::Sender<VaultRequest>,
    stop: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Connection {
    fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(View::default()));
        let state = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<VaultRequest>(1);
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let worker=std::thread::Builder::new().name("cshell-auth-prompts".into()).spawn(move||{
            let Ok(runtime)=tokio::runtime::Builder::new_current_thread().enable_all().build() else{return;};
            runtime.block_on(async move {
                let mut timer=tokio::time::interval(Duration::from_millis(300));timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let command=tokio::select!{biased;_=stopped.changed()=>break,c=receiver.recv()=>match c{Some(c)=>c,None=>break},_=timer.tick()=>VaultRequest::new(Op::KeyboardAuthPoll)};
                    let respond=command.operation==Op::KeyboardAuthRespond as i32;
                    let result=tokio::select!{_=stopped.changed()=>break,r=send(&config,&command)=>r};
                    let mut state=state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let old=state.challenge.clone();let old_error=state.error.clone();let busy=state.busy;let old_revision=state.catalog_revision;
                    state.busy=false;
                    match result {
                        Ok(response) if response.status==Status::Ok as i32=>{state.catalog_revision=response.auth_catalog_revision;state.challenge=response.auth_challenge; if respond{state.error=None;} if !response.auth_notice.is_empty(){state.error=Some(response.auth_notice);}},
                        Ok(_)=>{if respond{state.error=Some("验证已过期或被取消，请重新连接。".into());}state.challenge=None;},
                        Err(error)=>{if respond{state.error=Some(error);}else{state.challenge=None;}},
                    }
                    if old!=state.challenge || old_error!=state.error || busy!=state.busy || old_revision!=state.catalog_revision {state.generation=state.generation.wrapping_add(1);}
                }
            });
        })?;
        Ok(Self {
            shared,
            sender,
            stop,
            worker: Some(worker),
        })
    }
    fn view(&self) -> View {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn submit(&self, answer: KeyboardAuthAnswer) -> bool {
        let mut request = VaultRequest::new(Op::KeyboardAuthRespond);
        request.auth_answer = Some(answer);
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.busy {
            return false;
        }
        if self.sender.try_send(request).is_err() {
            return false;
        }
        state.busy = true;
        state.generation = state.generation.wrapping_add(1);
        true
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
async fn send(
    config: &DesktopConnectionConfig,
    request: &VaultRequest,
) -> Result<VaultResponse, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let endpoint = config.resolve().map_err(|_| "后台服务不可用")?;
        let mut stream = transport::connect(&endpoint.endpoint)
            .await
            .map_err(|_| "后台服务未连接")?;
        let mut hello = Handshake::new(
            endpoint.daemon_instance_id.to_vec(),
            endpoint.instance_token.to_vec(),
        );
        hello.feature_bits = features::VAULT_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "后台认证失败")?;
        if negotiated.protocol_minor < 62 || negotiated.feature_bits & features::VAULT_CONTROL == 0
        {
            return Err("后台需要更新后才能使用键盘交互验证".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::VaultRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "验证请求发送失败")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "验证回复读取失败；请检查连接状态")?;
        if response.request_id != 2 {
            return Err("验证回复身份无效".into());
        }
        match response.payload {
            Some(envelope::Payload::VaultResponse(response)) => Ok(response),
            _ => Err("验证回复无效".into()),
        }
    })
    .await
    .map_err(|_| "验证提交超时；请检查连接状态后重新连接".to_owned())?
}
#[derive(Default)]
pub(crate) struct Dialog {
    connection: Option<Connection>,
    view: View,
    answers: Vec<String>,
    username: String,
    save_credentials: bool,
    last_catalog_refresh: u64,
    focus: bool,
}
impl Drop for Dialog {
    fn drop(&mut self) {
        self.answers.zeroize();
    }
}
impl Dialog {
    pub fn refresh_profiles(
        &mut self,
        profiles: Option<&crate::profile_connection::DesktopProfileConnection>,
    ) {
        let revision = self.view.catalog_revision;
        if revision > self.last_catalog_refresh {
            if let Some(profiles) = profiles {
                if profiles.catalog_revision() == Some(revision)
                    || profiles.request(crate::profile_connection::ProfileClientCommand::Refresh)
                {
                    self.last_catalog_refresh = revision;
                }
            }
        }
    }
    pub fn sync(&mut self, config: Option<&DesktopConnectionConfig>) -> bool {
        if self.connection.is_none() {
            if let Some(config) = config {
                self.connection = Connection::start(config.clone()).ok();
            }
        }
        let Some(view) = self.connection.as_ref().map(Connection::view) else {
            return false;
        };
        if view.generation == self.view.generation {
            return false;
        }
        if self.view.challenge.as_ref().map(|c| &c.id) != view.challenge.as_ref().map(|c| &c.id) {
            self.answers.zeroize();
            self.answers =
                vec![String::new(); view.challenge.as_ref().map_or(0, |c| c.prompts.len())];
            self.username = view
                .challenge
                .as_ref()
                .map(|c| c.username.clone())
                .unwrap_or_default();
            self.save_credentials = view.challenge.as_ref().is_some_and(|c| c.save_credentials);
            self.focus = true;
        }
        self.view = view;
        true
    }
    pub fn blocks_input(&self) -> bool {
        self.view.challenge.is_some() || self.view.error.is_some()
    }
    pub fn draw(&mut self, context: &egui::Context) {
        let Some(challenge) = self.view.challenge.as_ref() else {
            if let Some(error) = &self.view.error {
                let mut dismiss = false;
                egui::Modal::new(egui::Id::new("ssh-auth-error")).show(context, |ui| {
                    ui.set_max_width(460.0);
                    ui.heading("SSH 连接提示");
                    ui.label(error);
                    dismiss = ui.button("关闭").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape));
                });
                if dismiss {
                    self.view.error = None;
                    if let Some(connection) = &self.connection {
                        let mut state = connection
                            .shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.error = None;
                        state.generation = state.generation.wrapping_add(1);
                    }
                }
            }
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("ssh-keyboard-interactive")).show(context, |ui| {
            ui.set_width(ui.available_width().min(460.0));
            ui.heading(if challenge.vault_unlock {"解锁并连接"} else if challenge.login {
                "SSH 账号与认证"
            } else {
                "SSH 键盘交互验证"
            });
            ui.label(egui::RichText::new(&challenge.target).color(cshell_ui::theme::ACCENT));
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .show(ui, |ui| {
                    if !challenge.name.is_empty() {
                        ui.label(&challenge.name);
                    }
                    if !challenge.instructions.is_empty() {
                        ui.label(&challenge.instructions);
                    }
                });
            ui.weak(if challenge.vault_unlock {"本地凭据库验证，主密码不会发送给服务器。"}else{"按服务器提示填写；每轮最多等待 2 分钟。"});
            ui.add_enabled_ui(!self.view.busy, |ui| {
                if challenge.login {
                    ui.label("用户名");
                    ui.add_enabled_ui(challenge.username_editable, |ui| {
                        let response =
                            ui.add(egui::TextEdit::singleline(&mut self.username).char_limit(128));
                        let focus_id = egui::Id::new(("ssh-login-focus", &challenge.id));
                        if self.focus || !ui.data(|d| d.get_temp::<bool>(focus_id).unwrap_or(false))
                        {
                            response.request_focus();
                            ui.data_mut(|d| d.insert_temp(focus_id, true));
                            self.focus = false;
                        }
                    });
                }

                egui::ScrollArea::vertical()
                    .id_salt("ssh-auth-fields")
                    .max_height((context.content_rect().height() - 260.0).clamp(80.0, 320.0))
                    .show(ui, |ui| {
                        for (index, prompt) in challenge.prompts.iter().enumerate() {
                            ui.label(if prompt.text.is_empty() {
                                "验证内容"
                            } else {
                                &prompt.text
                            });
                            let response = if prompt.echo {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.answers[index])
                                        .char_limit(4096),
                                )
                            } else {
                                crate::secret_edit::edit(
                                    ui,
                                    &mut self.answers[index],
                                    &format!("ssh-auth-{index}"),
                                    4096,
                                )
                            };
                            let focus_id = egui::Id::new(("ssh-auth-focus", &challenge.id));
                            if index == 0
                                && !(challenge.login && challenge.username_editable)
                                && (self.focus
                                    || !ui.data(|d| d.get_temp::<bool>(focus_id).unwrap_or(false)))
                            {
                                response.request_focus();
                                ui.data_mut(|d| d.insert_temp(focus_id, true));
                                self.focus = false;
                            }
                        }
                    });
                if let Some(error) = &self.view.error {
                    ui.colored_label(cshell_ui::theme::WARNING, error);
                }
                if challenge.login {
                    ui.checkbox(
                        &mut self.save_credentials,
                        "保存账号与密码（认证成功后保存）",
                    );
                }
                let ready = (!challenge.login
                    || (!self.username.trim().is_empty()
                        && !self.username.trim().chars().any(|c| c.is_control() || c.is_whitespace())))
                    && (!challenge.password_required
                        || self.answers.first().is_some_and(|s| !s.is_empty()));
                ui.horizontal(|ui| {
                    submit = ui
                        .add_enabled(ready, egui::Button::new(if challenge.vault_unlock {"解锁并连接"}else{"继续验证"}))
                        .clicked();
                    cancel = ui.button("取消连接").clicked();
                });
                if ready && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
            if self.view.busy {
                ui.spinner();
                ui.label("正在提交验证…");
            }
        });
        if submit || cancel {
            let answer = KeyboardAuthAnswer {
                id: challenge.id.clone(),
                answers: if cancel {
                    Vec::new()
                } else {
                    std::mem::take(&mut self.answers)
                },
                cancel,
                username: if !cancel && challenge.login {
                    self.username.trim().to_owned()
                } else {
                    String::new()
                },
                save_credentials: !cancel && challenge.login && self.save_credentials,
            };
            let sent = self.connection.as_ref().is_some_and(|c| c.submit(answer));
            self.answers.zeroize();
            self.answers = vec![String::new(); challenge.prompts.len()];
            if sent {
                self.view.busy = true;
            } else {
                self.view.error = Some("验证请求未提交，请重试。".into());
            }
        }
    }
}

#[cfg(test)]
mod tests;
