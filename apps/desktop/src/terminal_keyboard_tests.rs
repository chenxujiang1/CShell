// Test fixtures deliberately panic on failed setup and mutate defaults for individual cases.
//! Check desktop logical keys through the real IPC codec and terminal encoder.
use super::*;
use cshell_ipc::TerminalInputRequest;
use cshell_terminal::{InputEncoder, TerminalModes};

fn action(
    key: &Key,
    text: Option<&str>,
    modifiers: ModifiersState,
    modes: TerminalModes,
    pressed: bool,
    repeated: bool,
    broadcast: bool,
) -> Option<InputAction> {
    desktop_key_action(
        DesktopKeyInput {
            key,
            text,
            pressed,
            repeated,
        },
        modifiers,
        modes,
        broadcast,
    )
}
fn bytes(action: &InputAction, modes: TerminalModes) -> Vec<u8> {
    use prost::Message;
    let request = TerminalInputRequest::from_action(SessionId::new(), action).unwrap();
    let request = TerminalInputRequest::decode(request.encode_to_vec().as_slice()).unwrap();
    let decoded = request.decode_action().unwrap();
    assert_eq!(&decoded, action);
    InputEncoder::encode(&decoded, modes).unwrap()
}

#[test]
fn all_95_printable_ascii_and_unicode_text_survive_desktop_ipc_and_terminal_encoding() {
    let modes = TerminalModes::default();
    for value in 32u8..=126 {
        let text = char::from(value).to_string();
        let keys = if value == b' ' {
            vec![
                Key::Named(NamedKey::Space),
                Key::Character(text.clone().into()),
            ]
        } else {
            vec![Key::Character(text.clone().into())]
        };
        for key in keys {
            for modifiers in [ModifiersState::empty(), ModifiersState::SHIFT] {
                let a = action(&key, Some(&text), modifiers, modes, true, false, false).unwrap();
                assert_eq!(bytes(&a, modes), text.as_bytes(), "{key:?} {modifiers:?}");
            }
        }
    }
    for text in ["中文", "é", "e\u{301}", "€", "🙂", "a b  c", "中文 输入"] {
        let a = action(
            &Key::Character(text.into()),
            Some(text),
            ModifiersState::empty(),
            modes,
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(bytes(&a, modes), text.as_bytes());
        // IME commits use the Text action directly.
        assert_eq!(
            bytes(&InputAction::Text(text.into()), modes),
            text.as_bytes()
        );
    }
}

#[test]
fn space_preserves_control_alt_repeats_and_keyboard_protocol_events() {
    let key = Key::Named(NamedKey::Space);
    let legacy = TerminalModes::default();
    for (modifiers, expected) in [
        (ModifiersState::empty(), b" ".as_slice()),
        (ModifiersState::SHIFT, b" "),
        (ModifiersState::CONTROL, b"\0"),
        (ModifiersState::ALT, b"\x1b "),
        (ModifiersState::CONTROL | ModifiersState::ALT, b"\x1b\0"),
    ] {
        for text in [Some(" "), None] {
            let a = action(&key, text, modifiers, legacy, true, true, false).unwrap();
            assert_eq!(bytes(&a, legacy), expected);
        }
    }
    assert!(
        action(
            &key,
            None,
            ModifiersState::empty(),
            legacy,
            false,
            false,
            false
        )
        .is_none()
    );
    let kitty = TerminalModes {
        kitty_keyboard_flags: TerminalModes::KITTY_DISAMBIGUATE
            | TerminalModes::KITTY_REPORT_ALL_KEYS
            | TerminalModes::KITTY_REPORT_EVENTS,
        ..legacy
    };
    for (pressed, repeated, expected) in [
        (true, false, b"\x1b[32;1u".as_slice()),
        (true, true, b"\x1b[32;1:2u"),
        (false, false, b"\x1b[32;1:3u"),
    ] {
        let a = action(
            &key,
            None,
            ModifiersState::empty(),
            kitty,
            pressed,
            repeated,
            false,
        )
        .unwrap();
        assert_eq!(bytes(&a, kitty), expected);
    }
    let broadcast = action(
        &key,
        None,
        ModifiersState::empty(),
        legacy,
        false,
        false,
        true,
    )
    .unwrap();
    assert_eq!(bytes(&broadcast, legacy), b"");
    assert_eq!(bytes(&broadcast, kitty), b"\x1b[32;1:3u");
    let xterm = TerminalModes {
        modify_other_keys: 2,
        ..legacy
    };
    let a = action(
        &key,
        None,
        ModifiersState::CONTROL,
        xterm,
        true,
        false,
        false,
    )
    .unwrap();
    assert_eq!(bytes(&a, xterm), b"\x1b[27;5;32~");
}

#[test]
fn navigation_and_editing_keys_survive_ipc_in_legacy_application_and_kitty_modes() {
    let legacy = TerminalModes::default();
    for (key, normal, application, ctrl, kitty_release) in [
        (
            NamedKey::Home,
            "\x1b[H",
            "\x1bOH",
            "\x1b[1;5H",
            "\x1b[1;1:3H",
        ),
        (
            NamedKey::End,
            "\x1b[F",
            "\x1bOF",
            "\x1b[1;5F",
            "\x1b[1;1:3F",
        ),
        (
            NamedKey::Insert,
            "\x1b[2~",
            "\x1b[2~",
            "\x1b[2;5~",
            "\x1b[2;1:3~",
        ),
        (
            NamedKey::Delete,
            "\x1b[3~",
            "\x1b[3~",
            "\x1b[3;5~",
            "\x1b[3;1:3~",
        ),
        (
            NamedKey::PageUp,
            "\x1b[5~",
            "\x1b[5~",
            "\x1b[5;5~",
            "\x1b[5;1:3~",
        ),
        (
            NamedKey::PageDown,
            "\x1b[6~",
            "\x1b[6~",
            "\x1b[6;5~",
            "\x1b[6;1:3~",
        ),
    ] {
        let key = Key::Named(key);
        for (modes, modifiers, pressed, expected) in [
            (legacy, ModifiersState::empty(), true, normal),
            (
                TerminalModes {
                    application_cursor: true,
                    ..legacy
                },
                ModifiersState::empty(),
                true,
                application,
            ),
            (legacy, ModifiersState::CONTROL, true, ctrl),
            (
                TerminalModes {
                    kitty_keyboard_flags: TerminalModes::KITTY_DISAMBIGUATE
                        | TerminalModes::KITTY_REPORT_EVENTS,
                    ..legacy
                },
                ModifiersState::empty(),
                false,
                kitty_release,
            ),
        ] {
            let a = action(&key, None, modifiers, modes, pressed, false, false).unwrap();
            assert_eq!(bytes(&a, modes), expected.as_bytes(), "{key:?} {modes:?}");
        }
    }
}

#[test]
fn existing_enter_tab_backspace_arrows_functions_controls_and_dead_keys_are_preserved() {
    let modes = TerminalModes::default();
    for (key, expected) in [
        (NamedKey::Enter, b"\r".as_slice()),
        (NamedKey::Tab, b"\t"),
        (NamedKey::Backspace, b"\x7f"),
        (NamedKey::Escape, b"\x1b"),
        (NamedKey::ArrowUp, b"\x1b[A"),
        (NamedKey::ArrowDown, b"\x1b[B"),
        (NamedKey::ArrowLeft, b"\x1b[D"),
        (NamedKey::ArrowRight, b"\x1b[C"),
        (NamedKey::F1, b"\x1bOP"),
        (NamedKey::F12, b"\x1b[24~"),
    ] {
        let a = action(
            &Key::Named(key),
            None,
            ModifiersState::empty(),
            modes,
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(bytes(&a, modes), expected);
    }
    for value in b'a'..=b'z' {
        let text = char::from(value).to_string();
        let a = action(
            &Key::Character(text.into()),
            None,
            ModifiersState::CONTROL,
            modes,
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(bytes(&a, modes), vec![value - b'a' + 1]);
    }
    let a = action(
        &Key::Named(NamedKey::Tab),
        None,
        ModifiersState::SHIFT,
        modes,
        true,
        false,
        false,
    )
    .unwrap();
    assert_eq!(bytes(&a, modes), b"\x1b[Z");
    assert!(
        action(
            &Key::Dead(Some('´')),
            None,
            ModifiersState::empty(),
            modes,
            true,
            false,
            false
        )
        .is_none()
    );
    assert!(
        action(
            &Key::Named(NamedKey::Shift),
            None,
            ModifiersState::SHIFT,
            modes,
            true,
            false,
            false
        )
        .is_none()
    );
}

#[test]
fn terminal_tab_bypasses_ui_navigation_but_form_and_search_focus_keep_tab() {
    let tab = Key::Named(NamedKey::Tab);
    for terminal_available in [true, false] {
        for ui_wants_keyboard in [true, false] {
            assert_eq!(
                terminal_key_bypasses_egui(
                    &tab,
                    ModifiersState::empty(),
                    terminal_available,
                    ui_wants_keyboard
                ),
                terminal_available && !ui_wants_keyboard
            );
            assert!(!terminal_key_bypasses_egui(
                &Key::Named(NamedKey::Space),
                ModifiersState::empty(),
                terminal_available,
                ui_wants_keyboard
            ));
        }
    }
    let context = crate::ui_layout_gate::context();
    let mut text = String::new();
    let mut output = context.run_ui(Default::default(), |ui| {
        ui.text_edit_singleline(&mut text).request_focus();
    });
    output.textures_delta.clear();
    assert!(context.egui_wants_keyboard_input());
    assert!(!terminal_key_bypasses_egui(
        &tab,
        ModifiersState::empty(),
        true,
        context.egui_wants_keyboard_input()
    ));
}

#[test]
fn ctrl_break_is_an_explicit_interrupt_and_ctrl_c_still_obeys_negotiation() {
    for key in [Key::Named(NamedKey::Pause), Key::Named(NamedKey::Cancel)] {
        assert!(terminal_key_bypasses_egui(
            &key,
            ModifiersState::CONTROL,
            true,
            false
        ));
        for flags in 0..32 {
            let modes = TerminalModes {
                kitty_keyboard_flags: flags,
                modify_other_keys: 2,
                ..Default::default()
            };
            let sent = action(
                &key,
                None,
                ModifiersState::CONTROL,
                modes,
                true,
                false,
                false,
            )
            .unwrap();
            assert_eq!(bytes(&sent, modes), [3]);
            assert!(
                action(
                    &key,
                    None,
                    ModifiersState::CONTROL,
                    modes,
                    false,
                    false,
                    false
                )
                .is_none()
            );
        }
        assert!(!is_terminal_force_interrupt_key(
            &key,
            ModifiersState::empty()
        ));
        assert!(!is_terminal_force_interrupt_key(
            &key,
            ModifiersState::CONTROL | ModifiersState::SHIFT
        ));
    }
    let modes = TerminalModes {
        kitty_keyboard_flags: TerminalModes::KITTY_DISAMBIGUATE,
        ..Default::default()
    };
    let sent = action(
        &Key::Character("c".into()),
        None,
        ModifiersState::CONTROL,
        modes,
        true,
        false,
        false,
    )
    .unwrap();
    assert_eq!(bytes(&sent, modes), b"\x1b[99;5u");
}

#[test]
fn control_chords_bypass_ui_clipboard_only_when_terminal_owns_keyboard() {
    for letter in 'a'..='z' {
        let key = Key::Character(letter.to_string().into());
        for modifiers in [
            ModifiersState::CONTROL,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
        ] {
            assert!(terminal_key_bypasses_egui(&key, modifiers, true, false));
            assert!(!terminal_key_bypasses_egui(&key, modifiers, true, true));
            assert!(!terminal_key_bypasses_egui(&key, modifiers, false, false));
        }
        assert!(!terminal_key_bypasses_egui(
            &key,
            ModifiersState::empty(),
            true,
            false
        ));
        // A remote application gets its control character, never a paste or a
        // clipboard command; Ctrl+Shift+C/V remain separate desktop shortcuts.
        let a = action(
            &key,
            None,
            ModifiersState::CONTROL,
            TerminalModes::default(),
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            bytes(&a, TerminalModes::default()),
            vec![letter as u8 - b'a' + 1]
        );
    }
    for key in [Key::Named(NamedKey::Process), Key::Dead(Some('´'))] {
        assert!(!terminal_key_bypasses_egui(
            &key,
            ModifiersState::CONTROL,
            true,
            false
        ));
    }
    for letter in ["c", "v"] {
        let key = Key::Character(letter.into());
        assert!(!matches_terminal_copy_shortcut(
            ElementState::Pressed,
            false,
            &key,
            ModifiersState::CONTROL
        ));
        assert!(!matches_terminal_paste_shortcut(
            ElementState::Pressed,
            false,
            &key,
            ModifiersState::CONTROL
        ));
    }
}

#[test]
fn workbench_navigation_does_not_steal_shell_control_keys() {
    for letter in ["p", "f", "b", "n", "a", "e", "r", "w"] {
        let key = Key::Character(letter.into());
        assert!(!workbench_navigation::shortcut(
            ElementState::Pressed,
            &key,
            ModifiersState::CONTROL,
            letter
        ));
        let modes = TerminalModes::default();
        let encoded = action(
            &key,
            None,
            ModifiersState::CONTROL,
            modes,
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            bytes(&encoded, modes),
            vec![letter.as_bytes()[0] - b'a' + 1]
        );
    }
    for letter in ["p", "f"] {
        let key = Key::Character(letter.into());
        assert!(workbench_navigation::shortcut(
            ElementState::Pressed,
            &key,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            letter
        ));
        assert!(!workbench_navigation::shortcut(
            ElementState::Released,
            &key,
            ModifiersState::CONTROL | ModifiersState::SHIFT,
            letter
        ));
        assert!(!workbench_navigation::shortcut(
            ElementState::Pressed,
            &key,
            ModifiersState::CONTROL | ModifiersState::SHIFT | ModifiersState::ALT,
            letter
        ));
    }
}

#[cfg(windows)]
#[test]
#[ignore = "requires Ubuntu WSL with nano; explicit real Windows PTY editor gate"]
fn windows_pty_nano_receives_help_cancel_and_exit_controls() {
    use cshell_domain::TerminalSize;
    use cshell_terminal::TerminalEngine;
    use std::{collections::BTreeMap, io::Read, time::Duration};
    let profile = cshell_local::LocalProfile {
        name: "isolated nano control gate".into(),
        program: "wsl.exe".into(),
        args: [
            "-d",
            "Ubuntu",
            "-u",
            "root",
            "--exec",
            "/usr/bin/env",
            "LC_ALL=C",
            "TERM=xterm-256color",
            "/usr/bin/nano",
            "--ignorercfiles",
            "--restricted",
        ]
        .map(str::to_owned)
        .to_vec(),
        cwd_policy: cshell_local::WorkingDirectoryPolicy::Inherit,
        env_overrides: BTreeMap::new(),
    };
    let size = TerminalSize::cells(24, 100);
    let mut pty = cshell_local::PtySession::spawn(&profile, size).unwrap();
    let mut reader = pty.take_reader().unwrap();
    let (sent, received) = std::sync::mpsc::sync_channel(16);
    std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || sent.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut terminal = cshell_terminal::AlacrittyTerminalEngine::new(size);
    let screen = |terminal: &cshell_terminal::AlacrittyTerminalEngine| {
        terminal
            .snapshot()
            .cells
            .iter()
            .flat_map(cshell_terminal::Cell::characters)
            .collect::<String>()
    };
    let wait = |terminal: &mut cshell_terminal::AlacrittyTerminalEngine,
                pty: &mut cshell_local::PtySession,
                wanted: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !screen(terminal)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains(wanted)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "nano missing {wanted}: {}",
                screen(terminal)
            );
            if let Ok(data) = received.recv_timeout(Duration::from_millis(50)) {
                for response in terminal.feed(&data).terminal_responses {
                    pty.write_all(&response).unwrap();
                }
            }
        }
    };
    let control = |letter: &str, terminal: &cshell_terminal::AlacrittyTerminalEngine| {
        let key = Key::Character(letter.into());
        assert!(terminal_key_bypasses_egui(
            &key,
            ModifiersState::CONTROL,
            true,
            false
        ));
        let modes = terminal.modes();
        bytes(
            &action(
                &key,
                None,
                ModifiersState::CONTROL,
                modes,
                true,
                false,
                false,
            )
            .unwrap(),
            modes,
        )
    };
    wait(&mut terminal, &mut pty, "GNU nano");
    pty.write_all(&control("g", &terminal)).unwrap();
    wait(&mut terminal, &mut pty, "Main nano help text");
    pty.write_all(&control("x", &terminal)).unwrap();
    wait(&mut terminal, &mut pty, "New Buffer");
    assert!(!screen(&terminal).contains("Main nano help text"));
    pty.write_all(&control("c", &terminal)).unwrap();
    wait(&mut terminal, &mut pty, "line 1/1");
    pty.write_all(&control("x", &terminal)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = pty.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Ctrl+X did not exit nano"
        );
        // Drain ConPTY output so shutdown cannot wait on a full pipe.
        if let Ok(data) = received.recv_timeout(Duration::from_millis(25)) {
            terminal.feed(&data);
        }
    }
}
