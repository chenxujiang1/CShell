//! Shared workbench colors. Terminal ANSI palettes remain session-owned.
use egui::Color32;

pub const BACKGROUND: Color32 = Color32::from_rgb(20, 25, 32);
pub const PANEL: Color32 = Color32::from_rgb(27, 33, 41);
pub const RAISED: Color32 = Color32::from_rgb(36, 44, 54);
pub const INPUT: Color32 = Color32::from_rgb(18, 23, 30);
pub const LINE: Color32 = Color32::from_rgb(51, 62, 76);
pub const TEXT: Color32 = Color32::from_rgb(218, 225, 235);
pub const MUTED: Color32 = Color32::from_rgb(148, 161, 179);
pub const DISABLED: Color32 = Color32::from_rgb(95, 108, 124);
pub const ACCENT: Color32 = Color32::from_rgb(119, 201, 181);
pub const ACCENT_FILL: Color32 = Color32::from_rgb(39, 74, 68);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(49, 91, 83);
pub const SELECTED: Color32 = Color32::from_rgb(38, 57, 64);
pub const SUCCESS: Color32 = Color32::from_rgb(153, 199, 143);
pub const WARNING: Color32 = Color32::from_rgb(221, 180, 112);
pub const DANGER: Color32 = Color32::from_rgb(235, 144, 144);
