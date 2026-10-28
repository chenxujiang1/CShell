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
                terminal_tab_bypasses_egui(&tab, terminal_available, ui_wants_keyboard),
                terminal_available && !ui_wants_keyboard
            );
            assert!(!terminal_tab_bypasses_egui(
                &Key::Named(NamedKey::Space),
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
    assert!(!terminal_tab_bypasses_egui(
        &tab,
        true,
        context.egui_wants_keyboard_input()
    ));
}


#[test]
fn workbench_navigation_does_not_steal_shell_control_keys() {
    for letter in ["p","f","b","n","a","e","r","w"] {
        let key=Key::Character(letter.into());
        assert!(!workbench_navigation::shortcut(ElementState::Pressed,&key,ModifiersState::CONTROL,letter));
        let modes=TerminalModes::default();
        let encoded=action(&key,None,ModifiersState::CONTROL,modes,true,false,false).unwrap();
        assert_eq!(bytes(&encoded,modes),vec![letter.as_bytes()[0]-b'a'+1]);
    }
    for letter in ["p","f"] {
        let key=Key::Character(letter.into());
        assert!(workbench_navigation::shortcut(ElementState::Pressed,&key,ModifiersState::CONTROL|ModifiersState::SHIFT,letter));
        assert!(!workbench_navigation::shortcut(ElementState::Released,&key,ModifiersState::CONTROL|ModifiersState::SHIFT,letter));
        assert!(!workbench_navigation::shortcut(ElementState::Pressed,&key,ModifiersState::CONTROL|ModifiersState::SHIFT|ModifiersState::ALT,letter));
    }
}
