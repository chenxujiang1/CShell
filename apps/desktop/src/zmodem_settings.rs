use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub terminal_upload_protocol: cshell_application::TerminalUploadProtocol,
    pub confirm_terminal_upload: bool,
    #[serde(default = "auto_close_default")]
    pub close_successful_upload: bool,
    pub download_directory: String,
    pub sftp_verification: cshell_domain::SftpVerification,
}
fn auto_close_default() -> bool {
    true
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal_upload_protocol: Default::default(),
            confirm_terminal_upload: false,
            close_successful_upload: true,
            download_directory: String::new(),
            sftp_verification: Default::default(),
        }
    }
}
pub(crate) fn verification_label(mode: cshell_domain::SftpVerification) -> &'static str {
    match mode {
        cshell_domain::SftpVerification::Standard => "标准传输（推荐）",
        cshell_domain::SftpVerification::Full => "完整 SHA-256 校验",
    }
}
pub(crate) fn verification_selector(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut cshell_domain::SftpVerification,
) -> bool {
    let old = *value;
    egui::ComboBox::from_id_salt(id)
        .selected_text(verification_label(*value))
        .show_ui(ui, |ui| {
            for mode in [
                cshell_domain::SftpVerification::Standard,
                cshell_domain::SftpVerification::Full,
            ] {
                ui.selectable_value(value, mode, verification_label(mode));
            }
        });
    old != *value
}
pub(crate) fn verification_override_selector(
    ui: &mut egui::Ui,
    value: &mut Option<cshell_domain::SftpVerification>,
) {
    egui::ComboBox::from_id_salt("session-sftp-verification")
        .selected_text(value.map(verification_label).unwrap_or("继承全局默认"))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "继承全局默认");
            for mode in [
                cshell_domain::SftpVerification::Standard,
                cshell_domain::SftpVerification::Full,
            ] {
                ui.selectable_value(value, Some(mode), verification_label(mode));
            }
        });
}
fn path() -> Option<PathBuf> {
    crate::display_settings::settings_path().map(|p| p.with_file_name("transfer-settings.json"))
}
pub(crate) fn load() -> Settings {
    path().as_deref().map(read).unwrap_or_default()
}
fn read(path: &Path) -> Settings {
    (|| {
        if std::fs::metadata(path).ok()?.len() > 16384 {
            return None;
        }
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    })()
    .unwrap_or_default()
}
pub(crate) fn validate_directory(value: &str) -> Result<(), String> {
    if !value.is_empty()
        && (value.len() > 4096
            || value.contains('\0')
            || !Path::new(value).is_absolute()
            || !Path::new(value).is_dir())
    {
        return Err("请输入已存在文件夹的绝对路径，或留空使用默认目录".into());
    }
    Ok(())
}
pub(crate) fn save(settings: &Settings) -> Result<(), String> {
    let path = path().ok_or("无法定位设置目录")?;
    write(&path, settings)
}
fn write(path: &Path, settings: &Settings) -> Result<(), String> {
    use std::io::Write;
    validate_directory(&settings.download_directory)?;
    let parent = path.parent().ok_or("无法定位设置目录")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.error.to_string())?;
    Ok(())
}
pub(crate) fn user_directory() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default()
}
pub(crate) fn resolve(settings: &Settings, session_override: Option<&str>) -> PathBuf {
    if let Some(path) = session_override.filter(|p| !p.is_empty()) {
        return path.into();
    }
    if !settings.download_directory.is_empty() {
        return PathBuf::from(&settings.download_directory);
    }
    let home = user_directory();
    let downloads = home.join("Downloads");
    if downloads.is_dir() { downloads } else { home }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_inherits_live_global_defaults_without_copying_them() {
        let global = Settings {
            download_directory: "global-one".into(),
            ..Default::default()
        };
        assert_eq!(resolve(&global, None), PathBuf::from("global-one"));
        assert_eq!(resolve(&global, Some("session")), PathBuf::from("session"));
        let global = Settings {
            download_directory: "global-two".into(),
            ..Default::default()
        };
        assert_eq!(resolve(&global, None), PathBuf::from("global-two"));
        assert_eq!(resolve(&global, Some("session")), PathBuf::from("session"));
        assert_eq!(resolve(&global, Some("")), PathBuf::from("global-two"));
    }
    #[test]
    fn atomic_settings_save_reload_and_invalid_edit_preserves_previous() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        let good = Settings {
            download_directory: root.path().to_string_lossy().into(),
            sftp_verification: cshell_domain::SftpVerification::Full,
            ..Default::default()
        };
        write(&path, &good).unwrap();
        assert_eq!(read(&path).download_directory, good.download_directory);
        assert_eq!(
            read(&path).sftp_verification,
            cshell_domain::SftpVerification::Full
        );
        assert!(
            write(
                &path,
                &Settings {
                    download_directory: "relative".into(),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert_eq!(read(&path).download_directory, good.download_directory);
        std::fs::write(&path, "broken").unwrap();
        assert!(read(&path).download_directory.is_empty());
    }
    #[test]
    fn old_transfer_defaults_keep_the_directory_and_choose_standard() {
        let settings: Settings =
            serde_json::from_str(r#"{"download_directory":"D:/saved"}"#).unwrap();
        assert_eq!(settings.download_directory, "D:/saved");
        assert_eq!(
            settings.sftp_verification,
            cshell_domain::SftpVerification::Standard
        );
    }
}
