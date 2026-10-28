use std::path::{Path, PathBuf};

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Settings {
    #[serde(default)]
    sync_presets:Vec<SyncPreset>,
    #[serde(default)]
    local_directory: String,
    #[serde(default)]
    favorites: Vec<Favorite>,
    #[serde(default)]
    layout: Layout,
    #[serde(default)]
    window: Option<WindowPlacement>,
}
#[derive(Clone,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncPreset {
    pub id:cshell_domain::SessionId,pub name:String,pub peer:cshell_domain::ClipboardTrust,
    pub local:String,pub remote:String,pub direction:cshell_application::SftpQueueDirection,
    pub excludes:Vec<String>,pub compare_content:bool,pub preserve_modified:bool,
}
impl SyncPreset {
    fn valid(&self)->bool{!self.id.as_uuid().is_nil()&&!self.name.trim().is_empty()&&self.name.len()<=64&&!self.name.chars().any(char::is_control)
        && self.local.len()<=4096&&Path::new(&self.local).is_absolute()&&!self.local.chars().any(char::is_control)
        && cshell_application::sftp_remote_path(&self.remote,true)&&cshell_application::valid_sftp_exclusions(&self.excludes)}
}
pub(crate) fn sync_presets()->Vec<SyncPreset>{path().map(|p|load(&p).sync_presets.into_iter().filter(SyncPreset::valid).take(32).collect()).unwrap_or_default()}
pub(crate) fn save_sync_preset(value:SyncPreset)->Result<Vec<SyncPreset>,String>{let path=path().ok_or("无法定位 SFTP 设置")?;update_sync_preset(&path,Some(value),None).map_err(|_|"方案未保存：检查名称、规则与目录，最多保存 32 个方案".into())}
pub(crate) fn delete_sync_preset(id:cshell_domain::SessionId)->Result<Vec<SyncPreset>,String>{let path=path().ok_or("无法定位 SFTP 设置")?;update_sync_preset(&path,None,Some(id)).map_err(|_|"方案未删除，请检查设置目录权限".into())}
fn update_sync_preset(path:&Path,value:Option<SyncPreset>,remove:Option<cshell_domain::SessionId>)->std::io::Result<Vec<SyncPreset>>{
    let _guard=WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);let mut settings=load(path);
    if let Some(value)=value {if !value.valid(){return Err(std::io::ErrorKind::InvalidInput.into());}settings.sync_presets.retain(|p|p.id!=value.id);if settings.sync_presets.len()>=32{return Err(std::io::Error::other("preset limit"));}settings.sync_presets.push(value);}
    if let Some(id)=remove{settings.sync_presets.retain(|p|p.id!=id);}
    write(path,&settings)?;Ok(settings.sync_presets)
}

