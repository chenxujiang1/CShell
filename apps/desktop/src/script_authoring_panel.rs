//! Script/recording authoring and explicitly confirmed offline execution.
//! No live terminal/authentication payload enters the recorder or JS worker.
use cshell_application::*;
use cshell_domain::*;
use cshell_script_runtime::*;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    sync::mpsc,
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Copy)]
enum Kind {
    Script,
    Recording,
}
struct FileResult {
    kind: Kind,
    body: Option<String>,
}
type FileReply = Result<FileResult, String>;
struct ActiveRun {
    cancel: ScriptCancellation,
    receiver: mpsc::Receiver<Result<ScriptRunReport, ScriptWorkerError>>,
}
pub(super) struct ScriptAuthoringPanel {
    recorder: ScriptRecorder,
    recording_json: String,
    script_json: String,
    selected: BTreeSet<ScriptCapability>,
    plan: Option<ScriptPermissionPlan>,
    status: String,
    parameter: String,
    append_newline: bool,
    wait_ms: u32,
    sleep_ms: u32,
    notification: String,
    path: String,
    epoch: u64,
    pending: Option<(u64, mpsc::Receiver<FileReply>)>,
    parameter_values_json: Zeroizing<String>,
    offline_confirmed: bool,
    active_run: Option<ActiveRun>,
    run_report: Option<ScriptRunReport>,
    run_unknown: bool,
    live: crate::script_live_panel::Panel,
}
impl Default for ScriptAuthoringPanel {
    fn default() -> Self {
        Self {
            recorder: Default::default(),
            recording_json: serde_json::to_string_pretty(&ScriptRecording::default())
                .unwrap_or_default(),
            script_json: String::new(),
            selected: BTreeSet::new(),
            plan: None,
            status: String::new(),
            parameter: "input1".into(),
            append_newline: false,
            wait_ms: 3000,
            sleep_ms: 100,
            notification: String::new(),
            path: String::new(),
            epoch: 0,
            pending: None,
            parameter_values_json: Zeroizing::new("{\"values\":{}}".into()),
            offline_confirmed: false,
            active_run: None,
            run_report: None,
            run_unknown: false,
            live: Default::default(),
        }
    }
}
impl Drop for ScriptAuthoringPanel {
    fn drop(&mut self) {
        if let Some(run) = &self.active_run {
            run.cancel.cancel();
        }
        self.script_json.zeroize();
    }
}
impl ScriptAuthoringPanel {
    pub fn select_trigger_event(&mut self, event: u64) {
        self.live.trigger(event);
    }
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.plan = None;
        self.offline_confirmed = false;
    }
    fn snapshot(&mut self) {
        self.recording_json =
            serde_json::to_string_pretty(self.recorder.document()).unwrap_or_default();
        self.invalidate();
    }
    pub fn observe_session(&mut self, session: Option<SessionId>) {
        self.live.observe_session(session);
        let was_active = self.recorder.active();
        self.recorder.observe_session(session);
        if was_active && !self.recorder.active() {
            self.snapshot();
            self.status = "录制因会话变化停止，已留下待核对标记".into();
        }
    }
    pub fn input_omitted(&mut self, session: SessionId, broadcast: bool) {
        if !self.recorder.active() {
            return;
        }
        let reason = if broadcast {
            RecordingOmission::InputBroadcast
        } else {
            RecordingOmission::TerminalInput
        };
        let before = (
            self.recorder.document().steps.len(),
            self.recorder.active(),
            self.recorder.document().truncated,
        );
        let result = self.recorder.record_omission(session, reason);
        let after = (
            self.recorder.document().steps.len(),
            self.recorder.active(),
            self.recorder.document().truncated,
        );
        if before != after {
            self.snapshot();
        }
        if let Err(error) = result {
            self.status = error.to_string();
        }
    }
    pub fn indicator(&mut self, context: &egui::Context) -> bool {
        let mut show = self.live.indicator(context);
        if self.recorder.active() {
            cshell_ui::form::dialog(&context, "脚本录制中")
                .id(egui::Id::new("script-recording-indicator"))
                .collapsible(false)
                .resizable(false)
                .show(context, |ui| {
                    ui.label("仅当前会话的输入省略标记；不保存输入内容或时间。");
                    if ui.button("停止并查看草稿").clicked() {
                        self.recorder.stop();
                        self.snapshot();
                        show = true;
                    }
                });
        }
        if let Some(run) = &self.active_run {
            cshell_ui::form::dialog(&context, "离线脚本运行中")
                .id(egui::Id::new("script-run-indicator"))
                .collapsible(false)
                .resizable(false)
                .show(context, |ui| {
                    ui.label("关闭工具窗口不停止运行；取消后等待进程退出确认。");
                    if ui.button("取消离线运行").clicked() {
                        run.cancel.cancel();
                    }
                    if ui.button("查看运行状态").clicked() {
                        show = true;
                    }
                });
            context.request_repaint_after(std::time::Duration::from_millis(100));
        }
        show
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = self.poll_file();
        if let Some(run) = &self.active_run {
            let result = match run.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    run.cancel.cancel();
                    self.run_unknown = true;
                    Some(Err(ScriptWorkerError::Unavailable))
                }
            };
            if let Some(result) = result {
                self.active_run = None;
                self.offline_confirmed = false;
                match result {
                    Ok(report) => {
                        self.run_unknown |= !report.exit_confirmed
                            || !report.sandbox_cleanup_confirmed
                            || !report.effects_confirmed
                            || report.status == ScriptRunStatus::Unknown;
                        self.run_report = Some(report);
                        self.status = "离线运行已返回状态；未访问实时会话".into();
                    }
                    Err(error) => self.status = error.to_string(),
                }
                changed = true;
            }
        }
        changed
    }
    fn poll_file(&mut self) -> bool {
        let Some((epoch, receiver)) = &self.pending else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("脚本文件线程已退出".into()),
        };
        let same = *epoch == self.epoch;
        self.pending = None;
        match result {
            Ok(FileResult {
                body: Some(body),
                kind,
            }) if same => {
                match kind {
                    Kind::Script => {
                        self.script_json = body;
                        self.selected.clear();
                    }
                    Kind::Recording => self.recording_json = body,
                }
                self.invalidate();
                self.status = "已读取并检查文档结构，尚未执行或授权".into();
            }
            Ok(FileResult { body: Some(_), .. }) => {
                self.status = "草稿已变化，未采用迟到的读取结果".into()
            }
            Ok(_) => self.status = "已写入新文件，未执行或授权".into(),
            Err(error) => self.status = error,
        }
        true
    }
    fn file(&mut self, kind: Kind, export: bool) {
        if self.pending.is_some() {
            return;
        }
        let path = self.path.clone();
        let body = if export {
            let result = match kind {
                Kind::Script => decode_script_document(self.script_json.as_bytes()).and_then(|d| {
                    serde_json::to_string_pretty(&d).map_err(|_| ScriptDraftError::Json)
                }),
                Kind::Recording => decode_script_recording(self.recording_json.as_bytes())
                    .and_then(|d| {
                        serde_json::to_string_pretty(&d).map_err(|_| ScriptDraftError::Json)
                    }),
            };
            match result {
                Ok(body) if body.len() <= MAX_SCRIPT_DOCUMENT_BYTES => Some(body),
                Ok(_) => {
                    self.status = ScriptDraftError::Limit.to_string();
                    return;
                }
                Err(error) => {
                    self.status = error.to_string();
                    return;
                }
            }
        } else {
            None
        };
        let (sender, receiver) = mpsc::channel();
        let epoch = self.epoch;
        match std::thread::Builder::new()
            .name("script-draft-file".into())
            .spawn(move || {
                let result = file_job(&path, kind, body);
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.pending = Some((epoch, receiver));
                self.status = "正在处理文档文件".into();
            }
            Err(_) => self.status = "无法启动脚本文件线程".into(),
        }
    }
    fn append(&mut self, step: ScriptRecordingStep) {
        let result = if self.recorder.active() {
            let result = self.recorder.append(step);
            self.snapshot();
            result
        } else {
            decode_script_recording(self.recording_json.as_bytes()).and_then(|mut recording| {
                recording.steps.push(step);
                validate_script_recording(&recording)?;
                self.recording_json =
                    serde_json::to_string_pretty(&recording).map_err(|_| ScriptDraftError::Json)?;
                self.invalidate();
                Ok(())
            })
        };
        self.status = match result {
            Ok(()) => "已追加声明式步骤；没有发送输入".into(),
            Err(error) => error.to_string(),
        };
    }
    fn draw_authoring(&mut self, ui: &mut egui::Ui, session: Option<SessionId>) {
        ui.label("P3-060 草稿与权限声明：核对与导出不会执行；离线运行须另行确认。");
        ui.label("录制仅观察当前窗口提交的终端输入意图，内容一律省略，不保存输出/密码/KBI回答。连续输入不保存按键次数或时间；省略/截断必须人工编辑核对后才能生成脚本。");
        ui.label("关闭工具窗口仍继续录制；会话切换/结束停止。它不是送达或执行回执，也不覆盖其他窗口、任务或后台触发器。");
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    session.is_some() && !self.recorder.active() && self.pending.is_none(),
                    egui::Button::new("开始新录制（替换草稿）"),
                )
                .clicked()
                && let Some(session) = session
            {
                match ScriptRecorder::start(session) {
                    Ok(recorder) => {
                        self.recorder = recorder;
                        self.snapshot();
                    }
                    Err(error) => self.status = error.to_string(),
                }
            }
            if ui
                .add_enabled(self.recorder.active(), egui::Button::new("停止"))
                .clicked()
            {
                self.recorder.stop();
                self.snapshot();
            }
            if ui
                .add_enabled(
                    !self.recorder.active() && self.pending.is_none(),
                    egui::Button::new("空白录制草稿"),
                )
                .clicked()
            {
                self.recorder = ScriptRecorder::default();
                self.snapshot();
            }
        });
        ui.add_enabled_ui(self.pending.is_none(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("参数名（不填真实内容）");
                ui.text_edit_singleline(&mut self.parameter);
                ui.checkbox(&mut self.append_newline, "追加换行");
                if ui.button("追加参数化发送").clicked() {
                    self.append(ScriptRecordingStep::SendParameter {
                        parameter: self.parameter.clone(),
                        append_newline: self.append_newline,
                    });
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::DragValue::new(&mut self.wait_ms)
                        .range(100..=30_000)
                        .suffix(" ms"),
                );
                if ui.button("追加字面等待参数").clicked() {
                    self.append(ScriptRecordingStep::WaitForParameter {
                        parameter: self.parameter.clone(),
                        timeout_ms: self.wait_ms,
                    });
                }
                ui.add(
                    egui::DragValue::new(&mut self.sleep_ms)
                        .range(1..=30_000)
                        .suffix(" ms"),
                );
                if ui.button("追加暂停").clicked() {
                    self.append(ScriptRecordingStep::Sleep {
                        milliseconds: self.sleep_ms,
                    });
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("公开通知");
                ui.add(egui::TextEdit::singleline(&mut self.notification).char_limit(128));
                if ui.button("追加通知").clicked() {
                    self.append(ScriptRecordingStep::Notify {
                        message: self.notification.clone(),
                    });
                }
            });
        });
        ui.collapsing(
            "录制 JSON：停止后编辑每个 omitted / truncated 标记",
            |ui| {
                let changed = ui
                    .add_enabled(
                        !self.recorder.active() && self.pending.is_none(),
                        egui::TextEdit::multiline(&mut self.recording_json)
                            .code_editor()
                            .desired_rows(10)
                            .desired_width(f32::INFINITY)
                            .char_limit(MAX_SCRIPT_DOCUMENT_BYTES),
                    )
                    .changed();
                if changed {
                    self.invalidate();
                }
            },
        );
        if ui
            .add_enabled(
                !self.recorder.active() && self.pending.is_none(),
                egui::Button::new("从核对后的录制生成脚本文档"),
            )
            .clicked()
        {
            match decode_script_recording(self.recording_json.as_bytes())
                .and_then(|r| render_script_recording(&r))
            {
                Ok(document) => {
                    self.script_json = serde_json::to_string_pretty(&document).unwrap_or_default();
                    self.selected.clear();
                    self.invalidate();
                    self.status = "已生成脚本草稿，尚未授权或执行".into();
                }
                Err(error) => self.status = error.to_string(),
            }
        }
        ui.separator();
        ui.label("脚本 JSON（source / requested / parameters / limits）");
        if ui
            .add_enabled(
                self.pending.is_none(),
                egui::TextEdit::multiline(&mut self.script_json)
                    .code_editor()
                    .desired_rows(10)
                    .desired_width(f32::INFINITY)
                    .char_limit(MAX_SCRIPT_DOCUMENT_BYTES),
            )
            .changed()
        {
            self.selected.clear();
            self.invalidate();
        }
        if let Ok(document) = decode_script_document(self.script_json.as_bytes()) {
            ui.label("仅预览能力选择；默认全不选，勾选不会授予运行权限。");
            for capability in &document.requested {
                let mut selected = self.selected.contains(capability);
                if ui.checkbox(&mut selected, capability.label()).changed() {
                    if selected {
                        self.selected.insert(*capability);
                    } else {
                        self.selected.remove(capability);
                    }
                    self.invalidate();
                }
            }
            if ui.button("核对权限声明与完整文档摘要").clicked() {
                match plan_script_permissions(&document, &self.selected) {
                    Ok(plan) => {
                        self.plan = Some(plan);
                        self.status = "离线权限计划已生成；不是执行令牌".into();
                    }
                    Err(error) => self.status = error.to_string(),
                }
            }
        }
        if let Some(plan) = &self.plan {
            ui.label(format!(
                "文档 SHA-256: {}",
                plan.document_sha256
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ));
            ui.label(format!(
                "选择 {} 项；缺少 {} 项；参数 {} 个；总期限 {} ms；操作预算 {}",
                plan.selected.len(),
                plan.missing.len(),
                plan.parameters.len(),
                plan.limits.timeout_ms,
                plan.limits.max_operations
            ));
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("文件");
            if ui.text_edit_singleline(&mut self.path).changed() {
                self.invalidate();
            }
        });
        ui.add_enabled_ui(self.pending.is_none(), |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("读取脚本文档").clicked() {
                    self.file(Kind::Script, false);
                }
                if ui.button("导出脚本至新文件").clicked() {
                    self.file(Kind::Script, true);
                }
                if ui
                    .add_enabled(!self.recorder.active(), egui::Button::new("读取录制文档"))
                    .clicked()
                {
                    self.file(Kind::Recording, false);
                }
                if ui.button("导出录制至新文件").clicked() {
                    self.file(Kind::Recording, true);
                }
            });
        });
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
    }
    pub fn draw(&mut self, ui: &mut egui::Ui, session: Option<SessionId>) {
        ui.add_enabled_ui(self.active_run.is_none() && !self.live.active(), |ui| {
            self.draw_authoring(ui, session)
        });
        ui.separator();
        ui.heading("离线脚本运行");
        ui.label("独立 QuickJS 进程，仅参数、暂停及通知计数；没有实时终端/任务/文件/网络 API。Windows接低权限容器，Linux接系统调用白名单，macOS接默认拒绝访问策略。当前仅运行自行核对的本机草稿，效果统一后续验证。");
        ui.add_enabled_ui(self.active_run.is_none() && !self.live.active(), |ui| {
            ui.label(
                "临时参数 JSON（遮盖显示；不导出，启动后清空）：{\"values\":{\"参数名\":\"值\"}}",
            );
            if ui
                .add(
                    egui::TextEdit::singleline(&mut *self.parameter_values_json)
                        .password(true)
                        .char_limit(MAX_SCRIPT_DOCUMENT_BYTES)
                        .desired_width(f32::INFINITY),
                )
                .changed()
            {
                self.offline_confirmed = false;
                self.live.invalidate_confirmation();
            }
            ui.checkbox(
                &mut self.offline_confirmed,
                "我已核对完整草稿、能力选择及预算，确认本次离线运行",
            );
            if ui
                .add_enabled(
                    self.offline_confirmed
                        && self.plan.is_some()
                        && self.pending.is_none()
                        && !self.recorder.active()
                        && !self.run_unknown,
                    egui::Button::new("运行离线草稿"),
                )
                .clicked()
            {
                self.start_offline();
            }
        });
        if let Some(run) = &self.active_run {
            ui.label("后台运行中；取消后继续等待退出回执。");
            if ui.button("取消运行").clicked() {
                run.cancel.cancel();
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(report) = &self.run_report {
            ui.label(format!(
                "运行 {}：{:?}；操作 {}；通知计数 {}；退出已确认 {}",
                report.run_id,
                report.status,
                report.operations,
                report.notifications,
                report.exit_confirmed
            ));
            ui.label(format!(
                "运行时文档 SHA-256: {}",
                report
                    .document_sha256
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ));
            ui.label(if report.containment.is_some() {
                "worker已报告进程资源限制就绪"
            } else {
                "尚未收到进程资源限制就绪回执"
            });
            ui.label(match report.sandbox {
                Some(WorkerSandbox::WindowsLpacV1) => {
                    "worker已报告Windows低权限容器就绪（效果待统一验证）"
                }
                Some(WorkerSandbox::LinuxSeccompV1) => {
                    "worker已报告Linux系统调用白名单就绪（效果待统一验证）"
                }
                Some(WorkerSandbox::MacosSeatbeltV1) => {
                    "worker已报告macOS默认拒绝访问策略就绪（效果待统一验证）"
                }
                None => "尚未收到访问策略就绪回执",
            });
            ui.label(if report.sandbox_cleanup_confirmed {
                "隔离资源清理已确认"
            } else {
                "隔离资源清理尚未确认"
            });
        }
        if self.run_unknown {
            ui.label("进程退出或隔离资源清理未确认，本面板停止新运行；未自动重试。");
        }
        ui.add_enabled_ui(self.active_run.is_none(), |ui| {
            self.live.draw(
                ui,
                &self.script_json,
                &self.selected,
                session,
                &mut self.parameter_values_json,
                self.run_unknown,
            );
        });
        if self.live.take_recovery_confirmation() {
            self.run_unknown = false;
            self.run_report = None;
        }
    }
    pub fn poll_live(
        &mut self,
        config: Option<&crate::daemon_connection::DesktopConnectionConfig>,
    ) -> bool {
        self.live.poll(config)
    }
    fn start_offline(&mut self) {
        if self.active_run.is_some() || self.run_unknown || !self.offline_confirmed {
            return;
        }
        self.offline_confirmed = false;
        let request = (|| {
            let document = decode_script_document(self.script_json.as_bytes())
                .map_err(|_| ScriptWorkerError::Invalid)?;
            let plan = self.plan.clone().ok_or(ScriptWorkerError::Invalid)?;
            let parameters =
                serde_json::from_str::<ScriptParameterValues>(&self.parameter_values_json)
                    .map_err(|_| ScriptWorkerError::Parameters)?;
            OfflineScriptRun::review(document, plan, parameters)
        })();
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        self.parameter_values_json.zeroize();
        self.parameter_values_json.push_str("{\"values\":{}}");
        let cancel = ScriptCancellation::default();
        let thread_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("offline-script-supervisor".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| ScriptWorkerError::Unavailable)
                    .and_then(|runtime| {
                        let result = runtime.block_on(run_offline(request, thread_cancel));
                        // Unknown IO/reapers must not hide the returned report
                        // behind an unbounded runtime shutdown. Their leases
                        // retain quota if shutdown drops them without proof.
                        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
                        result
                    });
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.active_run = Some(ActiveRun { cancel, receiver });
                self.run_report = None;
                self.status = "正在启动离线 worker".into();
            }
            Err(_) => self.status = "无法启动离线运行线程".into(),
        }
    }
}
fn file_job(path: &str, kind: Kind, body: Option<String>) -> FileReply {
    if let Some(body) = body {
        let body = Zeroizing::new(body);
        let path = std::path::Path::new(path);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| "无法暂存脚本文档")?;
        file.write_all(body.as_bytes())
            .map_err(|_| "脚本文档写入失败")?;
        file.as_file().sync_all().map_err(|_| "脚本文档同步失败")?;
        file.persist_noclobber(path)
            .map_err(|_| "文件已存在或文档发布失败")?;
        return Ok(FileResult { kind, body: None });
    }
    let file = std::fs::File::open(path).map_err(|_| "无法打开脚本文档")?;
    if !file.metadata().map_err(|_| "无法检查脚本文档")?.is_file() {
        return Err("输入必须是普通文档文件".into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((MAX_SCRIPT_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取脚本文档")?;
    let body = match kind {
        Kind::Script => serde_json::to_string_pretty(
            &decode_script_document(&bytes).map_err(|e| e.to_string())?,
        ),
        Kind::Recording => serde_json::to_string_pretty(
            &decode_script_recording(&bytes).map_err(|e| e.to_string())?,
        ),
    }
    .map_err(|_| "无法编码脚本文档")?;
    if body.len() > MAX_SCRIPT_DOCUMENT_BYTES {
        return Err("文档大小超限".into());
    }
    Ok(FileResult {
        kind,
        body: Some(body),
    })
}
