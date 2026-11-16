use super::*;
use cshell_domain::{ProfileId, ProfileKind};
use winit::keyboard::KeyCode as NativeKey;

fn offline_app() -> DesktopApp {
    let mut app = DesktopApp::default();
    app.window_active = true;
    app.workspace.writable = true;
    app.workspace_loaded = true;
    let profile = ProfileId::new();
    let tab = app
        .workspace
        .add(Some(profile), "Disconnected SSH".into())
        .unwrap();
    app.current_workspace_tab = Some(tab);
    app.view_model
        .saved_ssh_profiles
        .push(cshell_ui::SavedSshProfileViewModel {
            id: profile,
            name: "SSH".into(),
            target: "user@fixture:22".into(),
            favorite: false,
            folder_id: None,
            kind: ProfileKind::Ssh,
        });
    app.ssh_tab_states
        .lock()
        .unwrap()
        .insert(tab, SshConnectionState::Disconnected);
    app.refresh_ssh_controls();
    app
}

#[test]
fn text_enter_and_ime_process_are_gestures_but_copy_navigation_and_controls_are_not() {
    for value in 32u8..=126 {
        let key = Key::Character(char::from(value).to_string().into());
        assert!(activation(&key, ModifiersState::empty()));
        assert!(activation(&key, ModifiersState::SHIFT));
        for modifiers in [
            ModifiersState::CONTROL,
            ModifiersState::ALT,
            ModifiersState::SUPER,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
        ] {
            assert!(!activation(&key, modifiers));
        }
    }
    for key in [NamedKey::Enter, NamedKey::Space, NamedKey::Process] {
        assert!(activation(&Key::Named(key), ModifiersState::empty()));
    }
    for key in [
        NamedKey::Escape,
        NamedKey::PageUp,
        NamedKey::PageDown,
        NamedKey::ArrowDown,
        NamedKey::End,
        NamedKey::Tab,
        NamedKey::Backspace,
        NamedKey::F1,
        NamedKey::Shift,
    ] {
        assert!(!activation(&Key::Named(key), ModifiersState::empty()));
    }
    for text in ["", "\u{3}", "\r", "\n"] {
        assert!(!activation(
            &Key::Character(text.into()),
            ModifiersState::empty()
        ));
    }
    assert!(activation(
        &Key::Character("中文".into()),
        ModifiersState::empty()
    ));
}

#[test]
fn activation_repeats_and_release_are_fenced_even_after_a_fast_reconnect() {
    let physical = PhysicalKey::Code(NativeKey::Enter);
    let mut state = State::default();
    state.hold(physical);
    for _ in 0..20 {
        assert!(state.consume_held(physical, ElementState::Pressed, true));
    }
    assert!(!state.consume_held(
        PhysicalKey::Code(NativeKey::KeyA),
        ElementState::Pressed,
        false
    ));
    assert!(state.consume_held(physical, ElementState::Released, false));
    assert!(!state.consume_held(physical, ElementState::Pressed, false));
    state.hold(physical);
    assert!(
        !state.consume_held(physical, ElementState::Pressed, false),
        "a fresh press recovers a release lost while unfocused"
    );
    assert!(!state.consume_held(physical, ElementState::Released, false));
}

#[test]
fn editors_dialogs_inactive_local_and_unconfirmed_tabs_cannot_reconnect_by_typing() {
    for blocked in 0..12 {
        let mut app = offline_app();
        match blocked {
            0 => app.window_active = false,
            1 => app.view_model.about_open = true,
            2 => app.view_model.shortcut_help_open = true,
            3 => app.terminal_search_open = true,
            4 => app.resources.open = true,
            5 => app.profile_panel.open_new_ssh_profile(),
            6 => app.workspace.writable = false,
            7 => app.view_model.layout_paused = true,
            8 => app.view_model.saved_ssh_profiles[0].kind = ProfileKind::Local,
            9 => {
                let tab = app.current_workspace_tab.unwrap();
                app.ssh_tab_states
                    .lock()
                    .unwrap()
                    .insert(tab, SshConnectionState::Unconfirmed);
                app.refresh_ssh_controls();
            }
            10 => app.current_workspace_tab = None,
            11 => {
                let mut output = app.egui_context.run_ui(egui::RawInput::default(), |ui| {
                    ui.text_edit_singleline(&mut String::new()).request_focus();
                });
                output.textures_delta.clear();
                assert!(app.egui_context.egui_wants_keyboard_input());
            }
            _ => unreachable!(),
        }
        assert!(!app.reconnect_from_ime("中文"), "blocked context {blocked}");
        assert!(app.workspace.pending_launch.is_none());
        assert!(app.ssh_control.is_none());
        assert!(app.view_model.input_warning.is_none());
    }
}

#[test]
fn no_release_repeat_empty_or_control_commit_can_start_reconnect() {
    let mut app = offline_app();
    let key = Key::Named(NamedKey::Enter);
    for (pressed, repeated) in [(false, false), (true, true)] {
        assert!(!app.reconnect_from_key(
            DesktopKeyInput {
                key: &key,
                text: None,
                pressed,
                repeated
            },
            PhysicalKey::Code(NativeKey::Enter)
        ));
    }
    for text in ["", "\r", "\u{3}"] {
        assert!(!app.reconnect_from_ime(text));
    }
    assert!(app.view_model.input_warning.is_none());
}
