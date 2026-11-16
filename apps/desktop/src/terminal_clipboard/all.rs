//! Bounded, cancellable reads of local terminal RAM history; never writes remote input.
use super::*;
use std::{
    io::Write,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::mpsc,
};

#[derive(Clone)]
struct Capture {
    tab: Option<cshell_domain::TabId>,
    session: SessionId,
    epoch: cshell_domain::ConnectionId,
    snapshot: Arc<cshell_terminal::FrameSnapshot>,
    first: Option<u64>,
}
impl Capture {
    fn tail(&self) -> u64 {
        self.snapshot
            .screen_start_line
            .saturating_add(u64::from(self.snapshot.rows))
            .saturating_sub(1)
    }
}

enum Outcome {
    Text(u64, zeroize::Zeroizing<String>),
    TooLarge(u64),
    Export(tempfile::NamedTempFile, std::path::PathBuf),
    Cancelled,
}
struct Job {
    cancel: Arc<AtomicBool>,
    results: mpsc::Receiver<Result<Outcome, String>>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
pub(crate) struct Selection {
    capture: Capture,
    job: Option<Job>,
    text: Option<zeroize::Zeroizing<String>>,
    too_large: bool,
    error: Option<String>,
}
static READERS: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Permit {
    fn acquire() -> Result<Self, String> {
        READERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 2).then_some(n + 1)
            })
            .map(|_| Self)
            .map_err(|_| "正在结束其他历史读取，请稍后重试。".into())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        READERS.fetch_sub(1, Ordering::Release);
    }
}

impl Selection {
    pub(crate) fn text(&self) -> Result<String, cshell_render::TerminalInteractionError> {
        if self.too_large {
            return Err(cshell_render::TerminalInteractionError::SelectionTooLarge(
                cshell_render::MAX_TERMINAL_SELECTION_BYTES,
            ));
        }
        self.text
            .as_ref()
            .map(|t| t.to_string())
            .ok_or(cshell_render::TerminalInteractionError::SelectionUnavailable)
    }
    fn matches(&self, app: &DesktopApp) -> bool {
        self.capture.tab == app.current_workspace_tab
            && app.daemon.as_ref().is_some_and(|daemon| {
                let view = daemon.view();
                view.session_id == Some(self.capture.session)
                    && view.input_epoch == self.capture.epoch
                    && view
                        .snapshot
                        .as_ref()
                        .is_some_and(|s| s.cols == self.capture.snapshot.cols)
            })
    }
}

fn spawn(
    config: DesktopConnectionConfig,
    capture: Capture,
    export: bool,
    ready: Option<zeroize::Zeroizing<String>>,
) -> Result<Job, String> {
    let permit = Permit::acquire()?;
    let cancel = Arc::new(AtomicBool::new(false));
    let stopped = cancel.clone();
    let (tx, results) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("terminal-selection-reader".into())
        .spawn(move || {
            let _permit = permit;
            let run = || -> Result<Outcome, String> {
                let destination = if export {
                    let Some(path) = rfd::FileDialog::new()
                        .set_file_name("terminal.txt")
                        .add_filter("文本", &["txt"])
                        .save_file()
                    else {
                        return Ok(Outcome::Cancelled);
                    };
                    Some(path)
                } else {
                    None
                };
                if stopped.load(Ordering::Acquire) {
                    return Ok(Outcome::Cancelled);
                }
                let mut file = destination
                    .as_ref()
                    .map(|path| {
                        tempfile::NamedTempFile::new_in(
                            path.parent().unwrap_or_else(|| std::path::Path::new(".")),
                        )
                    })
                    .transpose()
                    .map_err(|e| format!("无法创建导出文件：{e}"))?;
                if let Some(ready) = ready
                    && let (Some(mut target), Some(path)) = (file.take(), destination.clone())
                {
                    target
                        .write_all(ready.as_bytes())
                        .map_err(|e| e.to_string())?;
                    target.as_file().sync_all().map_err(|e| e.to_string())?;
                    return Ok(Outcome::Export(target, path));
                }
                let mut text = zeroize::Zeroizing::new(String::new());
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                let limit = if export {
                    256 * 1024 * 1024
                } else {
                    cshell_render::MAX_TERMINAL_SELECTION_BYTES
                };
                let result = runtime
                    .block_on(async {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(60),
                            read_capture(&config, &capture, &stopped, limit, |chunk| {
                                if let Some(file) = &mut file {
                                    file.write_all(chunk.as_bytes())
                                        .map_err(|e| e.to_string())?;
                                } else {
                                    text.push_str(chunk);
                                }
                                Ok(())
                            }),
                        )
                        .await
                    })
                    .map_err(|_| "历史读取超时，请重试或缩小范围。".to_owned())??;
                if stopped.load(Ordering::Acquire) {
                    return Ok(Outcome::Cancelled);
                }
                match result {
                    ReadResult::TooLarge(first) if !export => Ok(Outcome::TooLarge(first)),
                    ReadResult::TooLarge(_) => {
                        Err("导出超过 256 MiB 上限，未写入目标文件。".into())
                    }
                    ReadResult::Complete(first) => {
                        if let (Some(file), Some(path)) = (file, destination) {
                            file.as_file().sync_all().map_err(|e| e.to_string())?;
                            Ok(Outcome::Export(file, path))
                        } else {
                            Ok(Outcome::Text(first, text))
                        }
                    }
                }
            };
            let _ = tx.send(run());
        })
        .map_err(|e| format!("历史读取无法启动：{e}"))?;
    Ok(Job { cancel, results })
}