/// Sizes are logical pixels; ratios adapt to a resized window without changing saved preferences.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Layout {
    pub columns: [Vec<f32>; 2],
    pub left_ratio: f32,
    pub transfer_ratio: f32,
}
impl Default for Layout {
    fn default() -> Self {
        Self {
            columns: Default::default(),
            left_ratio: 0.5,
            transfer_ratio: 0.30,
        }
    }
}
impl Layout {
    pub(crate) fn sanitized(mut self) -> Self {
        if !self.left_ratio.is_finite() {
            self.left_ratio = 0.5;
        }
        if !self.transfer_ratio.is_finite() {
            self.transfer_ratio = 0.30;
        }
        self.left_ratio = self.left_ratio.clamp(0.15, 0.85);
        self.transfer_ratio = self.transfer_ratio.clamp(0.15, 0.65);
        for (side, count) in [(0, 4), (1, 6)] {
            if self.columns[side].len() != count
                || self.columns[side].iter().any(|v| !v.is_finite())
            {
                self.columns[side].clear();
            } else {
                for value in &mut self.columns[side] {
                    *value = value.clamp(48.0, 1500.0);
                }
            }
        }
        self
    }
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WindowPlacement {
    pub position: Option<[i32; 2]>,
    pub inner_size: [f64; 2],
    pub maximized: bool,
}
impl WindowPlacement {
    pub(crate) fn sanitized(mut self) -> Self {
        for (index, default, min, max) in [(0, 1560.0, 720.0, 7680.0), (1, 900.0, 480.0, 4320.0)] {
            if !self.inner_size[index].is_finite() {
                self.inner_size[index] = default;
            }
            self.inner_size[index] = self.inner_size[index].clamp(min, max);
        }
        self
    }
}
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
// Headless controls use isolated defaults instead of the operator's preferences.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn layout() -> Layout {
    path()
        .map(|p| load(&p).layout.sanitized())
        .unwrap_or_default()
}
pub(crate) fn window() -> Option<WindowPlacement> {
    path()
        .and_then(|p| load(&p).window)
        .map(WindowPlacement::sanitized)
}
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn save_layout(layout: &Layout) -> Result<(), String> {
    let path = path().ok_or("无法定位 SFTP 设置目录")?;
    update_layout(&path, layout).map_err(|e| format!("布局未保存：{e}"))
}
fn update_layout(path: &Path, layout: &Layout) -> std::io::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut settings = load(path);
    settings.layout = layout.clone().sanitized();
    write(path, &settings)
}
pub(crate) fn save_window(window: &WindowPlacement) -> Result<(), String> {
    let path = path().ok_or("无法定位 SFTP 设置目录")?;
    update_window(&path, window).map_err(|e| format!("窗口位置未保存：{e}"))
}
fn update_window(path: &Path, window: &WindowPlacement) -> std::io::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut settings = load(path);
    settings.window = Some(window.clone().sanitized());
    write(path, &settings)
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Favorite {
    pub peer: Option<cshell_domain::ClipboardTrust>,
    pub path: String,
}

fn path() -> Option<PathBuf> {
    crate::display_settings::settings_path().map(|path| path.with_file_name("sftp-settings.json"))
}

const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

fn load(path: &Path) -> Settings {
    (|| {
        if std::fs::metadata(path).ok()?.len() > MAX_SETTINGS_BYTES {
            return None;
        }
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    })()
    .unwrap_or_default()
}

fn read(path: &Path) -> String {
    let settings = load(path);
    if settings.local_directory.is_empty() || valid_directory(&settings.local_directory) {
        settings.local_directory
    } else {
        String::new()
    }
}

pub(crate) fn favorites() -> Vec<Favorite> {
    path().map(|path| load(&path).favorites).unwrap_or_default()
}

pub(crate) fn toggle_favorite(favorite: Favorite) -> Result<Vec<Favorite>, String> {
    let path = path().ok_or("无法定位 SFTP 设置目录")?;
    toggle(&path, favorite).map_err(|error| format!("收藏未保存：{error}"))
}

fn toggle(path: &Path, favorite: Favorite) -> std::io::Result<Vec<Favorite>> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let local = favorite.peer.is_none();
    if favorite.path.is_empty()
        || favorite.path.len() > cshell_ipc::MAX_SFTP_PATH_BYTES
        || favorite.path.chars().any(char::is_control)
        || if local {
            !Path::new(&favorite.path).is_absolute()
        } else {
            !favorite.path.starts_with('/')
        }
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "收藏目录必须是绝对路径",
        ));
    }
    let mut settings = load(path);
    if settings.favorites.contains(&favorite) {
        settings.favorites.retain(|item| item != &favorite);
    } else {
        if settings.favorites.len() >= 128
            || settings
                .favorites
                .iter()
                .filter(|item| item.peer == favorite.peer)
                .count()
                >= 32
        {
            return Err(std::io::Error::other(
                "每侧最多收藏 32 个目录，总计最多 128 个",
            ));
        }
        settings.favorites.push(favorite);
    }
    write(path, &settings)?;
    Ok(settings.favorites)
}

