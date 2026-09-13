use cshell_domain::{ControlAction, InputAction, KeyCode, KeyEvent, Modifiers};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalModes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    /// Kitty keyboard protocol progressive-enhancement flags.
    pub kitty_keyboard_flags: u8,
    /// xterm modifyOtherKeys level (0-3).
    pub modify_other_keys: u8,
    /// Use xterm's CSI-u formatOtherKeys representation.
    pub format_other_keys: bool,
}

impl TerminalModes {
    pub const KITTY_DISAMBIGUATE: u8 = 1;
    pub const KITTY_REPORT_EVENTS: u8 = 1 << 1;
    pub const KITTY_REPORT_ALTERNATE_KEYS: u8 = 1 << 2;
    pub const KITTY_REPORT_ALL_KEYS: u8 = 1 << 3;
    pub const KITTY_REPORT_ASSOCIATED_TEXT: u8 = 1 << 4;

    #[must_use]
    pub const fn kitty_enabled(self, flag: u8) -> bool {
        self.kitty_keyboard_flags & flag != 0
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InputEncoder;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum InputEncodeError {
    #[error("function key F{0} is outside the supported F1-F24 range")]
    UnsupportedFunctionKey(u8),
    #[error("input action is not supported by this encoder version")]
    UnsupportedInputAction,
    #[error("key code is not supported by this encoder version")]
    UnsupportedKeyCode,
}

impl InputEncoder {
    pub fn encode(action: &InputAction, modes: TerminalModes) -> Result<Vec<u8>, InputEncodeError> {
        match action {
            InputAction::Text(text) => Ok(text.as_bytes().to_vec()),
            InputAction::Paste { text, bracketed } => {
                if *bracketed && modes.bracketed_paste {
                    let mut bytes = Vec::with_capacity(text.len() + 12);
                    bytes.extend_from_slice(b"\x1b[200~");
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.extend_from_slice(b"\x1b[201~");
                    Ok(bytes)
                } else {
                    Ok(text.as_bytes().to_vec())
                }
            }
            InputAction::Control(control) => Ok(vec![match control {
                ControlAction::Interrupt => 0x03,
                ControlAction::EndOfFile => 0x04,
                ControlAction::Suspend => 0x1a,
                ControlAction::ClearScreen => 0x0c,
            }]),
            InputAction::Key(event) => encode_key(event, modes),
            _ => Err(InputEncodeError::UnsupportedInputAction),
        }
    }
}

fn encode_key(event: &KeyEvent, modes: TerminalModes) -> Result<Vec<u8>, InputEncodeError> {
    let report_events = modes.kitty_enabled(TerminalModes::KITTY_REPORT_EVENTS);
    if !event.pressed && !report_events {
        return Ok(Vec::new());
    }

    if let Some(encoded) = encode_kitty_key(event, modes)? {
        return Ok(encoded);
    }
    if !event.pressed {
        return Ok(Vec::new());
    }
    if let Some(encoded) = encode_xterm_other_key(event, modes) {
        return Ok(encoded);
    }

    match &event.code {
        KeyCode::Character(text) => Ok(encode_character(text, event.modifiers)),
        KeyCode::Enter => Ok(with_alt_prefix(b"\r", event.modifiers)),
        KeyCode::Tab if event.modifiers.shift => Ok(b"\x1b[Z".to_vec()),
        KeyCode::Tab => Ok(with_alt_prefix(b"\t", event.modifiers)),
        KeyCode::Backspace => Ok(with_alt_prefix(&[0x7f], event.modifiers)),
        KeyCode::Escape => Ok(vec![0x1b]),
        KeyCode::ArrowUp => Ok(encode_cursor(b'A', event.modifiers, modes)),
        KeyCode::ArrowDown => Ok(encode_cursor(b'B', event.modifiers, modes)),
        KeyCode::ArrowRight => Ok(encode_cursor(b'C', event.modifiers, modes)),
        KeyCode::ArrowLeft => Ok(encode_cursor(b'D', event.modifiers, modes)),
        KeyCode::Function(number) => encode_function(*number, event.modifiers),
        _ => Err(InputEncodeError::UnsupportedKeyCode),
    }
}

fn encode_xterm_other_key(event: &KeyEvent, modes: TerminalModes) -> Option<Vec<u8>> {
    let level = modes.modify_other_keys.min(3);
    let applies = match level {
        0 => false,
        1 => event.modifiers.alt || event.modifiers.super_key,
        2 => event.modifiers != Modifiers::default(),
        _ => true,
    };
    if !applies {
        return None;
    }
    let code = match &event.code {
        KeyCode::Character(text) => u32::from(text.chars().next()?),
        KeyCode::Tab => 9,
        _ => return None,
    };
    let modifier = xterm_modifier(event.modifiers);
    if modes.format_other_keys {
        Some(format!("\x1b[{code};{modifier}u").into_bytes())
    } else {
        Some(format!("\x1b[27;{modifier};{code}~").into_bytes())
    }
}

fn encode_kitty_key(
    event: &KeyEvent,
    modes: TerminalModes,
) -> Result<Option<Vec<u8>>, InputEncodeError> {
    let disambiguate = modes.kitty_enabled(TerminalModes::KITTY_DISAMBIGUATE)
        || modes.kitty_enabled(TerminalModes::KITTY_REPORT_ALL_KEYS);
    if !disambiguate {
        return Ok(None);
    }

    let report_all = modes.kitty_enabled(TerminalModes::KITTY_REPORT_ALL_KEYS);
    let modified_text_key = event.modifiers.ctrl
        || event.modifiers.alt
        || event.modifiers.super_key
        || (event.modifiers.shift && event.modifiers.alt);
    match &event.code {
        KeyCode::Character(text) if report_all || modified_text_key => {
            let character = text
                .chars()
                .next()
                .ok_or(InputEncodeError::UnsupportedKeyCode)?;
            let primary = if event.modifiers.shift && character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                character
            };
            let mut sequence = format!(
                "\x1b[{};{}",
                u32::from(primary),
                kitty_modifier_parameter(event, modes)
            );
            if event.pressed
                && report_all
                && modes.kitty_enabled(TerminalModes::KITTY_REPORT_ASSOCIATED_TEXT)
            {
                sequence.push(';');
                for (index, character) in text.chars().enumerate() {
                    if index > 0 {
                        sequence.push(':');
                    }
                    sequence.push_str(&u32::from(character).to_string());
                }
            }
            sequence.push('u');
            Ok(Some(sequence.into_bytes()))
        }
        KeyCode::Escape => Ok(Some(
            format!("\x1b[27;{}u", kitty_modifier_parameter(event, modes)).into_bytes(),
        )),
        KeyCode::Enter if report_all => Ok(Some(
            format!("\x1b[13;{}u", kitty_modifier_parameter(event, modes)).into_bytes(),
        )),
        KeyCode::Tab if report_all => Ok(Some(
            format!("\x1b[9;{}u", kitty_modifier_parameter(event, modes)).into_bytes(),
        )),
        KeyCode::Backspace if report_all => Ok(Some(
            format!("\x1b[127;{}u", kitty_modifier_parameter(event, modes)).into_bytes(),
        )),
        KeyCode::ArrowUp => Ok(Some(encode_kitty_functional('A', event, modes))),
        KeyCode::ArrowDown => Ok(Some(encode_kitty_functional('B', event, modes))),
        KeyCode::ArrowRight => Ok(Some(encode_kitty_functional('C', event, modes))),
        KeyCode::ArrowLeft => Ok(Some(encode_kitty_functional('D', event, modes))),
        KeyCode::Function(number) => Ok(Some(encode_kitty_function(*number, event, modes)?)),
        _ => Ok(None),
    }
}

fn kitty_modifier_parameter(event: &KeyEvent, modes: TerminalModes) -> String {
    let modifier = xterm_modifier(event.modifiers);
    if modes.kitty_enabled(TerminalModes::KITTY_REPORT_EVENTS) && event.repeated {
        format!("{modifier}:2")
    } else if !event.pressed {
        format!("{modifier}:3")
    } else {
        modifier.to_string()
    }
}

fn encode_kitty_functional(
    final_character: char,
    event: &KeyEvent,
    modes: TerminalModes,
) -> Vec<u8> {
    format!(
        "\x1b[1;{}{}",
        kitty_modifier_parameter(event, modes),
        final_character
    )
    .into_bytes()
}

fn encode_kitty_function(
    number: u8,
    event: &KeyEvent,
    modes: TerminalModes,
) -> Result<Vec<u8>, InputEncodeError> {
    match number {
        1..=4 => Ok(encode_kitty_functional(
            char::from(b'P' + number - 1),
            event,
            modes,
        )),
        5..=24 => {
            let legacy = encode_function(number, Modifiers::default())?;
            let code = std::str::from_utf8(&legacy[2..legacy.len() - 1])
                .map_err(|_| InputEncodeError::UnsupportedKeyCode)?;
            Ok(format!("\x1b[{code};{}~", kitty_modifier_parameter(event, modes)).into_bytes())
        }
        _ => Err(InputEncodeError::UnsupportedFunctionKey(number)),
    }
}

fn encode_character(text: &str, modifiers: Modifiers) -> Vec<u8> {
    let mut bytes = if modifiers.ctrl {
        text.chars()
            .next()
            .and_then(control_character)
            .map_or_else(|| text.as_bytes().to_vec(), |byte| vec![byte])
    } else {
        text.as_bytes().to_vec()
    };
    if modifiers.alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn control_character(character: char) -> Option<u8> {
    match character {
        '@' | ' ' => Some(0x00),
        'a'..='z' => Some(character as u8 - b'a' + 1),
        'A'..='Z' => Some(character as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn encode_cursor(final_byte: u8, modifiers: Modifiers, modes: TerminalModes) -> Vec<u8> {
    let modifier = xterm_modifier(modifiers);
    if modifier > 1 {
        return format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes();
    }
    if modes.application_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

fn encode_function(number: u8, modifiers: Modifiers) -> Result<Vec<u8>, InputEncodeError> {
    let modifier = xterm_modifier(modifiers);
    let bytes = match number {
        1..=4 => {
            let final_byte = b'P' + number - 1;
            if modifier == 1 {
                vec![0x1b, b'O', final_byte]
            } else {
                format!("\x1b[1;{modifier}{}", char::from(final_byte)).into_bytes()
            }
        }
        5..=24 => {
            let code = match number {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                13 => 25,
                14 => 26,
                15 => 28,
                16 => 29,
                17 => 31,
                18 => 32,
                19 => 33,
                20 => 34,
                21 => 42,
                22 => 43,
                23 => 44,
                24 => 45,
                _ => unreachable!(),
            };
            if modifier == 1 {
                format!("\x1b[{code}~").into_bytes()
            } else {
                format!("\x1b[{code};{modifier}~").into_bytes()
            }
        }
        _ => return Err(InputEncodeError::UnsupportedFunctionKey(number)),
    };
    Ok(bytes)
}

const fn xterm_modifier(modifiers: Modifiers) -> u8 {
    1 + modifiers.shift as u8
        + (modifiers.alt as u8) * 2
        + (modifiers.ctrl as u8) * 4
        + (modifiers.super_key as u8) * 8
}

fn with_alt_prefix(bytes: &[u8], modifiers: Modifiers) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(bytes.len() + usize::from(modifiers.alt));
    if modifiers.alt {
        encoded.push(0x1b);
    }
    encoded.extend_from_slice(bytes);
    encoded
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{InputEncoder, TerminalModes};
    use cshell_domain::{InputAction, KeyCode, KeyEvent, Modifiers};

    fn key(code: KeyCode, modifiers: Modifiers) -> InputAction {
        InputAction::Key(KeyEvent {
            code,
            modifiers,
            pressed: true,
            repeated: false,
        })
    }

    #[test]
    fn application_cursor_and_xterm_modifiers_are_encoded() {
        let application = TerminalModes {
            application_cursor: true,
            ..TerminalModes::default()
        };
        assert_eq!(
            InputEncoder::encode(&key(KeyCode::ArrowUp, Modifiers::default()), application)
                .unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            InputEncoder::encode(
                &key(
                    KeyCode::ArrowUp,
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::default()
                    }
                ),
                application
            )
            .unwrap(),
            b"\x1b[1;5A"
        );
    }

    #[test]
    fn paste_is_bracketed_only_when_policy_and_terminal_mode_allow_it() {
        let action = InputAction::Paste {
            text: "one\ntwo".to_owned(),
            bracketed: true,
        };
        assert_eq!(
            InputEncoder::encode(
                &action,
                TerminalModes {
                    bracketed_paste: true,
                    ..TerminalModes::default()
                }
            )
            .unwrap(),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
        assert_eq!(
            InputEncoder::encode(&action, TerminalModes::default()).unwrap(),
            b"one\ntwo"
        );
    }

    #[test]
    fn control_and_alt_character_preserve_terminal_semantics() {
        assert_eq!(
            InputEncoder::encode(
                &key(
                    KeyCode::Character("c".to_owned()),
                    Modifiers {
                        ctrl: true,
                        alt: true,
                        ..Modifiers::default()
                    }
                ),
                TerminalModes::default()
            )
            .unwrap(),
            [0x1b, 0x03]
        );
    }

    #[test]
    fn encodes_function_keys_and_ignores_legacy_key_release() {
        assert_eq!(
            InputEncoder::encode(
                &key(KeyCode::Function(5), Modifiers::default()),
                TerminalModes::default()
            )
            .unwrap(),
            b"\x1b[15~"
        );
        let release = InputAction::Key(KeyEvent {
            code: KeyCode::ArrowLeft,
            modifiers: Modifiers::default(),
            pressed: false,
            repeated: false,
        });
        assert!(
            InputEncoder::encode(&release, TerminalModes::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn kitty_protocol_disambiguates_modified_text_and_escape() {
        let modes = TerminalModes {
            kitty_keyboard_flags: TerminalModes::KITTY_DISAMBIGUATE,
            ..TerminalModes::default()
        };
        assert_eq!(
            InputEncoder::encode(
                &key(
                    KeyCode::Character("c".to_owned()),
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::default()
                    }
                ),
                modes
            )
            .unwrap(),
            b"\x1b[99;5u"
        );
        assert_eq!(
            InputEncoder::encode(&key(KeyCode::Escape, Modifiers::default()), modes).unwrap(),
            b"\x1b[27;1u"
        );
        assert_eq!(
            InputEncoder::encode(&key(KeyCode::Enter, Modifiers::default()), modes).unwrap(),
            b"\r"
        );
    }

    #[test]
    fn kitty_protocol_reports_all_text_event_types_and_associated_text() {
        let modes = TerminalModes {
            kitty_keyboard_flags: TerminalModes::KITTY_REPORT_ALL_KEYS
                | TerminalModes::KITTY_REPORT_EVENTS
                | TerminalModes::KITTY_REPORT_ASSOCIATED_TEXT,
            ..TerminalModes::default()
        };
        let release = InputAction::Key(KeyEvent {
            code: KeyCode::Character("A".to_owned()),
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            pressed: false,
            repeated: false,
        });
        assert_eq!(
            InputEncoder::encode(&release, modes).unwrap(),
            b"\x1b[97;2:3u"
        );
        let repeat = InputAction::Key(KeyEvent {
            code: KeyCode::ArrowLeft,
            modifiers: Modifiers::default(),
            pressed: true,
            repeated: true,
        });
        assert_eq!(
            InputEncoder::encode(&repeat, modes).unwrap(),
            b"\x1b[1;1:2D"
        );
    }

    #[test]
    fn xterm_modify_other_keys_levels_and_format_are_encoded() {
        let alt_tab = key(
            KeyCode::Tab,
            Modifiers {
                alt: true,
                ..Modifiers::default()
            },
        );
        let level_one = TerminalModes {
            modify_other_keys: 1,
            ..TerminalModes::default()
        };
        assert_eq!(
            InputEncoder::encode(&alt_tab, level_one).unwrap(),
            b"\x1b[27;3;9~"
        );

        let level_two_csi_u = TerminalModes {
            modify_other_keys: 2,
            format_other_keys: true,
            ..TerminalModes::default()
        };
        assert_eq!(
            InputEncoder::encode(
                &key(
                    KeyCode::Character("A".to_owned()),
                    Modifiers {
                        shift: true,
                        ..Modifiers::default()
                    }
                ),
                level_two_csi_u
            )
            .unwrap(),
            b"\x1b[65;2u"
        );
    }
}