enum ReadResult {
    Complete(u64),
    TooLarge(u64),
}

async fn read_capture(
    config: &DesktopConnectionConfig,
    capture: &Capture,
    cancel: &AtomicBool,
    limit: usize,
    mut output: impl FnMut(&str) -> Result<(), String>,
) -> Result<ReadResult, String> {
    use cshell_ipc::{
        Envelope, Handshake, client_handshake, envelope, features, read_envelope, transport,
        write_envelope,
    };
    let resolved = config.resolve().map_err(|e| e.to_string())?;
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        transport::connect(&resolved.endpoint),
    )
    .await
    .map_err(|_| "后台连接超时".to_owned())?
    .map_err(|e| e.to_string())?;
    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::LOG_PAGING | features::TERMINAL_SCROLLBACK;
    let negotiated = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client_handshake(&mut stream, 1, handshake),
    )
    .await
    .map_err(|_| "后台握手超时".to_owned())?
    .map_err(|e| e.to_string())?;
    if negotiated.feature_bits & features::TERMINAL_SCROLLBACK == 0 {
        return Err("后台版本不支持完整终端历史读取。".into());
    }
    let mut id = 2;
    let mut first = capture.first.unwrap_or(1);
    let mut cursor = first;
    let mut started = false;
    let mut continued = false;
    let mut bytes = 0_usize;
    let history_end = capture.snapshot.screen_start_line.saturating_sub(1);
    while cursor <= history_end {
        if cancel.load(Ordering::Acquire) {
            return Err("已取消历史读取".into());
        }
        let request = cshell_ipc::LogPageRequest {
            terminal_history: true,
            session_id: capture.session.as_uuid().as_bytes().to_vec(),
            anchor_line_id: Some(cursor),
            cell_offset: 0,
            rows_before: 0,
            rows_after: 255,
        };
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: id,
                payload: Some(envelope::Payload::LogPageRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_envelope(&mut stream),
        )
        .await
        .map_err(|_| "历史分页读取超时".to_owned())?
        .map_err(|e| e.to_string())?;
        if response.request_id != id {
            return Err("历史响应已过期".into());
        }
        id += 1;
        let Some(envelope::Payload::LogPage(page)) = response.payload else {
            return Err("后台未返回历史页面".into());
        };
        page.validate().map_err(|e| e.to_string())?;
        if page.session_id != capture.session.as_uuid().as_bytes() {
            return Err("历史目标已变化".into());
        }
        if !started && capture.first.is_none() {
            first = page.first_line_id.max(1);
            cursor = first;
            if cursor > capture.snapshot.screen_start_line {
                return Err("选区历史已被回收，请重新全选。".into());
            }
        }
        if page.first_line_id.max(1) > cursor {
            return Err("选区历史已被回收，请重新全选。".into());
        }
        let previous_cursor = cursor;
        for mut row in page.rows {
            if cancel.load(Ordering::Acquire) {
                return Err("已取消历史读取".into());
            }
            if row.line_id < cursor {
                continue;
            }
            if row.line_id > history_end {
                break;
            }
            if row.line_id != cursor {
                return Err("历史内容不连续，未复制不完整内容。".into());
            }
            if row.truncated {
                // Retry one row with the full codec text/style budget.
                let request = cshell_ipc::LogPageRequest {
                    terminal_history: true,
                    session_id: capture.session.as_uuid().as_bytes().to_vec(),
                    anchor_line_id: Some(cursor),
                    cell_offset: 0,
                    rows_before: 0,
                    rows_after: 0,
                };
                write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: id,
                        payload: Some(envelope::Payload::LogPageRequest(request)),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
                let response = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    read_envelope(&mut stream),
                )
                .await
                .map_err(|_| "历史读取超时".to_owned())?
                .map_err(|e| e.to_string())?;
                if response.request_id != id {
                    return Err("历史响应不匹配".into());
                }
                id += 1;
                let Some(envelope::Payload::LogPage(single)) = response.payload else {
                    return Err("历史行无法完整读取".into());
                };
                single.validate().map_err(|e| e.to_string())?;
                if single.session_id != capture.session.as_uuid().as_bytes() {
                    return Err("历史目标已变化".into());
                }
                row = single
                    .rows
                    .into_iter()
                    .find(|r| r.line_id == cursor && !r.truncated)
                    .ok_or_else(|| "历史行已回收或截断，无法完整复制。".to_owned())?;
            }
            if !emit_row(
                &row.text,
                row.soft_wrap,
                &mut started,
                &mut continued,
                &mut bytes,
                limit,
                &mut output,
            )? {
                return Ok(ReadResult::TooLarge(first));
            }
            cursor += 1;
        }
        if cursor == previous_cursor {
            return Err("后台历史未前进，已停止读取。".into());
        }
    }
    for row in 0..capture.snapshot.rows {
        if cancel.load(Ordering::Acquire) {
            return Err("已取消历史读取".into());
        }
        let cells = capture
            .snapshot
            .row(row)
            .ok_or_else(|| "屏幕快照不完整".to_owned())?;
        let soft = cells.iter().any(cshell_terminal::Cell::soft_wrap);
        let mut text = cells
            .iter()
            .filter(|c| {
                !matches!(
                    c.width,
                    cshell_terminal::CellWidth::WideSpacer
                        | cshell_terminal::CellWidth::LeadingWideSpacer
                )
            })
            .flat_map(cshell_terminal::Cell::characters)
            .collect::<String>();
        if !soft {
            text.truncate(text.trim_end_matches(' ').len());
        }
        if !emit_row(
            &text,
            soft,
            &mut started,
            &mut continued,
            &mut bytes,
            limit,
            &mut output,
        )? {
            return Ok(ReadResult::TooLarge(first));
        }
    }
    Ok(ReadResult::Complete(first))
}