fn valid_directory(directory: &str) -> bool {
    directory.len() <= cshell_ipc::MAX_SFTP_PATH_BYTES
        && !directory.contains('\0')
        && Path::new(directory).is_absolute()
        && Path::new(directory).is_dir()
}

pub fn local_directory() -> String {
    path().as_deref().map(read).unwrap_or_default()
}

pub fn save_local_directory(directory: &str) -> Result<(), String> {
    let path = path().ok_or("无法定位 SFTP 设置目录")?;
    save(&path, directory).map_err(|error| format!("默认目录未保存：{error}"))
}

fn save(path: &Path, directory: &str) -> std::io::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !directory.is_empty() && !valid_directory(directory) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "请输入已存在文件夹的绝对路径",
        ));
    }
    let mut settings = load(path);
    settings.local_directory = directory.into();
    write(path, &settings)
}

fn write(path: &Path, settings: &Settings) -> std::io::Result<()> {
    use std::io::Write;
    let data = serde_json::to_vec_pretty(settings)?;
    if data.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(std::io::Error::other("SFTP 设置过大"));
    }
    let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&data)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_directory_survives_reload_and_invalid_edits_preserve_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sftp-settings.json");
        let directory = root.path().to_str().unwrap();
        save(&path, directory).unwrap();
        assert_eq!(read(&path), directory);
        for invalid in [
            "relative/path",
            root.path().join("missing").to_str().unwrap(),
        ] {
            assert!(save(&path, invalid).is_err());
            assert_eq!(read(&path), directory);
        }
        save(&path, "").unwrap();
        assert_eq!(read(&path), "");
    }
    #[test]
    fn missing_folder_and_corrupt_settings_fall_back_to_user_home() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        save(&path, folder.to_str().unwrap()).unwrap();
        std::fs::remove_dir(&folder).unwrap();
        assert_eq!(read(&path), "");
        std::fs::write(&path, b"{broken").unwrap();
        assert_eq!(read(&path), "");
    }
}

