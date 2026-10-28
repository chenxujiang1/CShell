//! Native SFTP windows share the GUI process and its daemon lifetime, but have
//! their own OS window, input state, DPI, renderer, and pinned SSH session.
use super::*;
use std::time::{Duration, Instant};
mod placement;
mod tabs;
pub(super) use tabs::SftpTabs;

pub(super) struct SftpWindow {
    pub window: Arc<Window>,
    title: String,
    context: egui::Context,
    state: egui_winit::State,
    renderer: Option<WindowRenderer>,
    repaint: Option<Instant>,
    renderer_retry: Instant,
    upload_fonts: bool,
    placement: crate::sftp_settings::WindowPlacement,
    placement_due: Option<Instant>,
}

impl SftpWindow {
    fn new(
        event_loop: &ActiveEventLoop,
        proxy: Option<&EventLoopProxy<DesktopEvent>>,
        title: &str,
    ) -> Result<Self, Box<dyn Error>> {
        // No parent/owner relationship: resizing, minimizing and monitor placement
        // must be independent of the terminal's client rectangle.
        let primary = event_loop.primary_monitor();
        let mut monitors: Vec<_> = event_loop.available_monitors().collect();
        monitors.sort_by_key(|monitor| {
            if primary.as_ref() == Some(monitor) {
                0
            } else {
                1
            }
        });
        let screens: Vec<_> = monitors
            .iter()
            .map(|m| placement::Screen {
                origin: [m.position().x, m.position().y],
                size: [m.size().width, m.size().height],
                scale: m.scale_factor(),
            })
            .collect();
        let placement = placement::restore(crate::sftp_settings::window(), &screens);
        let mut attributes = WindowAttributes::default()
            .with_title(title)
            .with_window_icon(crate::app_icon::window_icon())
            .with_inner_size(winit::dpi::LogicalSize::new(
                placement.inner_size[0],
                placement.inner_size[1],
            ))
            .with_min_inner_size(winit::dpi::LogicalSize::new(
                720.0_f64.min(placement.inner_size[0]),
                480.0_f64.min(placement.inner_size[1]),
            ))
            .with_resizable(true)
            .with_theme(Some(winit::window::Theme::Dark))
            .with_visible(false);
        if let Some([x, y]) = placement.position {
            attributes = attributes.with_position(winit::dpi::PhysicalPosition::new(x, y));
        }
        let window = Arc::new(event_loop.create_window(attributes)?);
        let context = egui::Context::default();
        ui_fonts::install(&context);
        cshell_ui::install_workbench_style(&context);
        let mut state = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        if let Some(proxy) = proxy {
            state.init_accesskit(event_loop, &window, proxy.clone());
        }
        let renderer = initialize_renderer(window.clone(), false)?;
        window.set_maximized(placement.maximized);
        window.set_visible(true);
        window.request_redraw();
        Ok(Self {
            window,
            title: title.to_owned(),
            context,
            state,
            renderer: Some(renderer),
            repaint: None,
            renderer_retry: Instant::now(),
            upload_fonts: false,
            placement,
            placement_due: None,
        })
    }

    fn set_title(&mut self, title: &str) {
        if self.title != title {
            self.window.set_title(title);
            self.title = title.to_owned();
        }
    }

    fn remember_placement(&mut self) {
        let size = self.window.inner_size();
        if self.window.is_minimized() == Some(true) || size.width == 0 || size.height == 0 {
            return;
        }
        let mut current = self.placement.clone();
        current.maximized = self.window.is_maximized();
        if !current.maximized {
            let logical = size.to_logical::<f64>(self.window.scale_factor());
            current.inner_size = [logical.width, logical.height];
            if let Ok(position) = self.window.outer_position() {
                current.position = Some([position.x, position.y]);
            }
        }
        if current != self.placement {
            self.placement = current;
            self.placement_due = Some(Instant::now() + Duration::from_millis(350));
        }
    }
    fn flush_placement(&mut self) {
        if self.placement_due.take().is_some()
            && let Err(error) = crate::sftp_settings::save_window(&self.placement)
        {
            tracing::warn!(%error,"cannot save SFTP window placement");
        }
    }
    pub fn poll(&mut self, now: Instant) {
        if self.placement_due.is_some_and(|due| now >= due) {
            self.flush_placement();
        }
        if self.renderer.is_none() && now >= self.renderer_retry {
            self.renderer_retry = now + Duration::from_secs(2);
            match initialize_renderer(self.window.clone(), true) {
                Ok(renderer) => {
                    self.renderer = Some(renderer);
                    self.upload_fonts = true;
                    self.window.request_redraw();
                }
                Err(error) => tracing::warn!(%error, "SFTP renderer recovery failed"),
            }
        }
        if self.repaint.is_some_and(|deadline| now >= deadline) {
            self.repaint = None;
            self.window.request_redraw();
        }
    }

