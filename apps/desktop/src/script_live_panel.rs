//! Explicit daemon-owned live script review/run/revoke UI.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::*;
use cshell_script_runtime::*;
use std::{
    collections::BTreeSet,
    sync::mpsc,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeExtras {
    secret_parameters: BTreeSet<String>,
    tasks: Vec<ScriptTaskBinding>,
    trigger_event: Option<u64>,
    #[serde(default)]
    trigger_on_next_match: bool,
}
type Reply = Result<ScriptControlReply, String>;
pub(super) struct Panel {
    config: Option<DesktopConnectionConfig>,
    scope: String,
    epoch: u64,
    stamp: Option<(
        [u8; 32],
        BTreeSet<ScriptCapability>,
        Option<SessionId>,
        String,
    )>,
    pending: Option<(u64, mpsc::Receiver<Reply>)>,
    revoking: Option<mpsc::Receiver<Reply>>,
    review: Option<ScriptLiveReview>,
    submitted: Option<(ScriptRunId, [u8; 32])>,
    run: Option<ScriptLiveView>,
    target: Option<SessionId>,
    confirmed: bool,
    status: String,
    unknown: bool,
    recovery_clear: bool,
    last_poll: Instant,
    reset_requested: bool,
}
impl Default for Panel {
    fn default() -> Self {
        Self {
            config: None,
            scope: "{\"secret_parameters\":[],\"tasks\":[],\"trigger_event\":null}".into(),
            epoch: 0,
            stamp: None,
            pending: None,
            revoking: None,
            review: None,
            submitted: None,
            run: None,
            target: None,
            confirmed: false,
            status: String::new(),
            unknown: false,
            recovery_clear: false,
            last_poll: Instant::now(),
            reset_requested: false,
        }
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        if self.active()
            && let (Some(config), Some(run)) = (&self.config, &self.run)
        {
            let _ = spawn_query(
                config.clone(),
                ScriptControlCommand::Revoke { run: run.run },
            );
        }
    }
}
impl Panel {
    pub fn take_recovery_confirmation(&mut self) -> bool {
        std::mem::take(&mut self.reset_requested)
    }
    pub fn invalidate_confirmation(&mut self) {
        self.confirmed = false;
    }
    pub fn trigger(&mut self, event: u64) {
        if self.active() || self.pending.is_some() {
            return;
        }
        self.scope = serde_json::json!({"secret_parameters":[],"tasks":[],"trigger_event":event})
            .to_string();
        self.review = None;
        self.confirmed = false;
        self.status = "已选触发事件；请核对匹配script_id的草稿与当前会话并重新预览".into();
    }
    pub fn active(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|v| v.state.active() && v.report.is_none() && !v.failed)
    }
    fn request(&mut self, command: ScriptControlCommand) {
        if self.pending.is_some() {
            return;
        }
        let start = matches!(&command, ScriptControlCommand::Start { .. });
        let receiver = if matches!(&command, ScriptControlCommand::Recover { confirmed: true }) {
            spawn_recovery()
        } else if let Some(config) = &self.config {
            spawn_query(config.clone(), command)
        } else {
            Err("没有已配置的daemon连接".into())
        };
        match receiver {
            Ok(receiver) => {
                self.pending = Some((self.epoch, receiver));
                if start {
                    self.unknown = true;
                }
            }
            Err(error) => self.status = error,
        }
    }
    fn revoke(&mut self) {
        if self.revoking.is_some() {
            return;
        }
        let (Some(config), Some(run)) = (&self.config, &self.run) else {
            return;
        };
        match spawn_query(
            config.clone(),
            ScriptControlCommand::Revoke { run: run.run },
        ) {
            Ok(receiver) => self.revoking = Some(receiver),
            Err(error) => self.status = error,
        }
    }
    pub fn observe_session(&mut self, session: Option<SessionId>) {
        if self.active() && self.target.is_some() && self.target != session {
            self.revoke();
        }
    }
    fn adopt(&mut self, view: ScriptLiveView) {
        if self
            .submitted
            .is_some_and(|(review, digest)| view.review != review || view.document_sha256 != digest)
        {
            self.unknown = true;
            self.status = "运行回执与本次预览不匹配".into();
            return;
        }
        self.unknown = view.state == ScriptLiveState::Unknown;
        self.run = Some(view);
    }
    pub fn poll(&mut self, config: Option<&DesktopConnectionConfig>) -> bool {
        if let Some(config) = config {
            self.config = Some(config.clone());
        }
        let mut changed = false;
        if let Some(receiver) = &self.revoking
            && let Some(result) = receive(receiver)
        {
            self.revoking = None;
            changed = true;
            match result {
                Ok(ScriptControlReply::Run(view)) => self.adopt(view),
                _ => {
                    self.unknown = true;
                    self.status = "撤销回执未确认，请读取运行状态".into();
                }
            }
        }
        if let Some((epoch, receiver)) = &self.pending
            && let Some(result) = receive(receiver)
        {
            let same = *epoch == self.epoch;
            self.pending = None;
            changed = true;
            match result {
                Ok(ScriptControlReply::Review(review)) if same => {
                    self.target = review.scope.session;
                    self.review = Some(review);
                    self.confirmed = false;
                    self.status = "预览不构成授权，请核对后确认".into();
                }
                Ok(ScriptControlReply::Run(view)) => self.adopt(view),
                Ok(ScriptControlReply::Runs(views)) => {
                    if let Some((review, digest)) = self.submitted {
                        let matches = views
                            .into_iter()
                            .filter(|v| v.review == review && v.document_sha256 == digest)
                            .collect::<Vec<_>>();
                        if matches.len() == 1 {
                            self.adopt(matches[0].clone());
                        } else {
                            self.status = "尚未找到唯一运行；不重放旧启动，可在CLI核对列表".into();
                        }
                    } else {
                        self.status = format!("daemon保留 {} 条脚本运行回执", views.len());
                    }
                }
                Ok(ScriptControlReply::Recovery(report)) => {
                    self.recovery_clear = report.busy == 0 && report.unresolved == 0;
                    self.status = format!(
                        "资源恢复：回收 {}，占用 {}，未明 {}",
                        report.recovered, report.busy, report.unresolved
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    self.status = error;
                }
            }
        }
        if self.pending.is_none()
            && self.active()
            && self.last_poll.elapsed() >= Duration::from_millis(250)
        {
            self.last_poll = Instant::now();
            if let Some(run) = &self.run {
                self.request(ScriptControlCommand::Get { run: run.run });
            }
        }
        changed
    }
    pub fn indicator(&mut self, context: &egui::Context) -> bool {
        let mut show = false;
        if self.active() || self.unknown {
            cshell_ui::form::dialog(&context, "实时脚本授权与运行")
                .id(egui::Id::new("live-script-status"))
                .collapsible(false)
                .resizable(false)
                .show(context, |ui| {
                    ui.label(if self.unknown {
                        "运行或效果未确认；请读取回执，勿重放启动。"
                    } else {
                        "本次单次授权有效；关闭工具窗口继续运行。"
                    });
                    if self.active() && ui.button("撤销本次授权").clicked() {
                        self.revoke();
                    }
                    if ui.button("查看权限与状态").clicked() {
                        show = true;
                    }
                });
            context.request_repaint_after(Duration::from_millis(100));
        }
        show
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        source: &str,
        selected: &BTreeSet<ScriptCapability>,
        session: Option<SessionId>,
        values: &mut Zeroizing<String>,
        external_unknown: bool,
    ) {
        ui.separator();
        ui.heading("实时脚本：单次授权");
        ui.label("仅绑定本次当前会话；参数发送、字面等待、Profile摘要及已冻结任务均逐次检查。通知仅计数。关闭/替换会话、锁屏或认证代次变化会停止后续调用。");
        ui.label(format!("当前会话：{session:?}"));
        let busy = self.pending.is_some() || self.active();
        ui.add_enabled_ui(!busy, |ui| {
            ui.label("目标附加范围 JSON：秘密参数名称、任务名称/运行ID/版本、trigger_event；或设置trigger_on_next_match=true等待一次匹配。秘密值不会送入JS。");
            ui.add(egui::TextEdit::multiline(&mut self.scope).char_limit(16 * 1024).desired_rows(3));
        });
        let digest = decode_script_document(source.as_bytes())
            .ok()
            .and_then(|d| script_document_fingerprint(&d).ok());
        if let Some(digest) = digest {
            let stamp = (digest, selected.clone(), session, self.scope.clone());
            if self.stamp.as_ref() != Some(&stamp) {
                self.stamp = Some(stamp);
                self.epoch = self.epoch.wrapping_add(1);
                self.review = None;
                self.confirmed = false;
            }
        } else {
            self.review = None;
            self.confirmed = false;
        }
        ui.add_enabled_ui(!busy && !self.unknown, |ui| {
            if ui.button("生成本次实时权限预览").clicked() {
                let proposal = (|| {
                    let document =
                        decode_script_document(source.as_bytes()).map_err(|e| e.to_string())?;
                    let extras: ScopeExtras = serde_json::from_str(&self.scope)
                        .map_err(|_| "附加范围JSON无效".to_owned())?;
                    let proposal = ScriptLiveProposal {
                        document,
                        selected: selected.clone(),
                        scope: ScriptLiveScope {
                            session,
                            secret_parameters: extras.secret_parameters,
                            tasks: extras.tasks,
                            trigger_event: extras.trigger_event,
                            trigger_on_next_match: extras.trigger_on_next_match,
                        },
                    };
                    validate_live_proposal(&proposal).map_err(|e| e.to_string())?;
                    Ok(proposal)
                })();
                match proposal {
                    Ok(proposal) => self.request(ScriptControlCommand::Preview { proposal }),
                    Err(error) => self.status = error,
                }
            }
        });
        if let Some(review) = self.review.clone() {
            egui::CollapsingHeader::new("完整权限与目标预览")
                .default_open(true)
                .show(ui, |ui| {
                    let mut json = serde_json::to_string_pretty(&review).unwrap_or_default();
                    ui.add(
                        egui::TextEdit::multiline(&mut json)
                            .interactive(false)
                            .desired_rows(12)
                            .desired_width(f32::INFINITY),
                    );
                });
            ui.add_enabled_ui(!busy && !self.unknown, |ui| {
                ui.checkbox(&mut self.confirmed, "已核对完整源代码、摘要、预算、当前会话、秘密参数目标和任务ID/版本，确认仅本次执行");
                let label = if review.scope.trigger_on_next_match { "签发单次授权，等待下一次匹配" } else { "签发单次授权并运行" };
                if ui.add_enabled(self.confirmed, egui::Button::new(label)).clicked() {
                    match serde_json::from_str::<ScriptParameterValues>(values) {
                        Ok(parameters) => {
                            self.submitted = Some((review.review, review.plan.document_sha256)); self.target = review.scope.session;
                            self.request(ScriptControlCommand::Start { review: review.review, parameters, confirmed: true });
                            values.zeroize(); values.push_str("{\"values\":{}}"); self.review = None; self.confirmed = false; self.recovery_clear = false;
                        }
                        Err(_) => self.status = "临时参数JSON无效".into(),
                    }
                }
            });
        }
        if self.active() && ui.button("撤销并等待运行回执").clicked() {
            self.revoke();
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.pending.is_none(),
                    egui::Button::new("读取运行/查找未明启动"),
                )
                .clicked()
            {
                self.request(ScriptControlCommand::List);
            }
            if ui
                .add_enabled(
                    self.pending.is_none() && !self.active(),
                    egui::Button::new("恢复已退出的脚本资源"),
                )
                .clicked()
            {
                self.request(ScriptControlCommand::Recover { confirmed: true });
            }
        });
        if (self.unknown || external_unknown)
            && self.recovery_clear
            && ui
                .button("已在终端/任务页核对未明结果，允许重新预览")
                .clicked()
        {
            self.unknown = false;
            self.run = None;
            self.review = None;
            self.submitted = None;
            self.confirmed = false;
            self.reset_requested = true;
        }
        if let Some(view) = &self.run {
            ui.label(format!("运行 {}：{:?}", view.run, view.state));
            if let Some(r) = &view.report {
                ui.label(format!(
                    "{:?}；操作 {}；通知 {}；退出 {}；清理 {}；效果回执 {}",
                    r.status,
                    r.operations,
                    r.notifications,
                    r.exit_confirmed,
                    r.sandbox_cleanup_confirmed,
                    r.effects_confirmed
                ));
            }
        }
        ui.label(&self.status);
    }
}
fn receive(receiver: &mpsc::Receiver<Reply>) -> Option<Reply> {
    match receiver.try_recv() {
        Ok(r) => Some(r),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(_) => Some(Err("脚本IPC线程退出，结果未确认".into())),
    }
}
fn spawn_recovery() -> Result<mpsc::Receiver<Reply>, String> {
    let (send, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("cshell-script-resource-recovery".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "脚本恢复运行时不可用".to_owned())
                .and_then(|runtime| {
                    let result = runtime
                        .block_on(recover_script_resources_bounded())
                        .map(ScriptControlReply::Recovery)
                        .map_err(|e| e.to_string());
                    runtime.shutdown_timeout(Duration::from_secs(1));
                    result
                });
            let _ = send.send(result);
        })
        .map_err(|_| "脚本恢复线程不可用".to_owned())?;
    Ok(receive)
}
fn spawn_query(
    config: DesktopConnectionConfig,
    command: ScriptControlCommand,
) -> Result<mpsc::Receiver<Reply>, String> {
    let (send, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("cshell-script-control-ipc".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "脚本IPC运行时不可用".into())
                .and_then(|runtime| {
                    let reply = runtime.block_on(query(&config, &command));
                    runtime.shutdown_timeout(Duration::from_secs(1));
                    reply
                });
            let _ = send.send(result);
        })
        .map_err(|_| "脚本IPC线程不可用".to_owned())?;
    Ok(receive)
}
async fn query(config: &DesktopConnectionConfig, command: &ScriptControlCommand) -> Reply {
    use cshell_ipc::{
        Envelope, Handshake, ScriptControlStatus, client_handshake, envelope, features,
        read_envelope, transport, write_envelope,
    };
    let resolved = config.resolve().map_err(|_| "daemon发现失败".to_owned())?;
    tokio::time::timeout(Duration::from_secs(15), async {
        #[cfg(windows)]
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| "daemon连接失败".to_owned())?;
        #[cfg(unix)]
        let mut stream = transport::connect(std::path::Path::new(&resolved.endpoint))
            .await
            .map_err(|_| "daemon连接失败".to_owned())?;
        let mut hello = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        hello.feature_bits = features::SCRIPT_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon认证失败".to_owned())?;
        if negotiated.feature_bits & features::SCRIPT_CONTROL == 0 {
            return Err("daemon未协商脚本执行能力".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::ScriptControlRequest(
                    command.request().map_err(|e| e.to_string())?,
                )),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "发送结果未确认；读取状态，勿重放启动".to_owned())?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "回复未确认；读取状态，勿重放启动".to_owned())?;
        if reply.request_id != 2 {
            return Err("脚本回复身份错误".into());
        }
        let Some(envelope::Payload::ScriptControlResponse(reply)) = reply.payload else {
            return Err("脚本回复类型错误".into());
        };
        if reply.status != ScriptControlStatus::Ok as i32 {
            return Err("脚本请求被拒绝；重新读取状态".into());
        }
        if reply.document_json.len() > 256 * 1024 {
            return Err("脚本回复超限".into());
        }
        let reply = serde_json::from_slice(&reply.document_json)
            .map_err(|_| "脚本回复JSON错误".to_owned())?;
        validate_script_control_reply(command, &reply)
            .map_err(|_| "脚本完整回执不匹配".to_owned())?;
        Ok(reply)
    })
    .await
    .map_err(|_| "脚本IPC超过15秒；读取状态，勿重放启动".to_owned())?
}