#[cfg(test)]
mod favorite_tests {
    use super::*;
    #[test]
    fn favorites_survive_reload_and_preserve_default_folder_in_both_directions() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("sftp-settings.json");
        let directory = root.path().to_str().unwrap();
        // Old settings containing only local_directory remain readable.
        std::fs::write(
            &settings,
            serde_json::to_vec(&serde_json::json!({"local_directory": directory})).unwrap(),
        )
        .unwrap();
        let local = Favorite {
            peer: None,
            path: directory.into(),
        };
        assert_eq!(toggle(&settings, local.clone()).unwrap().len(), 1);
        assert_eq!(read(&settings), directory);
        save(&settings, "").unwrap();
        assert!(load(&settings).favorites == [local.clone()]);
        save(&settings, directory).unwrap();
        assert!(load(&settings).favorites == [local.clone()]);
        assert!(
            toggle(
                &settings,
                Favorite {
                    peer: None,
                    path: "relative".into()
                }
            )
            .is_err()
        );
        assert!(load(&settings).favorites == [local.clone()]);
        assert!(toggle(&settings, local).unwrap().is_empty());
        assert_eq!(read(&settings), directory);
    }
    #[test]
    fn different_remote_accounts_keep_separate_favorites() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("sftp-settings.json");
        let peer = cshell_domain::ClipboardTrust {
            host: cshell_domain::ClipboardHost::new("server.test", 22).unwrap(),
            username: "root".into(),
            host_key_sha256: "key".into(),
            route: "direct".into(),
        };
        let first = Favorite {
            peer: Some(peer.clone()),
            path: "/var/log".into(),
        };
        let mut other = peer;
        other.username = "user".into();
        let second = Favorite {
            peer: Some(other),
            path: "/var/log".into(),
        };
        toggle(&settings, first.clone()).unwrap();
        assert_eq!(toggle(&settings, second.clone()).unwrap().len(), 2);
        assert!(toggle(&settings, first).unwrap() == [second]);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    #[test]
    fn sync_presets_roundtrip_preserves_layout_and_refuses_invalid_updates() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("sftp-settings.json");
        save(&path,root.path().to_str().unwrap()).unwrap();let layout=Layout{left_ratio:0.6,..Default::default()};update_layout(&path,&layout).unwrap();
        let preset=SyncPreset{id:cshell_domain::SessionId::new(),name:"Deploy".into(),peer:cshell_domain::ClipboardTrust{host:cshell_domain::ClipboardHost::new("server.test",22).unwrap(),username:"operator".into(),host_key_sha256:"test-key".into(),route:"direct".into()},local:root.path().to_string_lossy().into(),remote:"/srv/app".into(),direction:cshell_application::SftpQueueDirection::Upload,excludes:vec![".git/**".into()],compare_content:true,preserve_modified:true};
        update_sync_preset(&path,Some(preset.clone()),None).unwrap();
        let mut other=preset.clone();other.id=cshell_domain::SessionId::new();other.peer.username="root".into();update_sync_preset(&path,Some(other),None).unwrap();
        let before=std::fs::read(&path).unwrap();let mut invalid=preset.clone();invalid.excludes=vec!["../escape".into()];assert!(update_sync_preset(&path,Some(invalid),None).is_err());assert_eq!(std::fs::read(&path).unwrap(),before);
        let restored=load(&path);assert_eq!(restored.layout.left_ratio,0.6);assert_eq!(restored.local_directory,root.path().to_str().unwrap());assert_eq!(restored.sync_presets.iter().filter(|p|p.peer==preset.peer).count(),1);
        assert_eq!(update_sync_preset(&path,None,Some(preset.id)).unwrap().len(),1);
    }
    #[test]
    fn layout_window_default_folder_and_favorites_merge_and_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sftp-settings.json");
        let directory = root.path().to_str().unwrap();
        save(&path, directory).unwrap();
        let favorite = Favorite {
            peer: None,
            path: directory.into(),
        };
        toggle(&path, favorite.clone()).unwrap();
        let layout = Layout {
            columns: [
                vec![250.0, 100.0, 90.0, 180.0],
                vec![220.0, 90.0, 80.0, 170.0, 100.0, 90.0],
            ],
            left_ratio: 0.63,
            transfer_ratio: 0.42,
        };
        let window = WindowPlacement {
            position: Some([-1600, 90]),
            inner_size: [1280.0, 820.0],
            maximized: true,
        };
        update_layout(&path, &layout).unwrap();
        update_window(&path, &window).unwrap();
        save(&path, "").unwrap();
        toggle(
            &path,
            Favorite {
                peer: None,
                path: root.path().join("second").to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        let restored = load(&path);
        assert_eq!(restored.layout, layout);
        assert_eq!(restored.window.unwrap(), window);
        assert!(restored.favorites.contains(&favorite));
        assert_eq!(restored.local_directory, "");
        update_layout(&path, &Layout::default()).unwrap();
        assert!(load(&path).window.is_some());
    }
    #[test]
    fn old_settings_and_invalid_layout_values_recover_to_usable_defaults() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sftp-settings.json");
        std::fs::write(&path, br#"{"local_directory":"","favorites":[]}"#).unwrap();
        assert_eq!(load(&path).layout, Layout::default());
        assert!(load(&path).window.is_none());
        let bad = Layout {
            columns: [vec![f32::NAN; 4], vec![1.0; 6]],
            left_ratio: f32::INFINITY,
            transfer_ratio: -50.0,
        }
        .sanitized();
        assert_eq!(bad.left_ratio, 0.5);
        assert_eq!(bad.transfer_ratio, 0.15);
        assert!(bad.columns[0].is_empty());
        assert!(bad.columns[1].iter().all(|v| *v == 48.0));
    }
}