    pub fn accesskit(&mut self, event: DesktopEvent) {
        let DesktopEvent::AccessKit(event) = event;
        match event.window_event {
            egui_winit::accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.context.enable_accesskit()
            }
            egui_winit::accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                self.context.disable_accesskit()
            }
            egui_winit::accesskit_winit::WindowEvent::ActionRequested(request) => {
                self.state.on_accesskit_action_request(request);
            }
        }
        self.window.request_redraw();
    }

    fn event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: WindowEvent,
        panel: &mut SftpPanel,
        auth: &mut crate::keyboard_auth::Dialog,
        session: Option<SessionId>,
        view: Option<&sftp_connection::SftpView>,
    ) -> Option<sftp_connection::SftpClientCommand> {
        let response = self.state.on_window_event(&self.window, &event);
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            self.window.request_redraw();
        }
        match event {
            WindowEvent::Moved(_) => self.remember_placement(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.remember_placement();
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(self.window.inner_size());
                }
                self.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let size = self.window.inner_size();
                if size.width == 0 || size.height == 0 {
                    return None;
                }
                let input = self.state.take_egui_input(&self.window);
                let mut command = None;
                let output = self.context.run_ui(input, |ui| {
                    ui.painter()
                        .rect_filled(ui.max_rect(), 0.0, cshell_ui::theme::BACKGROUND);
                    egui::Frame::NONE.inner_margin(10).show(ui, |ui| {
                        command = panel.draw_contents(ui, session, view);
                        auth.draw(ui.ctx());
                    });
                });
                self.repaint = output
                    .viewport_output
                    .get(&egui::ViewportId::ROOT)
                    .and_then(|v| Instant::now().checked_add(v.repaint_delay));
                let egui::FullOutput {
                    platform_output,
                    mut textures_delta,
                    shapes,
                    pixels_per_point,
                    ..
                } = output;
                self.state.handle_platform_output_with_event_loop(
                    &self.window,
                    event_loop,
                    platform_output,
                );
                let jobs = self.context.tessellate(shapes, pixels_per_point);
                if self.upload_fonts {
                    textures_delta.push(
                        egui::TextureId::default(),
                        egui::epaint::ImageDelta::full(
                            self.context.fonts(|fonts| fonts.image()),
                            Default::default(),
                        ),
                    );
                }
                if let Some(renderer) = &mut self.renderer {
                    let result = renderer.render_panes(
                        &[],
                        Some(EguiFrame {
                            paint_jobs: &jobs,
                            textures_delta: &mut textures_delta,
                            pixels_per_point,
                        }),
                    );
                    match result {
                        Ok(_) => self.upload_fonts = false,
                        Err(error) => {
                            tracing::warn!(%error, "SFTP window render failed; recovering");
                            self.renderer = None;
                            self.renderer_retry = Instant::now() + Duration::from_secs(2);
                        }
                    }
                }
                // Renderer creation/recovery may abandon this frame before texture upload.
                textures_delta.clear();
                return command;
            }
            _ => {}
        }
        None
    }
}

impl Drop for SftpWindow {
    fn drop(&mut self) {
        self.remember_placement();
        self.flush_placement();
    }
}

impl DesktopApp {
    pub(super) fn sync_sftp_window(&mut self, event_loop: &ActiveEventLoop) {
        if !self.sftp_panel.open {
            self.sftp_window = None;
            return;
        }
        let title = self
            .sftp
            .as_ref()
            .and_then(|s| s.view().connection_info.map(|i| i.title))
            .unwrap_or_else(|| "独立文件会话".into());
        let title = format!("SFTP 文件 - {title} - CShell");
        if let Some(window) = &mut self.sftp_window {
            window.set_title(&title);
        } else {
            match SftpWindow::new(event_loop, self.accesskit_proxy.as_ref(), &title) {
                Ok(window) => self.sftp_window = Some(window),
                Err(error) => {
                    tracing::error!(%error, "cannot create native SFTP window");
                    self.sftp_panel.open = false;
                    self.view_model.input_warning = Some(format!("无法打开 SFTP 窗口：{error}"));
                }
            }
        }
    }

