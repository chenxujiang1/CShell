use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
    pub generation: u64,
}

impl TerminalSize {
    #[must_use]
    pub const fn cells(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
            generation: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum KeyCode {
    Character(String),
    Enter,
    Tab,
    Backspace,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Function(u8),
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: Modifiers,
    pub pressed: bool,
    #[serde(default)]
    pub repeated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlAction {
    Interrupt,
    EndOfFile,
    Suspend,
    ClearScreen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
    WheelUp,
    WheelDown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub row: u16,
    pub column: u16,
    pub button: MouseButton,
    pub kind: MouseEventKind,
    pub modifiers: Modifiers,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum InputAction {
    Text(String),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste {
        text: String,
        bracketed: bool,
    },
    /// Bracketed paste, followed by an optional Enter outside the paste markers.
    Compose {
        text: String,
        append_enter: bool,
    },
    Control(ControlAction),
}
