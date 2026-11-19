//! A single in-memory snapshot, with explicitly opted-in refresh scheduling.
mod view;
use crate::{daemon_connection::DesktopConnectionConfig, terminal_tools_connection::query};
use cshell_application::TerminalToolsReply;
use cshell_domain::{ServerResources, SessionId};
use cshell_ipc::{TerminalToolsOperation, TerminalToolsRequest};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

struct Worker {
    result: mpsc::Receiver<Result<ServerResources, String>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}
impl Worker {
    fn start(config: DesktopConnectionConfig, session: SessionId) -> Result<Self, String> {
        let (send, result) = mpsc::sync_channel(1);
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        std::thread::Builder::new().name("cshell-resource-sample".into()).spawn(move || {
            let work = || -> Result<_, String> {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
                    .map_err(|_| "资源采集线程无法启动".to_owned())?;
                runtime.block_on(async {
                    let request = TerminalToolsRequest {
                        operation: TerminalToolsOperation::ServerResources as i32,
                        session_id: session.as_uuid().as_bytes().to_vec(),
                        ..Default::default()
                    };
                    tokio::select! {
                        biased;
                        _ = cancelled => Err("资源采集已取消".into()),
                        result = tokio::time::timeout(Duration::from_secs(15), query(&config, &request)) => {
                            match result.map_err(|_| "资源采集超时".to_owned())?? {
                                TerminalToolsReply::Resources(sample) if sample.validate(session) => Ok(sample),
                                _ => Err("服务器资源回复范围或格式无效".into()),
                            }
                        }
                    }
                })
            };
            let _ = send.send(work());
        }).map_err(|_| "资源采集线程无法创建".to_owned())?;
        Ok(Self {
            result,
            cancel: Some(cancel),
        })
    }
}

