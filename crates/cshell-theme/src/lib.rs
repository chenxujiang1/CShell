//! Clean-room palette conversion. No client source code or executable fields.
use cshell_domain::{TerminalPalette, ThemeRecord};
use std::collections::BTreeMap;

pub fn theme_reference(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    format!("xcs-{}", if slug.is_empty() { "imported" } else { &slug })
}
pub fn decode_theme_text(bytes: &[u8]) -> Result<String, &'static str> {
    if bytes.len() > 256 * 1024 {
        return Err("theme input exceeds 256 KiB");
    }
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        if bytes.len() % 2 != 0 {
            return Err("invalid UTF-16 theme");
        }
        let words = bytes[2..]
            .chunks_exact(2)
            .map(|v| {
                if little {
                    u16::from_le_bytes([v[0], v[1]])
                } else {
                    u16::from_be_bytes([v[0], v[1]])
                }
            })
            .collect::<Vec<_>>();
        String::from_utf16(&words).map_err(|_| "invalid UTF-16 theme")
    } else {
        std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .map(str::to_owned)
            .map_err(|_| "theme must be UTF-8 or BOM UTF-16")
    }
}
fn rgb(value: &str) -> Result<[u8; 3], &'static str> {
    if value.len() != 6 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("XCS colors must be six hex digits");
    }
    Ok([
        u8::from_str_radix(&value[0..2], 16).map_err(|_| "invalid color")?,
        u8::from_str_radix(&value[2..4], 16).map_err(|_| "invalid color")?,
        u8::from_str_radix(&value[4..6], 16).map_err(|_| "invalid color")?,
    ])
}
/// Supported public INI dialect: named sections plus [Names] count/nameN.
/// Every referenced section must be complete; unknown keys are rejected.
pub fn import_xcs(bytes: &[u8]) -> Result<Vec<ThemeRecord>, &'static str> {
    let text = decode_theme_text(bytes)?;
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = &line[1..line.len() - 1];
            if name.is_empty()
                || name.len() > 128
                || name.chars().any(char::is_control)
                || sections.contains_key(name)
                || sections.len() >= 129
            {
                return Err("invalid/duplicate XCS section");
            }
            sections.insert(name.into(), BTreeMap::new());
            current = Some(name.to_owned());
        } else {
            let (key, value) = line.split_once('=').ok_or("invalid XCS assignment")?;
            let row = sections
                .get_mut(current.as_deref().ok_or("XCS key before section")?)
                .ok_or("missing XCS section")?;
            if key.trim().len() > 64
                || value.trim().len() > 128
                || row.insert(key.trim().into(), value.trim().into()).is_some()
            {
                return Err("duplicate/oversized XCS assignment");
            }
        }
    }
    let names = sections.remove("Names").ok_or("XCS requires [Names]")?;
    let count: usize = names
        .get("count")
        .ok_or("XCS requires count")?
        .parse()
        .map_err(|_| "invalid XCS count")?;
    if count == 0 || count > 128 || names.len() != count + 1 {
        return Err("invalid XCS name directory");
    }
    let colors = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    let mut result = Vec::new();
    let mut refs = std::collections::BTreeSet::new();
    for i in 0..count {
        let name = names.get(&format!("name{i}")).ok_or("missing XCS name")?;
        let mut row = sections.remove(name).ok_or("missing XCS palette")?;
        let foreground = rgb(&row.remove("text").ok_or("missing XCS text")?)?;
        let background = rgb(&row.remove("background").ok_or("missing XCS background")?)?;
        // Bold default foreground is not a v1 palette field; parse it to refuse
        // malformed input. Preview surfaces explicitly disclose the mapping.
        if let Some(value) = row.remove("text(bold)") {
            rgb(&value)?;
        }
        let mut ansi = [[0; 3]; 16];
        for (index, key) in colors.iter().enumerate() {
            ansi[index] = rgb(&row.remove(*key).ok_or("missing XCS ANSI color")?)?;
            ansi[index + 8] = rgb(&row
                .remove(&format!("{key}(bold)"))
                .ok_or("missing XCS bright color")?)?;
        }
        if !row.is_empty() {
            return Err("unsupported XCS palette field");
        }
        let reference = theme_reference(name);
        if !refs.insert(reference.clone()) {
            return Err("XCS names collide after normalization");
        }
        result.push(ThemeRecord {
            name: reference,
            palette: TerminalPalette {
                foreground,
                background,
                cursor: foreground,
                selection: ansi[4],
                ansi,
            },
        });
    }
    if !sections.is_empty() {
        return Err("unreferenced XCS section");
    }
    Ok(result)
}

/// Local default palette; no external theme-pack assets are bundled.
pub fn default_palette() -> TerminalPalette {
    TerminalPalette {
        foreground: [216, 222, 233],
        background: [25, 29, 38],
        cursor: [216, 222, 233],
        selection: [51, 115, 230],
        ansi: [
            [0, 0, 0],
            [205, 49, 49],
            [13, 188, 121],
            [229, 229, 16],
            [36, 114, 200],
            [188, 63, 188],
            [17, 168, 205],
            [229, 229, 229],
            [102, 102, 102],
            [241, 76, 76],
            [35, 209, 139],
            [245, 245, 67],
            [59, 142, 234],
            [214, 112, 214],
            [41, 184, 219],
            [255, 255, 255],
        ],
    }
}