fn emit_row(
    text: &str,
    soft: bool,
    started: &mut bool,
    continued: &mut bool,
    bytes: &mut usize,
    limit: usize,
    output: &mut impl FnMut(&str) -> Result<(), String>,
) -> Result<bool, String> {
    let newline = *started && !*continued;
    let next = bytes
        .saturating_add(text.len())
        .saturating_add(usize::from(newline));
    if next > limit {
        return Ok(false);
    }
    if newline {
        output("\n")?;
    }
    output(text)?;
    *bytes = next;
    *started = true;
    *continued = soft;
    Ok(true)
}

impl DesktopApp {
    pub(crate) fn select_terminal_screen(&mut self) {
        self.clear_clipboard_selection();
        let history = self
            .log_surface
            .as_ref()
            .and_then(|s| s.prepare_frame(self.viewport_rows));
        let snapshot = history
            .as_ref()
            .map(|f| Arc::new(history_snapshot(f, self.viewport_rows)))
            .or_else(|| self.terminal_surface.latest_snapshot().cloned());
        let Some(snapshot) = snapshot else {
            return;
        };
        if snapshot.rows == 0 || snapshot.cols == 0 {
            return;
        }
        self.terminal_clipboard.owner = self.current_workspace_tab;
        self.terminal_clipboard.history_frame = history;
        self.terminal_clipboard.snapshot = Some(snapshot.clone());
        self.terminal_decorations.selection = Some(TerminalSelection {
            anchor: TerminalCellPoint { row: 0, column: 0 },
            focus: TerminalCellPoint {
                row: snapshot.rows - 1,
                column: snapshot.cols - 1,
            },
            mode: TerminalSelectionMode::Character,
        });
        self.update_clipboard_highlight();
        self.pause_terminal_reading();
    }
    pub(crate) fn select_terminal_all(&mut self) {
        self.start_terminal_all(false);
    }
    fn start_terminal_all(&mut self, export: bool) {
        let Some(session) = self.active_terminal_session_id() else {
            return;
        };
        let Some(snapshot) = self.terminal_surface.latest_snapshot().cloned() else {
            return;
        };
        let Some(config) = self
            .daemon_config
            .as_ref()
            .or(self.workspace_config.as_ref())
            .cloned()
        else {
            return;
        };
        let Some(view) = self.daemon.as_ref().map(|d| d.view()) else {
            return;
        };
        self.select_terminal_screen();
        let mut frozen = snapshot.as_ref().clone();
        let last_content = (0..frozen.rows)
            .rev()
            .find(|row| {
                frozen.row(*row).is_some_and(|cells| {
                    cells
                        .iter()
                        .any(|c| c.character != ' ' || !c.zerowidth().is_empty())
                })
            })
            .unwrap_or(0);
        frozen.rows = frozen
            .rows
            .min(last_content.max(frozen.cursor_row).saturating_add(1));
        let snapshot = Arc::new(frozen);
        if self.terminal_clipboard.history_frame.is_none() {
            self.terminal_clipboard.snapshot = Some(snapshot.clone());
            if let Some(selection) = &mut self.terminal_decorations.selection {
                selection.focus.row = snapshot.rows.saturating_sub(1);
                selection.focus.column = snapshot.cols.saturating_sub(1);
            }
        }
        let capture = Capture {
            tab: self.current_workspace_tab,
            session,
            epoch: view.input_epoch,
            snapshot,
            first: None,
        };
        match spawn(config, capture.clone(), export, None) {
            Ok(job) => {
                self.terminal_clipboard.all = Some(Selection {
                    capture,
                    job: Some(job),
                    text: None,
                    too_large: false,
                    error: None,
                });
                self.clipboard_notice("正在全选当前终端保留的内容；不会自动复制。".into());
            }
            Err(error) => self.view_model.input_warning = Some(error),
        }
    }
    pub(crate) fn export_terminal_all(&mut self) {
        if self.terminal_clipboard.all.is_none() {
            self.start_terminal_all(true);
            return;
        }
        let Some(mut selection) = self.terminal_clipboard.all.take() else {
            return;
        };
        selection.job = None;
        let Some(config) = self
            .daemon_config
            .as_ref()
            .or(self.workspace_config.as_ref())
            .cloned()
        else {
            return;
        };
        match spawn(
            config,
            selection.capture.clone(),
            true,
            selection.text.clone(),
        ) {
            Ok(job) => {
                selection.job = Some(job);
                self.terminal_clipboard.all = Some(selection);
            }
            Err(error) => {
                self.view_model.input_warning = Some(error);
                self.terminal_clipboard.all = Some(selection);
            }
        }
    }
    pub(crate) fn poll_terminal_all(&mut self) -> bool {
        let Some(mut selection) = self.terminal_clipboard.all.take() else {
            return false;
        };
        if !selection.matches(self) {
            self.clear_clipboard_selection();
            self.view_model.input_warning = Some("终端或布局已变化，请重新全选。".into());
            return true;
        }
        let result = selection
            .job
            .as_ref()
            .and_then(|job| match job.results.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("历史读取已停止，请重试。".into()))
                }
            });
        let changed = result.is_some();
        if let Some(result) = result {
            selection.job = None;
            match result {
                Ok(Outcome::Text(first, text)) => {
                    selection.capture.first = Some(first);
                    selection.text = Some(text);
                    self.clipboard_notice("已全选当前终端；按复制快捷键复制。".into());
                }
                Ok(Outcome::TooLarge(first)) => {
                    selection.capture.first = Some(first);
                    selection.too_large = true;
                    self.view_model.input_warning =
                        Some("全选超过 16 MiB 复制上限，请右键导出当前终端；不会截断复制。".into());
                }
                Ok(Outcome::Export(file, path)) => match file.persist(&path) {
                    Ok(_) => self.clipboard_notice("终端内容已导出。".into()),
                    Err(error) => {
                        self.view_model.input_warning =
                            Some(format!("导出保存失败：{}", error.error))
                    }
                },
                Ok(Outcome::Cancelled) => {}
                Err(error) => {
                    selection.error = Some(error.clone());
                    self.view_model.input_warning = Some(error);
                }
            }
        }
        if let Some(frame) = self
            .log_surface
            .as_ref()
            .and_then(|s| s.prepare_frame(self.viewport_rows))
        {
            let ranges: Arc<[_]> = frame
                .page
                .rows
                .iter()
                .filter(|r| {
                    r.line_id >= selection.capture.first.unwrap_or(1)
                        && r.line_id <= selection.capture.tail()
                })
                .map(|r| cshell_render::LogSearchMatch {
                    line_id: r.line_id,
                    byte_range: 0..r.text.len() as u32,
                })
                .collect::<Vec<_>>()
                .into();
            if self.log_decorations.selection_ranges != ranges {
                self.log_decorations.selection_ranges = ranges;
                self.log_decorations.revision = self.log_decorations.revision.wrapping_add(1);
            }
        }
        self.terminal_clipboard.all = Some(selection);
        changed
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
