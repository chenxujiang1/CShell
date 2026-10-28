use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    terminal_font_pixels: u16,
}

pub fn load() -> (Option<PathBuf>, u16) {
    let path = settings_path();
    let pixels = path
        .as_deref()
        .and_then(|path| {
            if std::fs::metadata(path).ok()?.len() > 4096 {
                return None;
            }
            let settings: Settings = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
            Some(settings.terminal_font_pixels.clamp(
                cshell_domain::MIN_TERMINAL_FONT_PIXELS,
                cshell_domain::MAX_TERMINAL_FONT_PIXELS,
            ))
        })
        .unwrap_or(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS);
    (path, pixels)
}

pub fn save(path: &Path, pixels: u16) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let settings = Settings {
        terminal_font_pixels: pixels.clamp(
            cshell_domain::MIN_TERMINAL_FONT_PIXELS,
            cshell_domain::MAX_TERMINAL_FONT_PIXELS,
        ),
    };
    std::fs::write(path, serde_json::to_vec_pretty(&settings)?)
}

pub(crate) fn settings_path() -> Option<PathBuf> {
    if let Some(database) = std::env::var_os("CSHELL_PROFILE_DB") {
        return PathBuf::from(database)
            .parent()
            .map(|parent| parent.join("display-settings.json"));
    }
    if let Some(root) = std::env::var_os("CSHELL_DATA_DIR") {
        return Some(PathBuf::from(root).join("display-settings.json"));
    }
    #[cfg(windows)]
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("CShell");
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .join("Library/Application Support/CShell");
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?
        .join("cshell");
    Some(root.join("display-settings.json"))
}