    pub(super) fn sftp_window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            self.close_all_sftp_tabs();
            self.sftp_panel.open = false;
            self.sftp_window = None;
            return;
        }
        let view = self.sftp.as_ref().map(DesktopSftpConnection::view).map(|v|self.sftp_tabs.local.display(v));
        let command = self.sftp_window.as_mut().and_then(|window| {
            window.event(
                event_loop,
                event,
                &mut self.sftp_panel,
                &mut self.keyboard_auth,
                self.sftp_session,
                view.as_ref(),
            )
        });
        if let Some(command) = command {
            self.submit_sftp_command(command);
        }
        if let Some(action)=self.sftp_panel.take_tab_action() {self.apply_sftp_tab_action(action);}
        if let Some(command) = self.sftp_panel.take_profile_command() {
            if !self.profiles.as_ref().is_some_and(|p| p.request(command)) {
                self.sftp_panel.profile_request_failed();
            }
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use winit::platform::windows::EventLoopBuilderExtWindows;

    #[test]
    #[ignore = "creates real native windows; run explicitly for native SFTP verification"]
    fn native_sftp_detaches_resizes_maximizes_and_closes_independently() {
        struct Check {
            windows: DesktopWindows,
            started: Instant,
            phase: u8,
            failure: Option<String>,
            view: sftp_connection::SftpView,
            expected_placement: Option<crate::sftp_settings::WindowPlacement>,
        }
        impl ApplicationHandler<DesktopEvent> for Check {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                self.windows.resumed(event_loop);
                self.windows.primary.sftp_panel.open = true;
                self.windows.primary.sync_sftp_window(event_loop);
                self.started = Instant::now();
            }
            fn user_event(&mut self, event_loop: &ActiveEventLoop, event: DesktopEvent) {
                self.windows.user_event(event_loop, event);
            }
            fn window_event(
                &mut self,
                event_loop: &ActiveEventLoop,
                id: WindowId,
                event: WindowEvent,
            ) {
                if matches!(event, WindowEvent::RedrawRequested)
                    && let Some(window) = &mut self.windows.primary.sftp_window
                    && window.window.id() == id
                {
                    window.event(
                        event_loop,
                        event,
                        &mut self.windows.primary.sftp_panel,
                        &mut self.windows.primary.keyboard_auth,
                        self.view.session_id,
                        Some(&self.view),
                    );
                    return;
                }
                self.windows.window_event(event_loop, id, event);
            }
            fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
                if self.started.elapsed() > Duration::from_secs(20) {
                    self.failure = Some(format!("native SFTP timed out at phase {}", self.phase));
                    event_loop.exit();
                    return;
                }
                self.windows.about_to_wait(event_loop);
                let host = self.windows.primary.window.as_ref().unwrap().clone();
                let native = self
                    .windows
                    .primary
                    .sftp_window
                    .as_ref()
                    .unwrap()
                    .window
                    .clone();
                match self.phase {
                    0 => {
                        assert_ne!(host.id(), native.id());
                        let _ = host.request_inner_size(winit::dpi::PhysicalSize::new(900, 600));
                        let _ =
                            native.request_inner_size(winit::dpi::LogicalSize::new(900.0, 500.0));
                        let origin = host.outer_position().unwrap();
                        native.set_outer_position(winit::dpi::PhysicalPosition::new(
                            origin.x + 180,
                            origin.y + 80,
                        ));
                        self.phase = 1;
                    }
                    1 if native.inner_size().width > host.inner_size().width
                        && native.inner_size().height > host.inner_size().height =>
                    {
                        assert_ne!(
                            native.outer_position().unwrap(),
                            host.outer_position().unwrap()
                        );
                        native.set_maximized(true);
                        self.phase = 2;
                    }
                    2 if native.is_maximized() => {
                        assert!(!host.is_maximized());
                        native.set_maximized(false);
                        self.phase = 3;
                    }
                    3 if !native.is_maximized() => {
                        self.windows
                            .primary
                            .sftp_window
                            .as_ref()
                            .unwrap()
                            .renderer
                            .as_ref()
                            .unwrap()
                            .simulate_device_loss();
                        native.request_redraw();
                        self.phase = 4;
                    }
                    4 if self
                        .windows
                        .primary
                        .sftp_window
                        .as_ref()
                        .unwrap()
                        .renderer
                        .is_none() =>
                    {
                        self.phase = 5;
                    }
                    5 if self
                        .windows
                        .primary
                        .sftp_window
                        .as_ref()
                        .unwrap()
                        .renderer
                        .is_some()
                        && !self
                            .windows
                            .primary
                            .sftp_window
                            .as_ref()
                            .unwrap()
                            .upload_fonts =>
                    {
                        // Route the same close event that a native title-bar X produces.
                        self.windows.window_event(
                            event_loop,
                            native.id(),
                            WindowEvent::CloseRequested,
                        );
                        assert!(self.windows.primary.sftp_window.is_none());
                        assert!(!self.windows.primary.sftp_panel.open);
                        let saved =
                            crate::sftp_settings::window().expect("normal geometry saved on close");
                        assert!(
                            !saved.maximized,
                            "maximizing must not overwrite the restored normal state"
                        );
                        let normal = native.inner_size().to_logical::<f64>(native.scale_factor());
                        assert!((saved.inner_size[0] - normal.width).abs() < 2.0);
                        assert!((saved.inner_size[1] - normal.height).abs() < 2.0);
                        assert!(self.windows.primary.window.is_some());
                        self.windows.primary.sftp_panel.open = true;
                        self.windows.primary.sync_sftp_window(event_loop);
                        let reopened = self.windows.primary.sftp_window.as_ref().unwrap();
                        assert_ne!(reopened.window.id(), native.id());
                        self.expected_placement = Some(reopened.placement.clone());
                        self.phase = 6;
                    }
                    6 if {
                        let expected = self.expected_placement.as_ref().unwrap();
                        let actual = native.inner_size().to_logical::<f64>(native.scale_factor());
                        (actual.width - expected.inner_size[0]).abs() < 2.0
                            && (actual.height - expected.inner_size[1]).abs() < 2.0
                    } =>
                    {
                        if let Some([x, y]) = self.expected_placement.as_ref().unwrap().position {
                            let actual = native.outer_position().unwrap();
                            assert!(
                                (i64::from(actual.x) - i64::from(x)).abs() <= 4
                                    && (i64::from(actual.y) - i64::from(y)).abs() <= 4,
                                "reopened window position: {actual:?}, expected {x},{y}"
                            );
                        }
                        self.windows.window_event(
                            event_loop,
                            host.id(),
                            WindowEvent::CloseRequested,
                        );
                        self.phase = 7;
                    }
                    _ => {}
                }
            }
        }
        let event_loop = EventLoop::<DesktopEvent>::with_user_event()
            .with_any_thread(true)
            .build()
            .unwrap();
        let app = DesktopApp {
            accesskit_proxy: Some(event_loop.create_proxy()),
            ..Default::default()
        };
        let session = SessionId::new();
        let mut check = Check {
            windows: DesktopWindows {
                primary: app,
                ..Default::default()
            },
            started: Instant::now(),
            phase: 0,
            failure: None,
            expected_placement: None,
            view: sftp_connection::SftpView {
                session_id: Some(session),
                local_directory: Some("C:/Users/operator".into()),
                remote_loaded_directory: Some("/home/operator".into()),
                queue: Some(Arc::new(cshell_application::SftpQueueSnapshot::initial(
                    session,
                ))),
                entries: vec![cshell_ipc::SftpDirectoryEntry {
                    name: "project".into(),
                    path: "/home/operator/project".into(),
                    kind: 2,
                    ..Default::default()
                }],
                ..Default::default()
            },
        };
        check.windows.primary.sftp_session = Some(session);
        event_loop.run_app(&mut check).unwrap();
        assert!(check.failure.is_none(), "{:?}", check.failure);
        assert_eq!(check.phase, 7);
        let weak = Arc::downgrade(&check.windows.primary.sftp_window.as_ref().unwrap().window);
        drop(check);
        assert!(
            weak.upgrade().is_none(),
            "SFTP window must not outlive the GUI"
        );
    }
}

