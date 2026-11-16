use super::*;
use cshell_terminal::{InputEncoder, TerminalModes};

fn terminal() -> Target {
    Target::Terminal {
        tab: Some(cshell_domain::TabId::new()),
        session: SessionId::new(),
        epoch: cshell_domain::ConnectionId::new(),
    }
}

#[test]
fn reconnect_discards_activation_and_late_commits_until_a_new_composition() {
    for composing in [false, true] {
        let mut state = State::default();
        if composing {
            state.event(terminal(), &Ime::Preedit("旧输入".into(), None));
        }
        state.discard_for_reconnect();
        let new = terminal();
        state.set_target(new);
        assert!(state.event(new, &Ime::Commit("旧输入".into())).1.is_none());
        state.event(new, &Ime::Preedit("新输入".into(), None));
        assert!(
            matches!(state.event(new, &Ime::Commit("新输入".into())).1, Some(InputAction::Text(text)) if text == "新输入")
        );
    }
}

#[test]
fn terminal_keeps_native_ime_enabled_in_every_platform_output() {
    let mut state = State::default();
    state.set_target(terminal());
    let rect = egui::Rect::from_min_size(egui::pos2(80.0, 100.0), egui::vec2(17.0, 32.0));
    for _ in 0..20 {
        let mut output = egui::PlatformOutput::default();
        state.output(&mut output, Some(rect));
        let ime = output.ime.unwrap();
        assert_eq!(ime.rect, rect);
        assert!(!ime.should_interrupt_composition);
    }
}

#[test]
fn editors_keep_their_own_candidate_geometry_and_purpose() {
    let mut state = State::default();
    state.set_target(Target::Editor(Some(egui::Id::new("editor"))));
    let rect = egui::Rect::from_min_size(egui::pos2(8.0, 10.0), egui::vec2(2.0, 20.0));
    let mut output = egui::PlatformOutput {
        ime: Some(egui::output::IMEOutput {
            purpose: egui::IMEPurpose::Password,
            rect,
            cursor_rect: rect,
            should_interrupt_composition: false,
        }),
        ..Default::default()
    };
    state.output(&mut output, Some(egui::Rect::EVERYTHING));
    let ime = output.ime.unwrap();
    assert_eq!(ime.rect, rect);
    assert_eq!(ime.purpose, egui::IMEPurpose::Password);
}

#[test]
fn preedit_is_local_and_only_confirmed_chinese_reaches_the_terminal() {
    let mut state = State::default();
    let target = terminal();
    for ime in [
        Ime::Enabled,
        Ime::Preedit("zhongwen".into(), Some((0, 8))),
        Ime::Preedit("中文".into(), Some((0, 6))),
        Ime::Preedit(String::new(), None),
    ] {
        let (handled, action) = state.event(target, &ime);
        assert!(handled);
        assert!(action.is_none());
    }
    let (_, action) = state.event(target, &Ime::Commit("中文 输入🙂".into()));
    let action = action.unwrap();
    for flags in 0..32 {
        let modes = TerminalModes {
            kitty_keyboard_flags: flags,
            modify_other_keys: 2,
            bracketed_paste: true,
            ..Default::default()
        };
        let request =
            cshell_ipc::TerminalInputRequest::from_action(SessionId::new(), &action).unwrap();
        let decoded = request.decode_action().unwrap();
        assert_eq!(
            InputEncoder::encode(&decoded, modes).unwrap(),
            "中文 输入🙂".as_bytes()
        );
    }
    assert!(state.event(target, &Ime::Disabled).1.is_none());
    assert!(state.preedit.is_empty());
}

#[test]
fn changing_tab_session_or_connection_fences_a_late_commit() {
    for target in [
        terminal(),
        Target::None,
        Target::Editor(Some(egui::Id::new("password"))),
    ] {
        let mut state = State::default();
        state.event(terminal(), &Ime::Preedit("中文".into(), None));
        state.event(state.target, &Ime::Preedit(String::new(), None));
        let (handled, action) = state.event(target, &Ime::Commit("中文".into()));
        assert!(handled && action.is_none());
        assert!(state.preedit.is_empty());
        state.event(target, &Ime::Enabled);
        state.event(target, &Ime::Preedit("新".into(), None));
        let (handled, action) = state.event(target, &Ime::Commit("新".into()));
        assert_eq!(action.is_some(), matches!(target, Target::Terminal { .. }));
        assert_eq!(handled, !matches!(target, Target::Editor(_)));
    }
}

#[test]
fn changing_editor_during_composition_does_not_leak_its_commit_to_a_shell() {
    let mut state = State::default();
    let editor = Target::Editor(Some(egui::Id::new("editor")));
    assert!(!state.event(editor, &Ime::Preedit("中文".into(), None)).0);
    let (handled, action) = state.event(terminal(), &Ime::Commit("中文".into()));
    assert!(handled && action.is_none());
}

#[test]
fn confirmation_keys_and_their_releases_do_not_execute_remote_commands() {
    let mut state = State::default();
    let target = terminal();
    let enter = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Enter);
    state.event(target, &Ime::Preedit("中文".into(), None));
    assert!(state.suppress_key(enter, ElementState::Pressed, false, false));
    state.event(target, &Ime::Commit("中文".into()));
    assert!(state.suppress_key(enter, ElementState::Released, false, false));
    assert!(!state.suppress_key(enter, ElementState::Pressed, false, false));
    state.event(target, &Ime::Preedit("中文".into(), None));
    assert!(!state.suppress_key(enter, ElementState::Pressed, false, true));
}

