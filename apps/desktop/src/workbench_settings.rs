use cshell_ui::workbench_layout::Layout;
use std::path::{Path, PathBuf};

pub(crate) fn load() -> (Option<PathBuf>, Layout) {
    let path = crate::display_settings::settings_path()
        .map(|p| p.with_file_name("workbench-settings.json"));
    let layout = path.as_deref().map(read).unwrap_or_default();
    (path, layout)
}
pub(crate) fn read(path: &Path) -> Layout {
    (|| {
        if std::fs::metadata(path).ok()?.len() > 4096 {
            return None;
        }
        let value: Layout = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        Some(value.normalized())
    })()
    .unwrap_or_default()
}
pub(crate) fn save(path: &Path, layout: &Layout) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().ok_or("无法定位布局设置目录")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    file.write_all(
        &serde_json::to_vec_pretty(&layout.clone().normalized()).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_round_trip_defaults_and_corruption_do_not_restore_runtime_data() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("workbench-settings.json");
        let layout = Layout {
            sidebar_hidden: true,
            sidebar_width: Some(162.0),
            toolbar_hidden: true,
        };
        save(&path, &layout).unwrap();
        assert_eq!(read(&path), layout);
        save(&path, &Layout::default()).unwrap();
        assert_eq!(read(&path), Layout::default());
        std::fs::write(&path, br#"{"sidebar_width":-5,"unexpected":true}"#).unwrap();
        assert_eq!(read(&path).sidebar_width, Some(120.0));
        std::fs::write(&path, b"invalid").unwrap();
        assert_eq!(read(&path), Layout::default());
        assert!(
            temp.path()
                .read_dir()
                .unwrap()
                .all(|e| e.unwrap().file_name() == "workbench-settings.json")
        );
    }
}
