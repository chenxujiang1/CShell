//! Non-modal terminal uploads. Every request retains the source session identity.
use crate::{daemon_connection::DesktopConnectionConfig, zmodem_settings};
use cshell_application::{
    TerminalUploadProtocol as Protocol, TerminalUploadSnapshot as Snapshot,
    TerminalUploadStart as Start, TerminalUploadState as State,
};
use cshell_domain::SessionId;
use cshell_ipc::{SftpOperation as Op, SftpRequest, SftpStatus};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Row {
    source: SessionId,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    finished_at: Option<Instant>,
    auto_close: bool,
    pending: bool,
}
#[derive(Default)]
struct View {
    generation: u64,
    rows: BTreeMap<SessionId, Row>,
}
struct Connection {
    shared: Arc<Mutex<View>>,
    sender: tokio::sync::mpsc::Sender<SftpRequest>,
    stop: tokio::sync::watch::Sender<bool>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
impl Connection {
    fn start(config: DesktopConnectionConfig) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "无法启动上传进度运行环境")?;
        let shared = Arc::new(Mutex::new(View::default()));
        let work = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<SftpRequest>(16);
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        std::thread::Builder::new().name("cshell-terminal-upload".into()).spawn(move||{
            runtime.block_on(async move {
                let mut interval=tokio::time::interval(Duration::from_millis(250));interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let commands=tokio::select! {
                        _=stopped.changed()=>break,
                        request=receiver.recv()=>match request{Some(r)=>vec![r],None=>break},
                        _=interval.tick()=>{let view=work.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.rows.iter().filter(|(_,r)|r.snapshot.as_ref().is_none_or(|s|!s.state.finished()) && !(r.snapshot.is_none() && r.error.is_some())).map(|(id,row)|query(Op::TerminalUploadGet,row.source,*id)).collect::<Vec<_>>()},
                    };
                    for request in commands {
                        let Ok(id)=<[u8;16]>::try_from(request.transfer_id.as_slice()).map(SessionId::from_bytes) else{continue;};
                        let result=tokio::select!{_=stopped.changed()=>return,result=crate::sftp_connection::send(&config,request)=>result};
                        let parsed=result.and_then(|reply|{if reply.status!=SftpStatus::Ok as i32 {return Err(reply.detail);}
                            let snapshot:Snapshot=serde_json::from_slice(&reply.document_json).map_err(|_|"上传进度格式无效")?;if snapshot.id!=id{return Err("上传任务回执不匹配".into());}Ok(snapshot)});
                        let mut view=work.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(row)=view.rows.get_mut(&id){row.pending=false;match parsed{Ok(snapshot) if snapshot.source==row.source=>{if snapshot.state.finished(){row.finished_at.get_or_insert_with(Instant::now);}row.error=None;row.snapshot=Some(snapshot);},Ok(_)=>row.error=Some("上传源会话回执不匹配".into()),Err(error)=>{row.error=Some(error);}}view.generation=view.generation.wrapping_add(1);}
                    }
                }
            });
        }).map_err(|_|"无法启动上传进度服务")?;
        Ok(Self {
            shared,
            sender,
            stop,
        })
    }
    fn request(&self, request: SftpRequest) -> bool {
        self.sender.try_send(request).is_ok()
    }
}
fn query(operation: Op, source: SessionId, id: SessionId) -> SftpRequest {
    SftpRequest {
        operation: operation as i32,
        session_id: source.as_uuid().as_bytes().to_vec(),
        transfer_id: id.as_uuid().as_bytes().to_vec(),
        ..Default::default()
    }
}
#[derive(Default)]
pub(crate) struct Panel {
    noticed: std::collections::BTreeSet<(SessionId, u8)>,
    connection: Option<Connection>,
    generation: u64,
    pub open: bool,
    error: Option<String>,
}
impl Panel {
    pub fn poll(&mut self) -> bool {
        let generation = self.connection.as_ref().map_or(0, |c| {
            c.shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generation
        });
        let changed = generation != self.generation;
        self.generation = generation;
        if changed && let Some(connection) = &self.connection {
            let view = connection
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, row) in &view.rows {
                let attention = if row.error.is_some()
                    || row
                        .snapshot
                        .as_ref()
                        .is_some_and(|s| s.state == State::Failed)
                {
                    Some(1)
                } else if row
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.state == State::Ready)
                {
                    Some(2)
                } else {
                    None
                };
                if let Some(kind) = attention
                    && self.noticed.insert((*id, kind))
                {
                    self.open = true;
                }
            }
        }
        changed
    }
    fn upload(
        &mut self,
        config: &DesktopConnectionConfig,
        source: SessionId,
        paths: Vec<String>,
        settings: zmodem_settings::Settings,
        verification: cshell_domain::SftpVerification,
    ) {
        if self.connection.is_none() {
            match Connection::start(config.clone()) {
                Ok(c) => self.connection = Some(c),
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            }
        }
        let Some(connection) = &self.connection else {
            return;
        };
        let id = SessionId::new();
        let start = Start {
            paths,
            protocol: settings.terminal_upload_protocol,
            verification,
            confirm_before_start: settings.confirm_terminal_upload,
        };
        let mut request = query(Op::TerminalUploadStart, source, id);
        request.confirmed = true;
        request.document_json = match serde_json::to_vec(&start) {
            Ok(v) => v,
            Err(_) => {
                self.error = Some("无法提交拖放内容".into());
                return;
            }
        };
        {
            let mut view = connection
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if view
                .rows
                .values()
                .filter(|r| {
                    r.snapshot.as_ref().is_none_or(|s| !s.state.finished())
                        && !(r.snapshot.is_none() && r.error.is_some())
                })
                .count()
                >= 4
            {
                self.error = Some("同时上传的批次已满，请等待现有传输结束".into());
                return;
            }
            if view.rows.len() >= 32 {
                view.rows
                    .retain(|_, r| r.snapshot.as_ref().is_none_or(|s| !s.state.finished()));
            }
            view.rows.insert(
                id,
                Row {
                    source,
                    snapshot: None,
                    error: None,
                    finished_at: None,
                    auto_close: settings.close_successful_upload,
                    pending: true,
                },
            );
        }
        if !connection.request(request) {
            connection
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rows
                .remove(&id);
            self.error = Some("上传请求队列繁忙，尚未提交，请重新拖入".into());
        }
        self.open = true;
    }
    pub fn drop_target(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        source: Option<SessionId>,
        config: Option<&DesktopConnectionConfig>,
        blocked: bool,
        verification: Option<cshell_domain::SftpVerification>,
    ) {
        let context = ui.ctx().clone();
        let over = context
            .pointer_latest_pos()
            .is_some_and(|p| rect.contains(p) && context.layer_id_at(p) == Some(ui.layer_id()));
        let hovering = context.input(|i| !i.raw.hovered_files.is_empty());
        if hovering && over {
            let allowed = source.is_some() && config.is_some() && !blocked;
            let color = if allowed {
                egui::Color32::from_rgb(95, 185, 220)
            } else {
                cshell_ui::theme::WARNING
            };
            ui.painter().rect_filled(
                rect.shrink(3.0),
                6.0,
                egui::Color32::from_rgba_unmultiplied(20, 35, 46, 225),
            );
            ui.painter().rect_stroke(
                rect.shrink(3.0),
                6.0,
                egui::Stroke::new(2.0, color),
                egui::StrokeKind::Inside,
            );
            let protocol = match zmodem_settings::load().terminal_upload_protocol {
                Protocol::Sftp => "SFTP",
                Protocol::Zmodem => "ZMODEM · 文件夹使用 SFTP",
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if allowed {
                    format!("释放即上传到此 SSH 的当前目录\n{protocol} · 同名文件不覆盖")
                } else {
                    "请先连接 SSH，并关闭正在操作的对话框".into()
                },
                egui::FontId::proportional(18.0),
                color,
            );
        }
        let dropped = context.input(|i| i.raw.dropped_files.clone());
        if over && !dropped.is_empty() {
            if blocked || source.is_none() || config.is_none() {
                self.error = Some("当前不是可上传的 SSH 终端，文件未发送".into());
                self.open = true;
                return;
            }
            if dropped.len() > 128 {
                self.error = Some("一次最多拖入 128 个文件或文件夹".into());
                self.open = true;
                return;
            }
            let paths = dropped
                .iter()
                .map(|f| {
                    f.path()
                        .to_str()
                        .map(str::to_owned)
                        .ok_or("文件路径不是有效 Unicode")
                })
                .collect::<Result<Vec<_>, _>>();
            match paths {
                Ok(paths) => {
                    let settings = zmodem_settings::load();
                    let mode = verification.unwrap_or(settings.sftp_verification);
                    self.error = None;
                    if let (Some(config), Some(source)) = (config, source) {
                        self.upload(config, source, paths, settings, mode);
                    }
                }
                Err(e) => {
                    self.error = Some(e.into());
                    self.open = true;
                }
            }
        }
    }
    pub fn draw(&mut self, ctx: &egui::Context) {
        let rows = self
            .connection
            .as_ref()
            .map(|c| {
                c.shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .rows
                    .clone()
            })
            .unwrap_or_default();
        let visible: Vec<_> = rows
            .iter()
            .filter(|(_, r)| {
                !(r.auto_close
                    && r.error.is_none()
                    && r.snapshot
                        .as_ref()
                        .is_some_and(|s| s.state == State::Succeeded)
                    && r.finished_at
                        .is_some_and(|at| at.elapsed() > Duration::from_millis(1500)))
            })
            .collect();
        if !rows.is_empty() && visible.is_empty() && self.error.is_none() {
            self.open = false;
        }
        let mut open = self.open;
        egui::Window::new("上传任务")
            .open(&mut open)
            .default_width(460.0)
            .max_width((ctx.content_rect().width() - 32.0).max(200.0))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -24.0])
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(error) = &self.error {
                    ui.colored_label(cshell_ui::theme::WARNING, error);
                }
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() * 0.55).max(160.0))
                    .show(ui, |ui| {
                        for (id, row) in visible {
                            ui.push_id(id, |ui| {
                                if let Some(s) = &row.snapshot {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.strong(format!("↑ {}", s.host));
                                        ui.weak(match s.protocol {
                                            Protocol::Sftp => "SFTP",
                                            Protocol::Zmodem => "ZMODEM",
                                        });
                                    });
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&s.directory).monospace(),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&s.directory);
                                    let fraction = if s.state == State::Succeeded {
                                        1.0
                                    } else if s.total_bytes > 0 {
                                        (s.bytes as f64 / s.total_bytes as f64).clamp(0.0, 1.0)
                                            as f32
                                    } else {
                                        0.0
                                    };
                                    ui.add(
                                        egui::ProgressBar::new(fraction)
                                            .show_percentage()
                                            .desired_width(ui.available_width()),
                                    );
                                    let remaining = s.total_bytes.saturating_sub(s.bytes);
                                    let eta = if s.bytes_per_second > 0 {
                                        format!(" · 剩余约 {} 秒", remaining / s.bytes_per_second)
                                    } else {
                                        String::new()
                                    };
                                    ui.label(format!(
                                        "{}/{} 个文件 · {} / {} · {}/s{}",
                                        s.completed,
                                        s.total_files,
                                        size(s.bytes),
                                        size(s.total_bytes),
                                        size(s.bytes_per_second),
                                        eta
                                    ));
                                    if !s.current_name.is_empty() {
                                        ui.add(egui::Label::new(&s.current_name).truncate());
                                    }
                                    ui.colored_label(
                                        if s.state == State::Failed {
                                            cshell_ui::theme::WARNING
                                        } else {
                                            cshell_ui::theme::TEXT
                                        },
                                        &s.detail,
                                    );
                                    ui.horizontal_wrapped(|ui| {
                                        if s.state == State::Ready
                                            && ui.button("确认上传到此目录").clicked()
                                        {
                                            self.control(
                                                Op::TerminalUploadContinue,
                                                row.source,
                                                *id,
                                            );
                                        }
                                        if s.protocol == Protocol::Sftp
                                            && matches!(s.state, State::Running | State::Paused)
                                            && ui
                                                .button(if s.state == State::Paused {
                                                    "继续"
                                                } else {
                                                    "暂停"
                                                })
                                                .clicked()
                                        {
                                            self.control(
                                                if s.state == State::Paused {
                                                    Op::TerminalUploadContinue
                                                } else {
                                                    Op::TerminalUploadPause
                                                },
                                                row.source,
                                                *id,
                                            );
                                        }
                                        if !s.state.finished() && ui.button("取消上传").clicked()
                                        {
                                            self.control(Op::TerminalUploadCancel, row.source, *id);
                                        }
                                    });
                                } else {
                                    ui.label("正在确认目标主机和当前目录…");
                                    if ui.button("取消上传").clicked() {
                                        self.control(Op::TerminalUploadCancel, row.source, *id);
                                    }
                                }
                                if row.pending {
                                    ui.weak("请求处理中…");
                                }
                                if let Some(error) = &row.error {
                                    ui.colored_label(cshell_ui::theme::WARNING, error);
                                    if ui.button("刷新任务状态").clicked() {
                                        self.control(Op::TerminalUploadGet, row.source, *id);
                                    }
                                    ui.weak("通信结果不确定时不会重复提交，请核对远端文件。");
                                }
                                ui.separator();
                            });
                        }
                    });
                ui.weak("关闭进度不取消上传；自动收起可在全局设置中调整。");
            });
        self.open = open;
        if rows.values().any(|r| {
            r.snapshot.as_ref().is_none_or(|s| !s.state.finished())
                || r.auto_close
                    && r.finished_at
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
        }) {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }
    fn control(&mut self, op: Op, source: SessionId, id: SessionId) {
        if !self
            .connection
            .as_ref()
            .is_some_and(|c| c.request(query(op, source, id)))
        {
            self.error = Some("上传控制请求队列繁忙，请稍后重试".into());
        }
    }
}
fn size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests;