#[test]
fn preedit_cancellation_restores_normal_keyboard_input() {
    let mut state = State::default();
    let target = terminal();
    state.event(target, &Ime::Preedit("zhong".into(), None));
    state.event(target, &Ime::Preedit(String::new(), None));
    let key = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyA);
    assert!(!state.suppress_key(key, ElementState::Pressed, false, false));
}

#[test]
fn windows_process_key_before_preedit_does_not_leave_an_orphan_key_release() {
    let mut state = State::default();
    state.set_target(terminal());
    let key = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyZ);
    assert!(state.suppress_key(key, ElementState::Pressed, true, false));
    assert!(state.suppress_key(key, ElementState::Released, false, false));
    assert!(!state.suppress_key(key, ElementState::Pressed, false, false));
}

#[test]
fn candidate_position_uses_glyph_metrics_and_respects_dpi() {
    let mut snapshot = visual_corpus::snapshot();
    snapshot.cursor_col = 7;
    snapshot.cursor_row = 3;
    let rect = cursor_rect(
        TerminalViewport {
            x: 100,
            y: 200,
            width: 1000,
            height: 700,
        },
        [17.0, 32.0],
        &snapshot,
        2.0,
    );
    assert_eq!(rect.min, egui::pos2(109.5, 148.0));
    assert_eq!(rect.size(), egui::vec2(8.5, 16.0));
}

#[test]
fn zero_sized_pane_candidate_geometry_does_not_panic() {
    let rect = cursor_rect(
        TerminalViewport {
            x: 100,
            y: 200,
            width: 0,
            height: 0,
        },
        [17.0, 32.0],
        &visual_corpus::snapshot(),
        1.0,
    );
    assert_eq!(rect.min, egui::pos2(100.0, 200.0));
    assert!(rect.is_finite());
}

#[test]
fn preedit_is_bounded_on_utf8_boundaries_and_cleared_on_blur() {
    let mut state = State::default();
    state.event(terminal(), &Ime::Preedit("中".repeat(5000), None));
    assert!(!state.preedit.is_empty() && state.preedit.len() <= 4096);
    assert!(state.preedit.chars().all(|c| c == '中'));
    state.cancel();
    assert!(state.preedit.is_empty());
}

#[test]
fn terminal_preedit_has_visible_shapes_without_focusing_an_editor() {
    let ctx = egui::Context::default();
    let mut state = State::default();
    state.event(terminal(), &Ime::Preedit("中文".into(), None));
    let clip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(clip),
            ..Default::default()
        },
        |ui| {
            state.paint(
                ui.ctx(),
                Some(egui::Rect::from_min_size(
                    egui::pos2(50.0, 60.0),
                    egui::vec2(17.0, 32.0),
                )),
                clip,
            );
        },
    );
    assert!(!output.shapes.is_empty());
    assert!(!ctx.egui_wants_keyboard_input());
    output.textures_delta.clear();
}

#[cfg(windows)]
#[test]
#[ignore = "requires a native Windows event loop; run explicitly"]
fn native_windows_ime_owner_stays_enabled_across_terminal_redraws() {
    use winit::{
        application::ApplicationHandler,
        event_loop::{ActiveEventLoop, EventLoop},
        platform::windows::EventLoopBuilderExtWindows,
    };
    #[derive(Default)]
    struct Probe {
        passed: bool,
    }
    impl ApplicationHandler for Probe {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let window = event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("CShell isolated IME probe")
                        .with_visible(false)
                        .with_active(false),
                )
                .unwrap();
            let ctx = egui::Context::default();
            let mut integration = egui_winit::State::new(
                ctx,
                egui::ViewportId::ROOT,
                &window,
                Some(window.scale_factor() as f32),
                window.theme(),
                None,
            );
            // The old out-of-band enable left egui's cached state false.
            integration.handle_platform_output_with_event_loop(
                &window,
                event_loop,
                egui::PlatformOutput::default(),
            );
            window.set_ime_allowed(true);
            assert!(!integration.allow_ime());
            let mut state = State::default();
            let target = terminal();
            state.event(target, &Ime::Preedit("中文".into(), None));
            for _ in 0..20 {
                let mut output = egui::PlatformOutput::default();
                state.output(
                    &mut output,
                    Some(egui::Rect::from_min_size(
                        egui::pos2(30.0, 50.0),
                        egui::vec2(17.0, 32.0),
                    )),
                );
                integration.handle_platform_output_with_event_loop(&window, event_loop, output);
                assert!(integration.allow_ime());
                assert_eq!(state.preedit, "中文");
            }
            disable(&window, Some(&mut integration));
            assert!(!integration.allow_ime());
            self.passed = true;
            event_loop.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    let event_loop = EventLoop::builder().with_any_thread(true).build().unwrap();
    let mut probe = Probe::default();
    event_loop.run_app(&mut probe).unwrap();
    assert!(probe.passed);
}