impl DesktopApp {
    pub(super) fn sftp_profile_for_open(&self) -> Option<cshell_domain::ProfileId> {
        let selected = self.view_model.selected_profile;
        let active = self
            .active_terminal_session_id()
            .and_then(|id| self.daemon.as_ref()?.session_profile(id));
        let catalog = self.profiles.as_ref()?.view().catalog?;
        active
            .filter(|id| catalog.ssh_connections.iter().any(|c| c.profile_id == *id))
            .or_else(|| {
                selected.filter(|id| catalog.ssh_connections.iter().any(|c| c.profile_id == *id))
            })
    }
    pub(super) fn open_sftp_profile(&mut self,profile:cshell_domain::ProfileId) {self.open_sftp_tab(profile);}
    fn submit_sftp_command(&mut self, mut command:sftp_connection::SftpClientCommand) {
        if let sftp_connection::SftpClientCommand::ConnectProfile{profile,source_ssh_session,..}=&mut command {
            if source_ssh_session.is_none() {*source_ssh_session=self.sftp_ssh_source(*profile);}
        }
        let new_session=if let sftp_connection::SftpClientCommand::ConnectProfile{session,..}=&command {Some(*session)} else {None};
        if let Some(accepted)=self.sftp_tabs.local.submit(&command) {if !accepted {self.sftp_panel.request_failed(false);}return;}
        let transfer=matches!(&command,sftp_connection::SftpClientCommand::TransferBatch(..)|sftp_connection::SftpClientCommand::Tree(_,cshell_ipc::SftpOperation::TreeStart,..));
        if self.sftp.as_ref().is_some_and(|s|s.request(command)) {
            if let Some(session)=new_session {self.sftp_session=Some(session);self.sftp_panel.open=true;}
        }else{self.sftp_panel.request_failed(transfer);}
    }
    pub(super) fn sync_sftp_lifecycle(&mut self) {self.sync_sftp_tabs();}
}
