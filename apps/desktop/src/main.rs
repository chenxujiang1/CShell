mod cursor_blink;
mod daemon_connection;
mod terminal_search;
mod visual_corpus;
mod window_e2e;

use cshell_domain::{InputAction, KeyCode, KeyEvent, Modifiers};
use cshell_render::{
    EguiFrame, LogReflowLayout, LogReflowRequest, LogScrollbarState, LogSourceId, LogSurfaceError,
    LogSurfaceModel, MAX_TERMINAL_SEARCH_QUERY_BYTES, TerminalCellPoint, TerminalDecorations,
    TerminalSelection, TerminalSelectionMode, TerminalSurfaceModel, TerminalViewport,
    WindowRenderer,
};
use cshell_ui::WorkbenchViewModel;
use cursor_blink::CursorBlinkState;
use daemon_connection::{DesktopConnectionConfig, DesktopDaemonConnection};
use std::error::Error;
use std::sync::Arc;
use terminal_search::{TerminalSearchRequest, TerminalSearchWorker};
use tracing::info;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

const SESSION_LOG_ARGUMENT: &str = "--session-log";
const TERMINAL_MULTI_CLICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Default)]
struct TerminalClickState {
    last: Option<(std::time::Instant, TerminalCellPoint)>,
    count: u8,
}

#[derive(Default)]
struct DesktopApp {
    window: Option<Arc<Window>>,
    renderer: Option<WindowRenderer>,
    egui_context: egui::Context,
    egui_state: Option<egui_winit::State>,
    last_renderer_attempt: Option<std::time::Instant>,
    view_model: WorkbenchViewModel,
    daemon: Option<DesktopDaemonConnection>,
    terminal_surface: TerminalSurfaceModel,
    terminal_decorations: TerminalDecorations,
    terminal_search: Option<TerminalSearchWorker>,
    terminal_search_open: bool,
    terminal_search_focus_requested: bool,
    terminal_search_query: String,
    terminal_search_options: cshell_render::TerminalSearchOptions,
    terminal_search_revision: u64,
    terminal_search_requested_generation: Option<u64>,
    log_surface: Option<LogSurfaceModel>,
    submitted_log_page: Option<(LogSourceId, u64)>,
    log_reflow: Option<LogReflowWorker>,
    terminal_viewport: Option<TerminalViewport>,
    viewport_rows: u16,
    cursor_position: Option<winit::dpi::PhysicalPosition<f64>>,
    terminal_selecting: bool,
    terminal_click: TerminalClickState,
    wheel_row_accumulator: f64,
    modifiers: ModifiersState,
    cursor_blink: CursorBlinkState,
    window_active: bool,
    window_e2e: Option<window_e2e::WindowE2e>,
}

struct LogReflowWorker {
    requests: Option<std::sync::mpsc::Sender<LogReflowRequest>>,
    results: std::sync::mpsc::Receiver<Result<LogReflowLayout, LogSurfaceError>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogScrollbarAction {
    FollowTail,
    JumpTo(u64),
}

impl LogReflowWorker {
    fn start() -> Result<Self, std::io::Error> {
        let (request_sender, request_receiver) = std::sync::mpsc::channel::<LogReflowRequest>();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("cshell-log-reflow".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    if result_sender.send(request.execute()).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests: Some(request_sender),
            results: result_receiver,
            worker: Some(worker),
        })
    }

    fn submit(&self, request: LogReflowRequest) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|sender| sender.send(request).is_ok())
    }
}

impl Drop for LogReflowWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _result = worker.join();
        }
    }
}