#[derive(Default)]
pub(super) struct Panel {
    pub open: bool,
    session: Option<SessionId>,
    transport_generation: Option<u64>,
    sample: Option<ServerResources>,
    worker: Option<Worker>,
    error: Option<String>,
    initial: bool,
    visible: bool,
    refresh_seconds: u32,
    last_interval: u32,
    next_due: Option<Instant>,
}
impl Panel {
    pub fn show(&mut self) {
        if !self.open {
            *self = Self {
                open: true,
                initial: true,
                ..Default::default()
            };
        }
    }
    pub fn suspend(&mut self) {
        self.visible = false;
        self.next_due = None;
    }
    pub fn bind_transport(&mut self, session: Option<SessionId>, generation: Option<u64>) {
        if (self.session, self.transport_generation) != (session, generation) {
            self.worker = None;
            self.sample = None;
            self.error = None;
            self.session = session;
            self.transport_generation = generation;
            self.initial = session.is_some();
            self.interval(0, Instant::now());
        }
    }
    fn interval(&mut self, seconds: u32, now: Instant) {
        self.refresh_seconds = if seconds == 0 {
            0
        } else {
            seconds.clamp(5, 3600)
        };
        if self.refresh_seconds > 0 {
            self.last_interval = self.refresh_seconds;
        }
        self.next_due = (self.refresh_seconds > 0)
            .then(|| now + Duration::from_secs(u64::from(self.refresh_seconds)));
    }
    fn due(&self, now: Instant) -> bool {
        self.open
            && self.visible
            && self.worker.is_none()
            && (self.initial || self.next_due.is_some_and(|due| now >= due))
    }
    fn receive(&mut self, now: Instant) {
        let Some(worker) = &self.worker else {
            return;
        };
        let result = match worker.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("资源采集线程已结束，请手动刷新".into()),
        };
        self.worker = None;
        match result {
            Ok(sample)
                if Some(sample.session) == self.session
                    && self
                        .transport_generation
                        .is_none_or(|expected| expected == sample.transport_generation) =>
            {
                self.sample = Some(sample);
                self.error = None;
                self.interval(self.refresh_seconds, now);
            }
            Ok(_) => {
                self.error = Some("SSH 会话已变化，已丢弃旧采集结果".into());
                self.interval(0, now);
            }
            Err(error) => {
                self.error = Some(error);
                self.interval(0, now);
            }
        }
    }
    /// Returns true when closing the window should return keyboard focus.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        config: Option<&DesktopConnectionConfig>,
        session: Option<SessionId>,
        target: Option<&str>,
        visible: bool,
    ) -> bool {
        if !self.open || !visible {
            self.suspend();
            return false;
        }
        let now = Instant::now();
        if self.session != session {
            self.bind_transport(session, None);
        }
        if !self.visible {
            self.visible = true;
            self.interval(self.refresh_seconds, now);
        }
        if session.is_none() {
            self.initial = false;
            self.interval(0, now);
        }
        self.receive(now);
        let mut open = true;
        let mut manual = false;
        let mut close = false;
        let width = (ctx.content_rect().width() - 40.0).max(240.0);
        let shown = egui::Window::new(egui::RichText::new("服务器资源").size(18.0).strong())
            .id(egui::Id::new("server-resources-panel"))
            .open(&mut open)
            .default_width(660.0_f32.min(width))
            .default_height((ctx.content_rect().height() - 120.0).clamp(160.0, 680.0))
            .min_width(280.0_f32.min(width))
            .max_width(width)
            .max_height((ctx.content_rect().height() - 100.0).max(160.0))
            .frame(
                egui::Frame::window(&ctx.style_of(egui::Theme::Dark))
                    .fill(cshell_ui::theme::PANEL)
                    .inner_margin(16.0),
            )
            .show(ctx, |ui| {
                view::reading_style(ui);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(target.unwrap_or("当前 SSH 会话"))
                            .size(18.0)
                            .strong(),
                    )
                    .truncate(),
                )
                .on_hover_text(format!(
                    "Linux · 实际采集目标\n{}\n在 Shell 内手动 ssh 不会切换采集目标。",
                    target.unwrap_or("当前 SSH 会话")
                ));
                if session.is_some() {
                    ui.weak("采集此 CShell 会话的目标；在 Shell 内手动 ssh 不切换目标。");
                }
                egui::Frame::new()
                    .fill(cshell_ui::theme::RAISED)
                    .inner_margin(12.0)
                    .corner_radius(5.0)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            manual = ui
                                .add_enabled(
                                    session.is_some() && self.worker.is_none(),
                                    egui::Button::new("刷新一次"),
                                )
                                .clicked();
                            let mut automatic = self.refresh_seconds > 0;
                            if ui
                                .add_enabled(
                                    session.is_some(),
                                    egui::Checkbox::new(&mut automatic, "自动刷新"),
                                )
                                .changed()
                            {
                                self.interval(
                                    if automatic {
                                        if self.last_interval == 0 {
                                            10
                                        } else {
                                            self.last_interval
                                        }
                                    } else {
                                        0
                                    },
                                    now,
                                );
                            }
                            ui.label("间隔（秒）");
                            let mut seconds = self.refresh_seconds;
                            if ui
                                .add_enabled(
                                    session.is_some(),
                                    egui::DragValue::new(&mut seconds).range(0..=3600),
                                )
                                .changed()
                            {
                                self.interval(seconds, now);
                            }
                        });
                        ui.weak(if self.refresh_seconds == 0 {
                            "0 秒：不自动刷新；开启后最短 5 秒。"
                        } else {
                            "自动刷新已开启 · 隐藏面板或最小化窗口时暂停。"
                        });
                    });
                let body_height = (ui.available_height() - 64.0).max(48.0);
                egui::ScrollArea::vertical()
                    .id_salt("server-resource-metrics")
                    .min_scrolled_height(32.0)
                    .max_height(body_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if session.is_none() {
                            ui.label(egui::RichText::new("尚未连接 SSH").size(20.0).strong());
                            ui.label("切换到已连接的 SSH 标签后即可采集服务器资源。");
                        }
                        if self.worker.is_some() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(if self.sample.is_some() {
                                    "正在刷新，以下为上次采集结果…"
                                } else {
                                    "正在采集，约需一秒…"
                                });
                            });
                        }
                        if let Some(error) = &self.error {
                            ui.colored_label(
                                cshell_ui::theme::WARNING,
                                egui::RichText::new("采集未完成").strong(),
                            );
                            ui.label(error);
                            if self.sample.is_some() {
                                ui.colored_label(
                                    cshell_ui::theme::WARNING,
                                    "以下为上次成功快照，尚未更新。",
                                );
                            }
                        }
                        if let Some(sample) = &self.sample {
                            view::metrics(ui, sample);
                        }
                    });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if let Some(sample) = &self.sample {
                        let seconds = sample.sampled_unix_ms / 1000;
                        ui.weak(format!(
                            "UTC {:02}:{:02}:{:02} · {}",
                            seconds / 3600 % 24,
                            seconds / 60 % 60,
                            seconds % 60,
                            if self.error.is_some() || self.worker.is_some() {
                                "上次快照"
                            } else {
                                "本次快照"
                            }
                        ))
                        .on_hover_text("采集完成时间；仅保留本次快照，不保存监控历史。");
                    } else {
                        ui.weak("仅本次快照，不保存监控历史。");
                    }
                    if ui.button("关闭").clicked() {
                        close = true;
                    }
                });
            });
        if !open || close {
            self.open = false;
            self.initial = false;
            self.worker = None;
            self.interval(0, now);
            self.suspend();
            return true;
        }
        if !shown.is_some_and(|response| response.inner.is_some()) {
            self.suspend();
            return false;
        }
        if manual {
            self.initial = true;
        }
        if self.due(now)
            && let (Some(config), Some(session)) = (config, session)
        {
            self.initial = false;
            self.next_due = None;
            self.error = None;
            match Worker::start(config.clone(), session) {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    self.error = Some(error);
                    self.interval(0, now);
                }
            }
        }
        if config.is_none() && session.is_some() {
            self.error = Some("后台连接不可用，请恢复连接后刷新".into());
        }
        if self.worker.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(due) = self.next_due {
            ctx.request_repaint_after(due.saturating_duration_since(now));
        }
        false
    }
}

fn memory(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.2} GiB", kib as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} MiB", kib as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests;
