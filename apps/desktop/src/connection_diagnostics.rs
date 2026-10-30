//! Live attempt-local diagnostics; no disk history and no authentication material.
use cshell_domain::{ConnectionDiagnostic, ConnectionOutcome as Outcome, ConnectionStage as Stage};
use cshell_ipc::{Envelope, IpcError, envelope, read_envelope};
use std::{future::Future, sync::Arc, time::Instant};
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) fn preview(outcome: Outcome) -> View {
    tests::sample(outcome)
}
#[derive(Clone, Debug)]
pub(crate) struct View {
    pub diagnostic: Arc<ConnectionDiagnostic>,
    pub received: Instant,
}
#[derive(Clone)]
struct Observer {
    enabled: bool,
    publish: Arc<dyn Fn(View) + Send + Sync>,
}
tokio::task_local! {static CURRENT:Observer;}
pub(crate) async fn observe<T>(
    enabled: bool,
    publish: impl Fn(View) + Send + Sync + 'static,
    work: impl Future<Output = T>,
) -> T {
    CURRENT
        .scope(
            Observer {
                enabled,
                publish: Arc::new(publish),
            },
            work,
        )
        .await
}
pub(crate) fn requested() -> bool {
    CURRENT.try_with(|o| o.enabled).unwrap_or(false)
}
pub(crate) async fn read_response<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    id: u64,
) -> Result<Envelope, IpcError> {
    let mut attempt = None;
    let mut last_elapsed = 0;
    let mut finished = false;
    for _ in 0..4096 {
        let reply = read_envelope(stream).await?;
        if reply.request_id != id {
            return Err(invalid());
        }
        let Some(envelope::Payload::ConnectionDiagnosticUpdate(update)) = &reply.payload else {
            return Ok(reply);
        };
        if !requested() || update.document_json.len() > 96 * 1024 || finished {
            return Err(invalid());
        }
        let value: ConnectionDiagnostic =
            serde_json::from_slice(&update.document_json).map_err(|_| invalid())?;
        if !value.validate()
            || attempt.is_some_and(|id| id != value.attempt)
            || value.elapsed_ms < last_elapsed
        {
            return Err(invalid());
        }
        attempt = Some(value.attempt);
        last_elapsed = value.elapsed_ms;
        finished = value.outcome != Outcome::Running;
        let _ = CURRENT.try_with(|o| {
            (o.publish)(View {
                diagnostic: Arc::new(value),
                received: Instant::now(),
            })
        });
    }
    Err(invalid())
}
fn invalid() -> IpcError {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "连接诊断回复身份或格式无效",
    )
    .into()
}
pub(crate) fn uncertain(view: &mut Option<View>) {
    if let Some(view) = view
        && matches!(
            view.diagnostic.outcome,
            Outcome::Running | Outcome::Succeeded
        )
    {
        let extra = view.received.elapsed().as_millis().min(86_400_000) as u64;
        let value = Arc::make_mut(&mut view.diagnostic);
        value.elapsed_ms = value.elapsed_ms.saturating_add(extra).min(86_400_000);
        value.outcome = Outcome::Uncertain;
        if let Some(last) = value.steps.last_mut() {
            last.elapsed_ms = value.elapsed_ms.saturating_sub(last.started_ms);
            last.outcome = Outcome::Uncertain;
        }
        view.received = Instant::now();
    }
}
pub(crate) fn label(stage: Stage) -> &'static str {
    match stage {
        Stage::Preparing => "准备连接",
        Stage::Credentials => "准备认证凭据",
        Stage::Resolving => "解析主机名",
        Stage::Tcp => "建立 TCP 连接",
        Stage::Proxy => "建立代理隧道",
        Stage::JumpChannel => "打开跳板通道",
        Stage::Handshake => "SSH 握手",
        Stage::HostKey => "验证主机密钥 / 完成握手",
        Stage::Authentication => "SSH 认证",
        Stage::WaitingForInput => "等待输入 / 解锁确认",
        Stage::TerminalChannel => "打开终端通道",
        Stage::Pty => "申请 PTY",
        Stage::Shell => "启动 Shell",
        Stage::SftpChannel => "打开 SFTP 通道",
        Stage::SftpSubsystem => "启动 SFTP 子系统",
        Stage::InitialDirectory => "读取初始目录",
        Stage::ReusingTransport => "复用已认证的 SSH",
        Stage::Ready => "连接就绪",
    }
}
fn guidance(stage: Stage) -> &'static str {
    match stage {
        Stage::Resolving => "检查主机名、DNS 与 VPN；经跳板或代理解析的目标由远端处理。",
        Stage::Tcp => "检查地址、端口、网络路由和防火墙，再重试连接。",
        Stage::Proxy => "检查代理地址、协议及代理到目标服务器的连通性。",
        Stage::JumpChannel => "检查该级跳板到目标的网络，以及服务器是否允许 TCP 转发。",
        Stage::Handshake => "确认该端口提供 SSH 服务，并检查服务器与当前算法策略是否兼容。",
        Stage::HostKey => "请在主机密钥确认窗口核对指纹；服务器重装导致指纹变化时，核对新旧指纹后点击“确认更新并连接”。",
        Stage::Authentication | Stage::Credentials | Stage::WaitingForInput => {
            "检查账号和认证方式、凭据库或私钥；重试时可重新输入密码和动态码。"
        }
        Stage::Pty | Stage::Shell | Stage::TerminalChannel => {
            "检查账号是否允许终端登录；仅允许文件传输的账号可直接打开 SFTP。"
        }
        Stage::SftpChannel | Stage::SftpSubsystem => {
            "检查服务器是否启用 SFTP 子系统，以及当前账号是否有访问权限。"
        }
        Stage::InitialDirectory => "检查初始目录是否存在以及账号的目录访问权限。",
        _ => "检查下方错误信息和连接状态后重试。",
    }
}
/// Returns an explicit retry request. Uncertain outcomes never authorize replay.
pub(crate) fn draw(
    ui: &mut egui::Ui,
    view: Option<&View>,
    opening: bool,
    retry_enabled: bool,
) -> bool {
    let Some(view) = view else {
        if opening {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("准备连接 · 等待后台响应…");
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        return false;
    };
    let value = &view.diagnostic;
    let awaiting_receipt = opening && value.outcome == Outcome::Succeeded;
    let active = (value.outcome == Outcome::Running || awaiting_receipt) && opening;
    let extra = if active {
        view.received.elapsed().as_millis().min(86_400_000) as u64
    } else {
        0
    };
    let total = value.elapsed_ms.saturating_add(extra);
    let failed = matches!(value.outcome, Outcome::Failed | Outcome::Uncertain);
    let caption = if awaiting_receipt {
        "等待连接回执"
    } else if active {
        value.steps.last().map_or("连接中", |s| label(s.stage))
    } else {
        match value.outcome {
            Outcome::Succeeded => "连接已就绪",
            Outcome::Cancelled => "连接已取消",
            Outcome::Uncertain => "连接结果未确认",
            _ => "连接失败",
        }
    };
    let mut retry = false;
    cshell_ui::form::utility_bar().show(ui, |ui| {
        if value.outcome != Outcome::Succeeded || awaiting_receipt {
            ui.horizontal_wrapped(|ui| {
                if active {
                    ui.spinner();
                }
                ui.colored_label(
                    if failed {
                        cshell_ui::theme::WARNING
                    } else {
                        cshell_ui::theme::ACCENT
                    },
                    if failed {
                        format!(
                            "{caption} · {} · {:.2} 秒",
                            value.steps.last().map_or("未知阶段", |s| label(s.stage)),
                            total as f64 / 1000.0
                        )
                    } else {
                        format!("{caption} · {:.2} 秒", total as f64 / 1000.0)
                    },
                );
                if value.outcome == Outcome::Failed
                    && ui
                        .add_enabled(retry_enabled, egui::Button::new("重试连接"))
                        .clicked()
                {
                    retry = true;
                }
            });
        }
        let title = if value.outcome == Outcome::Succeeded && !awaiting_receipt {
            format!(
                "连接诊断 · 最近一次建立耗时 {:.2} 秒",
                total as f64 / 1000.0
            )
        } else {
            "连接诊断 · 阶段与耗时".into()
        };
        egui::CollapsingHeader::new(title)
            .id_salt((
                "connection-diagnostics",
                value.attempt,
                value.outcome != Outcome::Running && !awaiting_receipt,
            ))
            .default_open(failed || active)
            .show(ui, |ui| {
                ui.weak("按实际事件计时；总耗时包含等待输入。跳板 / 代理目标解析由远端处理。");
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .id_salt(("connection-stages", value.attempt))
                    .max_height((ui.ctx().content_rect().height() * 0.25).clamp(70.0, 180.0))
                    .show(ui, |ui| {
                        for row in &value.steps {
                            let status = match row.outcome {
                                Outcome::Running => "进行中",
                                Outcome::Succeeded => "完成",
                                Outcome::Failed => "失败",
                                Outcome::Cancelled => "取消",
                                Outcome::Uncertain => "待确认",
                            };
                            let milliseconds = row.elapsed_ms
                                + if row.outcome == Outcome::Running {
                                    extra
                                } else {
                                    0
                                };
                            let prefix = if row.hop == 0 {
                                if matches!(row.stage, Stage::Resolving | Stage::Tcp) {
                                    "网络端点"
                                } else {
                                    "目标"
                                }
                                .to_owned()
                            } else {
                                format!("跳板 {}", row.hop)
                            };
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    if matches!(row.outcome, Outcome::Failed | Outcome::Uncertain) {
                                        cshell_ui::theme::WARNING
                                    } else {
                                        cshell_ui::theme::MUTED
                                    },
                                    status,
                                );
                                let name_width =
                                    (ui.available_width() - 60.0 - ui.spacing().item_spacing.x)
                                        .max(24.0);
                                ui.add_sized(
                                    [name_width, 22.0],
                                    egui::Label::new(format!(
                                        "{prefix} · {} · {}",
                                        label(row.stage),
                                        row.endpoint
                                    ))
                                    .halign(egui::Align::Min)
                                    .truncate(),
                                )
                                .on_hover_text(&row.endpoint);
                                ui.add_sized(
                                    [60.0, 22.0],
                                    egui::Label::new(
                                        egui::RichText::new(format!(
                                            "{:.2}s",
                                            milliseconds as f64 / 1000.0
                                        ))
                                        .color(cshell_ui::theme::MUTED),
                                    )
                                    .halign(egui::Align::Max),
                                );
                            });
                        }
                    });
                if value.truncated {
                    ui.weak("部分中间阶段已省略，保留最初阶段和当前 / 最终阶段。");
                }
                if failed {
                    if let Some(last) = value.steps.last() {
                        ui.label(guidance(last.stage));
                    }
                    if value.outcome == Outcome::Uncertain {
                        ui.colored_label(
                            cshell_ui::theme::WARNING,
                            "请求结果尚未确认，请先检查会话状态；不要重复创建连接。",
                        );
                    }
                }
                if ui.small_button("复制诊断").clicked() {
                    let mut report = format!(
                        "CShell connection {:?} · {:.2}s\n",
                        value.outcome,
                        total as f64 / 1000.0
                    );
                    for step in &value.steps {
                        report.push_str(&format!(
                            "hop={} {} {} {:?} {:.2}s\n",
                            step.hop,
                            step.endpoint,
                            label(step.stage),
                            step.outcome,
                            step.elapsed_ms as f64 / 1000.0
                        ));
                    }
                    ui.ctx().copy_text(report);
                }
            });
    });
    if active {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
    retry
}