impl ApplicationHandler for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        match event_loop.create_window(
            WindowAttributes::default()
                .with_title("CShell Phase 0")
                .with_inner_size(winit::dpi::LogicalSize::new(1200.0, 760.0)),
        ) {
            Ok(window) => {
                let window = Arc::new(window);
                self.window_active = true;
                info!(window_id = ?window.id(), "desktop shell created");
                self.egui_state = Some(egui_winit::State::new(
                    self.egui_context.clone(),
                    egui::ViewportId::ROOT,
                    window.as_ref(),
                    Some(window.scale_factor() as f32),
                    window.theme(),
                    None,
                ));
                self.last_renderer_attempt = Some(std::time::Instant::now());
                match initialize_renderer(Arc::clone(&window)) {
                    Ok(renderer) => self.renderer = Some(renderer),
                    Err(error) => tracing::error!(%error, "cannot initialize terminal renderer"),
                }
                window.request_redraw();
                self.window = Some(window);
            }
            Err(error) => {
                tracing::error!(%error, "cannot create desktop window");
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = std::time::Instant::now();
        let mut redraw_needed = false;
        if let (Some(reflow), Some(surface)) = (&self.log_reflow, &mut self.log_surface) {
            while let Ok(result) = reflow.results.try_recv() {
                match result {
                    Ok(layout) => {
                        let promoted = surface.submit_reflow(layout);
                        redraw_needed |= promoted;
                        if let Some(e2e) = &mut self.window_e2e {
                            e2e.note_reflow(promoted);
                        }
                    }
                    Err(error) => {
                        surface.reflow_failed();
                        tracing::error!(%error, "background log reflow failed");
                    }
                }
            }
        }
        let search_response = self
            .terminal_search
            .as_ref()
            .and_then(TerminalSearchWorker::take_latest);
        if let Some(response) = search_response {
            let current_generation = self
                .terminal_surface
                .latest_snapshot()
                .map(|snapshot| snapshot.generation);
            if response.revision == self.terminal_search_revision
                && current_generation == Some(response.snapshot_generation)
            {
                match response.result {
                    Ok(result) => {
                        let previous_active =
                            self.terminal_decorations.active_search_match.unwrap_or(0);
                        self.terminal_decorations.search = result;
                        self.terminal_decorations.active_search_match =
                            (!self.terminal_decorations.search.matches.is_empty()).then_some(
                                previous_active
                                    .min(self.terminal_decorations.search.matches.len() - 1),
                            );
                        self.bump_terminal_decorations();
                        redraw_needed = true;
                    }
                    Err(error) => tracing::warn!(%error, "terminal search rejected"),
                }
            }
        }
        if let Some(daemon) = &self.daemon {
            let view = daemon.view();
            redraw_needed |= self.view_model.daemon_connected != view.connected;
            redraw_needed |= self.view_model.daemon_status_detail != view.detail;
            redraw_needed |= self.view_model.terminal_generation
                != view.snapshot.as_ref().map(|snapshot| snapshot.generation);
            self.view_model.daemon_connected = view.connected;
            self.view_model.daemon_status_detail = view.detail;
            self.view_model.terminal_generation =
                view.snapshot.as_ref().map(|snapshot| snapshot.generation);
            if let Some(snapshot) = view.snapshot {
                redraw_needed |= self.cursor_blink.synchronize(
                    Some(snapshot.as_ref()),
                    self.window_active && view.connected,
                    now,
                );
                redraw_needed |= self.terminal_surface.submit_snapshot(snapshot);
            }
            if let Some(page) = view.log_page {
                let key = (page.source_id, page.revision);
                if self.submitted_log_page != Some(key)
                    && let Some(log_surface) = &mut self.log_surface
                {
                    redraw_needed = true;
                    if let Err(error) = log_surface.submit_page(page) {
                        tracing::error!(%error, "daemon log page rejected by render model");
                    }
                    // A delivered page is immutable. Even when it is stale for
                    // the current anchor, retrying it every frame cannot make it
                    // acceptable and would only flood the log until the newer
                    // request arrives on the dedicated paging connection.
                    self.submitted_log_page = Some(key);
                }
            }
            if let Some(session_id) = view.session_id {
                if let Some(session) = self
                    .view_model
                    .sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                {
                    redraw_needed |= session.connected != view.connected;
                    session.connected = view.connected;
                } else {
                    redraw_needed = true;
                    self.view_model
                        .sessions
                        .push(cshell_ui::SessionTabViewModel {
                            id: session_id,
                            title: view
                                .session_title
                                .clone()
                                .unwrap_or_else(|| "Terminal".to_owned()),
                            connected: view.connected,
                        });
                    self.view_model.selected = Some(session_id);
                }
            }
        }
        if self.log_surface.is_some() {
            redraw_needed |= self.cursor_blink.synchronize(None, false, now);
        }
        let current_generation = self
            .terminal_surface
            .latest_snapshot()
            .map(|snapshot| snapshot.generation);
        if self.terminal_search_open
            && !self.terminal_search_query.is_empty()
            && current_generation != self.terminal_search_requested_generation
        {
            self.schedule_terminal_search(false);
        }
        if self.renderer.is_none()
            && self
                .last_renderer_attempt
                .is_none_or(|attempt| attempt.elapsed() >= std::time::Duration::from_secs(2))
            && let Some(window) = &self.window
        {
            self.last_renderer_attempt = Some(std::time::Instant::now());
            match initialize_renderer(Arc::clone(window)) {
                Ok(renderer) => {
                    self.renderer = Some(renderer);
                    redraw_needed = true;
                }
                Err(error) => tracing::error!(%error, "terminal renderer recovery failed"),
            }
        }
        if redraw_needed && let Some(window) = &self.window {
            window.request_redraw();
        }
        if self
            .window_e2e
            .as_mut()
            .is_some_and(window_e2e::WindowE2e::should_exit)
        {
            event_loop.exit();
        }
        if self.window_e2e.is_some()
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
        let poll_interval = if self.view_model.daemon_connected {
            std::time::Duration::from_millis(16)
        } else {
            std::time::Duration::from_millis(100)
        };
        let poll_deadline = now + poll_interval;
        let wake_deadline = self
            .cursor_blink
            .next_toggle()
            .map_or(poll_deadline, |deadline| deadline.min(poll_deadline));
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake_deadline));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        let mut egui_consumed = false;
        if let Some(state) = &mut self.egui_state {
            let response = state.on_window_event(&window, &event);
            egui_consumed = response.consumed;
            if response.repaint {
                window.request_redraw();
            }
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(e2e) = &mut self.window_e2e {
                    e2e.note_resize(size);
                }
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some(position);
                if self.terminal_selecting
                    && let Some(point) = self.terminal_point_at(position)
                    && let Some(selection) = &mut self.terminal_decorations.selection
                    && selection.focus != point
                {
                    selection.focus = point;
                    self.terminal_decorations.revision =
                        self.terminal_decorations.revision.wrapping_add(1);
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if self.log_surface.is_none() => match state {
                ElementState::Pressed if !egui_consumed => {
                    if let Some(position) = self.cursor_position
                        && let Some(point) = self.terminal_point_at(position)
                    {
                        let click_count = register_terminal_click(
                            &mut self.terminal_click,
                            point,
                            std::time::Instant::now(),
                        );
                        self.terminal_decorations.selection = Some(TerminalSelection {
                            anchor: point,
                            focus: point,
                            mode: if self.modifiers.alt_key() {
                                TerminalSelectionMode::Block
                            } else if click_count == 3 {
                                TerminalSelectionMode::Line
                            } else {
                                TerminalSelectionMode::Character
                            },
                        });
                        self.terminal_decorations.revision =
                            self.terminal_decorations.revision.wrapping_add(1);
                        self.terminal_selecting = true;
                        window.request_redraw();
                    }
                }
                ElementState::Released => self.terminal_selecting = false,
                ElementState::Pressed => {}
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::Focused(active) => {
                self.window_active = active;
                if self.cursor_blink.set_window_active(
                    active && self.view_model.daemon_connected,
                    std::time::Instant::now(),
                ) {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if self.log_surface.is_none()
                    && is_terminal_search_shortcut(&event, self.modifiers) =>
            {
                self.terminal_search_open = true;
                self.terminal_search_focus_requested = true;
                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. }
                if self.terminal_search_open
                    && event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                self.close_terminal_search();
                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. }
                if !egui_consumed && self.log_surface.is_none() =>
            {
                if is_terminal_copy_shortcut(&event, self.modifiers) {
                    if let (Some(selection), Some(snapshot)) = (
                        self.terminal_decorations.selection,
                        self.terminal_surface.latest_snapshot(),
                    ) {
                        match selection.text(snapshot) {
                            Ok(text) => self.egui_context.copy_text(text),
                            Err(error) => {
                                tracing::warn!(%error, "terminal selection copy rejected")
                            }
                        }
                    }
                    window.request_redraw();
                    return;
                }
                let terminal_modes = self
                    .terminal_surface
                    .latest_snapshot()
                    .map_or_else(cshell_terminal::TerminalModes::default, |snapshot| {
                        snapshot.terminal_modes
                    });
                if let Some(action) = terminal_key_action(&event, self.modifiers, terminal_modes)
                    && let Some(daemon) = &self.daemon
                {
                    if daemon.send_input(action) {
                        if self.cursor_blink.note_activity(std::time::Instant::now()) {
                            window.request_redraw();
                        }
                    } else {
                        tracing::warn!("terminal input was not accepted by the daemon client");
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(text))
                if !egui_consumed && self.log_surface.is_none() && !text.is_empty() =>
            {
                if let Some(daemon) = &self.daemon {
                    if daemon.send_input(InputAction::Text(text)) {
                        if self.cursor_blink.note_activity(std::time::Instant::now()) {
                            window.request_redraw();
                        }
                    } else {
                        tracing::warn!("IME commit was not accepted by the daemon client");
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let over_terminal = self
                    .cursor_position
                    .zip(self.terminal_viewport)
                    .is_some_and(|(cursor, viewport)| {
                        cursor.x >= f64::from(viewport.x)
                            && cursor.x < f64::from(viewport.x.saturating_add(viewport.width))
                            && cursor.y >= f64::from(viewport.y)
                            && cursor.y < f64::from(viewport.y.saturating_add(viewport.height))
                    });
                if over_terminal && self.viewport_rows > 0 {
                    let rows = match delta {
                        MouseScrollDelta::LineDelta(_, vertical) => f64::from(vertical) * 3.0,
                        MouseScrollDelta::PixelDelta(position) => {
                            let cell_height = self
                                .terminal_viewport
                                .map_or(1.0, |viewport| {
                                    f64::from(viewport.height) / f64::from(self.viewport_rows)
                                })
                                .max(1.0);
                            position.y / cell_height
                        }
                    };
                    self.wheel_row_accumulator -= rows;
                    let whole_rows = self.wheel_row_accumulator.trunc() as i32;
                    if whole_rows != 0 {
                        self.wheel_row_accumulator -= f64::from(whole_rows);
                        if let Some(surface) = &mut self.log_surface {
                            surface.scroll_visual_rows(whole_rows, self.viewport_rows);
                        }
                    }
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let Some(egui_state) = &mut self.egui_state else {
                    return;
                };
                let raw_input = egui_state.take_egui_input(&window);
                let mut terminal_rect = egui::Rect::NOTHING;
                let scrollbar_state = self
                    .log_surface
                    .as_ref()
                    .and_then(|surface| surface.scrollbar_state(self.viewport_rows.max(1)));
                let mut scrollbar_action = None;
                let context = self.egui_context.clone();
                let mut search_changed = false;
                let mut search_navigation = 0_i8;
                let mut close_search = false;
                let full_output = context.run_ui(raw_input, |ui| {
                    terminal_rect = cshell_ui::draw_workbench(ui, &mut self.view_model);
                    if let Some(state) = scrollbar_state {
                        scrollbar_action = draw_log_scrollbar(ui, &mut terminal_rect, state);
                    }
                    if self.terminal_search_open && self.log_surface.is_none() {
                        egui::Window::new("查找终端")
                            .anchor(egui::Align2::RIGHT_TOP, [-16.0, 48.0])
                            .collapsible(false)
                            .resizable(false)
                            .show(ui.ctx(), |ui| {
                                ui.horizontal(|ui| {
                                    let response = ui.add(
                                        egui::TextEdit::singleline(&mut self.terminal_search_query)
                                            .desired_width(260.0)
                                            .char_limit(MAX_TERMINAL_SEARCH_QUERY_BYTES)
                                            .hint_text("查找当前终端内容"),
                                    );
                                    if self.terminal_search_focus_requested {
                                        response.request_focus();
                                        self.terminal_search_focus_requested = false;
                                    }
                                    if response.changed() {
                                        truncate_utf8_bytes(
                                            &mut self.terminal_search_query,
                                            MAX_TERMINAL_SEARCH_QUERY_BYTES,
                                        );
                                        search_changed = true;
                                    }
                                    if ui.button("上一个").clicked() {
                                        search_navigation = -1;
                                    }
                                    if ui.button("下一个").clicked() {
                                        search_navigation = 1;
                                    }
                                    if ui.button("关闭").clicked() {
                                        close_search = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    search_changed |= ui
                                        .checkbox(
                                            &mut self.terminal_search_options.case_sensitive,
                                            "区分大小写",
                                        )
                                        .changed();
                                    search_changed |= ui
                                        .checkbox(
                                            &mut self.terminal_search_options.whole_word,
                                            "全词匹配",
                                        )
                                        .changed();
                                    let count = self.terminal_decorations.search.matches.len();
                                    let active = self
                                        .terminal_decorations
                                        .active_search_match
                                        .map_or(0, |index| index + 1);
                                    ui.label(format!("{active}/{count}"));
                                });
                                if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                                    search_navigation = if ui.input(|input| input.modifiers.shift) {
                                        -1
                                    } else {
                                        1
                                    };
                                }
                            });
                    }
                });
                if let (Some(surface), Some(action)) = (&mut self.log_surface, scrollbar_action) {
                    match action {
                        LogScrollbarAction::FollowTail => surface.follow_tail(),
                        LogScrollbarAction::JumpTo(line_id) => {
                            surface.jump_to_line(line_id, 0);
                        }
                    }
                }
                let egui::FullOutput {
                    platform_output,
                    mut textures_delta,
                    shapes,
                    pixels_per_point,
                    ..
                } = full_output;
                egui_state.handle_platform_output_with_event_loop(
                    &window,
                    event_loop,
                    platform_output,
                );
                if close_search {
                    self.close_terminal_search();
                } else {
                    if search_changed {
                        self.schedule_terminal_search(true);
                    }
                    if search_navigation != 0 {
                        self.navigate_terminal_search(search_navigation);
                    }
                }
                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                let paint_jobs = self.egui_context.tessellate(shapes, pixels_per_point);
                let viewport = TerminalViewport::from_logical_rect(terminal_rect, pixels_per_point);
                let viewport_rows = renderer.viewport_rows(viewport);
                self.terminal_viewport = Some(viewport);
                self.viewport_rows = viewport_rows;
                if self.log_surface.is_none()
                    && let Some(daemon) = &self.daemon
                {
                    daemon.request_resize(
                        viewport_rows,
                        renderer.viewport_columns(viewport),
                        viewport.width.min(u32::from(u16::MAX)) as u16,
                        viewport.height.min(u32::from(u16::MAX)) as u16,
                    );
                }
                let egui_frame = EguiFrame {
                    paint_jobs: &paint_jobs,
                    textures_delta: &mut textures_delta,
                    pixels_per_point,
                };
                let render_result = if let Some(log_surface) = &mut self.log_surface {
                    let viewport_columns = renderer.viewport_columns(viewport);
                    if let Some(request) = log_surface.request_reflow(viewport_columns)
                        && self
                            .log_reflow
                            .as_ref()
                            .is_none_or(|worker| !worker.submit(request))
                    {
                        log_surface.reflow_failed();
                    }
                    if let Some(request) = log_surface.take_page_request(viewport_rows) {
                        if let Some(daemon) = &self.daemon {
                            daemon.request_log_page(request);
                        } else if let Some(e2e) = &mut self.window_e2e {
                            let page = e2e.page_for(request);
                            if let Err(error) = log_surface.submit_page(page) {
                                e2e.fail(format!("generated log page was rejected: {error}"));
                            }
                        }
                    }
                    let frame = log_surface.prepare_frame(viewport_rows);
                    renderer.render_log(frame.as_ref(), viewport, Some(egui_frame))
                } else {
                    let frame = self.terminal_surface.prepare_frame(0, viewport_rows);
                    window.set_ime_allowed(true);
                    if let Some(frame) = &frame {
                        position_terminal_ime(&window, viewport, &frame.snapshot);
                    }
                    renderer.render(
                        frame.as_ref(),
                        viewport,
                        &self.terminal_decorations,
                        self.cursor_blink.visible(),
                        Some(egui_frame),
                    )
                };
                // A surface/device failure can return before GPU texture upload. The next
                // renderer recreates egui's font texture; clear this frame's abandoned deltas.
                textures_delta.clear();
                match render_result {
                    Ok(_outcome) => {
                        if let (Some(e2e), Some(log_surface)) =
                            (&mut self.window_e2e, &mut self.log_surface)
                        {
                            e2e.on_present(log_surface, &window, viewport_rows);
                        }
                    }
                    Err(error) => {
                        if let Some(e2e) = &mut self.window_e2e {
                            e2e.fail(format!("window render/present failed: {error}"));
                        }
                        tracing::error!(%error, "terminal frame rendering failed");
                        self.renderer = None;
                    }
                }
            }
            _ => {}
        }
    }
}

impl DesktopApp {
    fn bump_terminal_decorations(&mut self) {
        self.terminal_decorations.revision = self.terminal_decorations.revision.wrapping_add(1);
    }

    fn schedule_terminal_search(&mut self, reset_active: bool) {
        self.terminal_search_revision = self.terminal_search_revision.wrapping_add(1);
        self.terminal_decorations.search = Default::default();
        if reset_active {
            self.terminal_decorations.active_search_match = None;
        }
        self.bump_terminal_decorations();
        let Some(snapshot) = self.terminal_surface.latest_snapshot().cloned() else {
            self.terminal_search_requested_generation = None;
            return;
        };
        self.terminal_search_requested_generation = Some(snapshot.generation);
        if self.terminal_search_query.is_empty() {
            return;
        }
        if let Some(search) = &self.terminal_search {
            search.submit(TerminalSearchRequest {
                revision: self.terminal_search_revision,
                snapshot,
                query: self.terminal_search_query.clone(),
                options: self.terminal_search_options,
            });
        }
    }

    fn navigate_terminal_search(&mut self, direction: i8) {
        let count = self.terminal_decorations.search.matches.len();
        if count == 0 {
            return;
        }
        let current = self
            .terminal_decorations
            .active_search_match
            .unwrap_or(0)
            .min(count - 1);
        self.terminal_decorations.active_search_match = Some(if direction < 0 {
            current.checked_sub(1).unwrap_or(count - 1)
        } else {
            (current + 1) % count
        });
        self.bump_terminal_decorations();
    }

    fn close_terminal_search(&mut self) {
        self.terminal_search_open = false;
        self.terminal_search_focus_requested = false;
        self.terminal_search_revision = self.terminal_search_revision.wrapping_add(1);
        self.terminal_search_requested_generation = None;
        self.terminal_decorations.search = Default::default();
        self.terminal_decorations.active_search_match = None;
        self.bump_terminal_decorations();
    }

    fn terminal_point_at(
        &self,
        position: winit::dpi::PhysicalPosition<f64>,
    ) -> Option<TerminalCellPoint> {
        let viewport = self.terminal_viewport?;
        let snapshot = self.terminal_surface.latest_snapshot()?;
        terminal_point_at(position, viewport, snapshot.rows, snapshot.cols)
    }
}

fn terminal_point_at(
    position: winit::dpi::PhysicalPosition<f64>,
    viewport: TerminalViewport,
    rows: u16,
    cols: u16,
) -> Option<TerminalCellPoint> {
    if rows == 0
        || cols == 0
        || position.x < f64::from(viewport.x)
        || position.y < f64::from(viewport.y)
        || position.x >= f64::from(viewport.x.saturating_add(viewport.width))
        || position.y >= f64::from(viewport.y.saturating_add(viewport.height))
    {
        return None;
    }
    let x = position.x - f64::from(viewport.x);
    let y = position.y - f64::from(viewport.y);
    Some(TerminalCellPoint {
        row: ((y * f64::from(rows) / f64::from(viewport.height.max(1))) as u16).min(rows - 1),
        column: ((x * f64::from(cols) / f64::from(viewport.width.max(1))) as u16).min(cols - 1),
    })
}

fn is_terminal_copy_shortcut(event: &winit::event::KeyEvent, modifiers: ModifiersState) -> bool {
    event.state == ElementState::Pressed
        && (modifiers.super_key() || (modifiers.control_key() && modifiers.shift_key()))
        && matches!(&event.logical_key, Key::Character(text) if text.eq_ignore_ascii_case("c"))
}

fn is_terminal_search_shortcut(event: &winit::event::KeyEvent, modifiers: ModifiersState) -> bool {
    event.state == ElementState::Pressed
        && (modifiers.super_key() || modifiers.control_key())
        && matches!(&event.logical_key, Key::Character(text) if text.eq_ignore_ascii_case("f"))
}

fn truncate_utf8_bytes(text: &mut String, maximum: usize) {
    if text.len() <= maximum {
        return;
    }
    let mut boundary = maximum;
    while !text.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    text.truncate(boundary);
}

fn register_terminal_click(
    state: &mut TerminalClickState,
    point: TerminalCellPoint,
    now: std::time::Instant,
) -> u8 {
    let continues = state.last.is_some_and(|(last_at, last_point)| {
        last_point == point && now.duration_since(last_at) <= TERMINAL_MULTI_CLICK_INTERVAL
    });
    state.count = if continues && state.count < 3 {
        state.count + 1
    } else {
        1
    };
    state.last = Some((now, point));
    state.count
}

fn position_terminal_ime(
    window: &Window,
    viewport: TerminalViewport,
    snapshot: &cshell_terminal::FrameSnapshot,
) {
    let cell_width = (viewport.width / u32::from(snapshot.cols.max(1))).max(1);
    let cell_height = (viewport.height / u32::from(snapshot.rows.max(1))).max(1);
    let x = viewport
        .x
        .saturating_add(u32::from(snapshot.cursor_col).saturating_mul(cell_width));
    let y = viewport.y.saturating_add(
        u32::from(snapshot.cursor_row)
            .saturating_add(1)
            .saturating_mul(cell_height),
    );
    window.set_ime_cursor_area(
        winit::dpi::PhysicalPosition::new(x, y),
        winit::dpi::PhysicalSize::new(cell_width, cell_height),
    );
}

fn terminal_key_action(
    event: &winit::event::KeyEvent,
    modifiers: ModifiersState,
    modes: cshell_terminal::TerminalModes,
) -> Option<InputAction> {
    let pressed = event.state == ElementState::Pressed;
    let report_events = modes.kitty_enabled(cshell_terminal::TerminalModes::KITTY_REPORT_EVENTS);
    let report_all = modes.kitty_enabled(cshell_terminal::TerminalModes::KITTY_REPORT_ALL_KEYS);
    if (!pressed && !report_events) || (modifiers.super_key() && modes.kitty_keyboard_flags == 0) {
        return None;
    }
    let modifiers = Modifiers {
        ctrl: modifiers.control_key(),
        alt: modifiers.alt_key(),
        shift: modifiers.shift_key(),
        super_key: modifiers.super_key(),
    };
    let code = match &event.logical_key {
        Key::Character(text)
            if modifiers.ctrl || modifiers.alt || modifiers.super_key || report_all =>
        {
            KeyCode::Character(text.to_string())
        }
        Key::Character(_) if pressed => {
            return event
                .text
                .as_ref()
                .filter(|text| !text.is_empty())
                .map(|text| InputAction::Text(text.to_string()));
        }
        Key::Named(NamedKey::Enter) => KeyCode::Enter,
        Key::Named(NamedKey::Tab) => KeyCode::Tab,
        Key::Named(NamedKey::Backspace) => KeyCode::Backspace,
        Key::Named(NamedKey::Escape) => KeyCode::Escape,
        Key::Named(NamedKey::ArrowUp) => KeyCode::ArrowUp,
        Key::Named(NamedKey::ArrowDown) => KeyCode::ArrowDown,
        Key::Named(NamedKey::ArrowLeft) => KeyCode::ArrowLeft,
        Key::Named(NamedKey::ArrowRight) => KeyCode::ArrowRight,
        Key::Named(named) => KeyCode::Function(function_key_number(*named)?),
        _ => return None,
    };
    Some(InputAction::Key(KeyEvent {
        code,
        modifiers,
        pressed,
        repeated: event.repeat,
    }))
}

const fn function_key_number(key: NamedKey) -> Option<u8> {
    match key {
        NamedKey::F1 => Some(1),
        NamedKey::F2 => Some(2),
        NamedKey::F3 => Some(3),
        NamedKey::F4 => Some(4),
        NamedKey::F5 => Some(5),
        NamedKey::F6 => Some(6),
        NamedKey::F7 => Some(7),
        NamedKey::F8 => Some(8),
        NamedKey::F9 => Some(9),
        NamedKey::F10 => Some(10),
        NamedKey::F11 => Some(11),
        NamedKey::F12 => Some(12),
        NamedKey::F13 => Some(13),
        NamedKey::F14 => Some(14),
        NamedKey::F15 => Some(15),
        NamedKey::F16 => Some(16),
        NamedKey::F17 => Some(17),
        NamedKey::F18 => Some(18),
        NamedKey::F19 => Some(19),
        NamedKey::F20 => Some(20),
        NamedKey::F21 => Some(21),
        NamedKey::F22 => Some(22),
        NamedKey::F23 => Some(23),
        NamedKey::F24 => Some(24),
        _ => None,
    }
}

fn draw_log_scrollbar(
    ui: &mut egui::Ui,
    terminal_rect: &mut egui::Rect,
    state: LogScrollbarState,
) -> Option<LogScrollbarAction> {
    const WIDTH: f32 = 12.0;
    const MIN_THUMB: f32 = 24.0;
    if terminal_rect.width() <= WIDTH || terminal_rect.height() <= 0.0 {
        return None;
    }
    let track = egui::Rect::from_min_max(
        egui::pos2(terminal_rect.max.x - WIDTH, terminal_rect.min.y),
        terminal_rect.max,
    );
    terminal_rect.max.x -= WIDTH;
    let total = state.total_line_count.max(1);
    let visible_fraction = (f64::from(state.viewport_rows) / total as f64).clamp(0.03, 1.0);
    let thumb_height = (track.height() * visible_fraction as f32)
        .max(MIN_THUMB)
        .min(track.height());
    let progress = if state.follow_tail || total <= 1 {
        1.0
    } else {
        (state.top_line_id.saturating_sub(1) as f64 / total.saturating_sub(1) as f64)
            .clamp(0.0, 1.0) as f32
    };
    let thumb_top = track.top() + (track.height() - thumb_height) * progress;
    let thumb = egui::Rect::from_min_size(
        egui::pos2(track.left() + 2.0, thumb_top),
        egui::vec2(WIDTH - 4.0, thumb_height),
    );
    ui.painter()
        .rect_filled(track, 0.0, egui::Color32::from_black_alpha(70));
    ui.painter()
        .rect_filled(thumb, 2.0, egui::Color32::from_gray(130));
    let response = ui.interact(
        track,
        ui.make_persistent_id("cshell-log-scrollbar"),
        egui::Sense::click_and_drag(),
    );
    if !(response.clicked() || response.dragged()) {
        return None;
    }
    let pointer = response.interact_pointer_pos()?;
    let progress = ((pointer.y - track.top()) / track.height()).clamp(0.0, 1.0);
    Some(log_scrollbar_action(progress, total))
}

fn log_scrollbar_action(progress: f32, total_line_count: u64) -> LogScrollbarAction {
    let progress = progress.clamp(0.0, 1.0);
    if progress >= 0.999 {
        return LogScrollbarAction::FollowTail;
    }
    let total = total_line_count.max(1);
    let line_id = 1 + (f64::from(progress) * total.saturating_sub(1) as f64).round() as u64;
    LogScrollbarAction::JumpTo(line_id)
}

fn initialize_renderer(window: Arc<Window>) -> Result<WindowRenderer, Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(WindowRenderer::new(window))?)
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let arguments: Vec<_> = std::env::args().collect();
    let show_visual_corpus = arguments
        .iter()
        .any(|argument| argument == visual_corpus::ARGUMENT);
    let show_log_corpus = arguments
        .iter()
        .any(|argument| argument == visual_corpus::LOG_ARGUMENT);
    let show_session_log = arguments
        .iter()
        .any(|argument| argument == SESSION_LOG_ARGUMENT);
    let run_window_e2e = arguments
        .iter()
        .any(|argument| argument == window_e2e::ARGUMENT);
    let daemon = if show_visual_corpus || show_log_corpus || run_window_e2e {
        None
    } else {
        DesktopConnectionConfig::from_env()?
            .map(|config| DesktopDaemonConnection::start(config.with_log_pages(show_session_log)))
            .transpose()?
    };
    let mut app = DesktopApp {
        daemon,
        terminal_search: Some(TerminalSearchWorker::start()?),
        ..DesktopApp::default()
    };
    if show_visual_corpus {
        let snapshot = Arc::new(visual_corpus::snapshot());
        app.view_model.daemon_status_detail = "visual corpus: terminal-unicode-v1".to_owned();
        app.view_model.terminal_generation = Some(snapshot.generation);
        app.terminal_surface.submit_snapshot(snapshot);
    }
    if show_log_corpus {
        let mut log_surface = LogSurfaceModel::default();
        log_surface.submit_page(visual_corpus::log_page())?;
        app.view_model.daemon_status_detail = "log corpus: terminal-unicode-v1".to_owned();
        app.log_surface = Some(log_surface);
        app.log_reflow = Some(LogReflowWorker::start()?);
    } else if show_session_log {
        app.view_model.daemon_status_detail = "session log: loading".to_owned();
        app.log_surface = Some(LogSurfaceModel::default());
        app.log_reflow = Some(LogReflowWorker::start()?);
    }
    if run_window_e2e {
        let e2e = window_e2e::WindowE2e::new();
        let mut log_surface = LogSurfaceModel::default();
        log_surface.submit_page(e2e.initial_page())?;
        app.view_model.daemon_status_detail = "automated log window E2E".to_owned();
        app.log_surface = Some(log_surface);
        app.log_reflow = Some(LogReflowWorker::start()?);
        app.window_e2e = Some(e2e);
    }
    event_loop.run_app(&mut app)?;
    if let Some(e2e) = &app.window_e2e {
        e2e.finish()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LogScrollbarAction, TERMINAL_MULTI_CLICK_INTERVAL, TerminalCellPoint, TerminalClickState,
        TerminalViewport, log_scrollbar_action, register_terminal_click, terminal_point_at,
        truncate_utf8_bytes,
    };
    use std::time::{Duration, Instant};
    use winit::dpi::PhysicalPosition;

    #[test]
    fn scrollbar_maps_track_positions_to_stable_line_ids_and_tail_mode() {
        assert_eq!(
            log_scrollbar_action(0.0, 1_000),
            LogScrollbarAction::JumpTo(1)
        );
        assert_eq!(
            log_scrollbar_action(0.5, 1_000),
            LogScrollbarAction::JumpTo(501)
        );
        assert_eq!(
            log_scrollbar_action(1.0, 1_000),
            LogScrollbarAction::FollowTail
        );
    }

    #[test]
    fn terminal_pointer_mapping_is_bounded_and_rejects_viewport_edges() {
        let viewport = TerminalViewport {
            x: 10,
            y: 20,
            width: 800,
            height: 400,
        };
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(10.0, 20.0), viewport, 20, 80),
            Some(TerminalCellPoint { row: 0, column: 0 })
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(809.9, 419.9), viewport, 20, 80),
            Some(TerminalCellPoint {
                row: 19,
                column: 79,
            })
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(810.0, 100.0), viewport, 20, 80),
            None
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(100.0, 420.0), viewport, 20, 80),
            None
        );
        assert_eq!(
            terminal_point_at(PhysicalPosition::new(100.0, 100.0), viewport, 0, 80),
            None
        );
    }

    #[test]
    fn search_query_byte_limit_never_splits_utf8() {
        let mut query = "ab界cd".to_owned();
        truncate_utf8_bytes(&mut query, 4);
        assert_eq!(query, "ab");
        truncate_utf8_bytes(&mut query, 1);
        assert_eq!(query, "a");
    }

    #[test]
    fn triple_click_requires_the_same_cell_and_a_bounded_interval() {
        let mut state = TerminalClickState::default();
        let started = Instant::now();
        let point = TerminalCellPoint { row: 2, column: 3 };
        assert_eq!(register_terminal_click(&mut state, point, started), 1);
        assert_eq!(
            register_terminal_click(&mut state, point, started + Duration::from_millis(100)),
            2
        );
        assert_eq!(
            register_terminal_click(&mut state, point, started + Duration::from_millis(200)),
            3
        );
        assert_eq!(
            register_terminal_click(&mut state, point, started + Duration::from_millis(300)),
            1
        );
        assert_eq!(
            register_terminal_click(
                &mut state,
                TerminalCellPoint { row: 2, column: 4 },
                started + Duration::from_millis(350),
            ),
            1
        );
        assert_eq!(
            register_terminal_click(
                &mut state,
                TerminalCellPoint { row: 2, column: 4 },
                started + TERMINAL_MULTI_CLICK_INTERVAL + Duration::from_millis(400),
            ),
            1
        );
    }
}