impl crate::DesktopApp {
    pub(super) fn draw_connection_diagnostics(&mut self, ui: &mut egui::Ui) {
        if !self.view_model.connection_diagnostics_open {
            return;
        }
        let view = self.active_ssh_profile().and_then(|(tab, profile)| {
            let view = self.daemon.as_ref()?.view();
            let matching_runtime = self.workspace.sessions.get(&tab)
                .map_or(view.session_id.is_none(), |id| view.session_id == Some(*id));
            (view.session_profile_id == Some(profile)
                && (matching_runtime || view.session_opening && self.workspace.pending_launch == Some(tab)))
                .then_some(view)
        });
        let mut open = true;
        let mut retry = false;
        egui::Window::new("连接诊断")
            .id(egui::Id::new("connection-diagnostics-window"))
            .open(&mut open)
            .default_width(480.0)
            .max_width((ui.ctx().content_rect().width() - 24.0).max(240.0))
            .show(ui.ctx(), |ui| {
                if let Some(view) = view {
                    retry = draw(ui, view.connection_trace.as_ref(), view.session_opening,
                        self.view_model.can_restart_ssh && !view.session_opening);
                    if view.connection_trace.is_none() && !view.session_opening {
                        ui.label("当前连接暂无诊断记录。");
                    }
                } else {
                    ui.label("请选择 SSH 连接查看本次诊断记录。");
                }
            });
        self.view_model.connection_diagnostics_open = open;
        if retry {
            self.request_ssh_control(true);
        }
    }
}
