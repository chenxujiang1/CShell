//! Shared, non-mutating storage selection for desktop and daemon upgrades.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoragePaths {
    pub profile_database: PathBuf,
    pub data_root: PathBuf,
    /// Custom/legacy stores must not attach to the default store's daemon.
    pub runtime_root: Option<PathBuf>,
}

impl StoragePaths {
    pub fn for_current_user() -> io::Result<Self> {
        Self::resolve(
            std::env::var_os("CSHELL_PROFILE_DB").map(PathBuf::from),
            std::env::var_os("CSHELL_DATA_DIR").map(PathBuf::from),
            &std::env::current_exe()?,
            default_data_root,
        )
    }

    fn resolve(
        database: Option<PathBuf>,
        data: Option<PathBuf>,
        executable: &Path,
        default_root: impl FnOnce() -> io::Result<PathBuf>,
    ) -> io::Result<Self> {
        let legacy_root = if database.is_none() && data.is_none() {
            legacy_trial_root(executable)?
        } else {
            None
        };
        let (profile_database, scoped) = if let Some(path) = database {
            (absolute(&path)?, true)
        } else if let Some(path) = data {
            (absolute(&path)?.join("cshell.db"), true)
        } else if let Some(root) = &legacy_root {
            (root.join("data/cshell.db"), true)
        } else {
            (absolute(&default_root()?)?.join("cshell.db"), false)
        };
        let data_root = profile_database
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid database path"))?
            .to_path_buf();
        // The historical launcher keeps runtime beside data, not inside it.
        let runtime_root = if scoped {
            if let Some(root) = legacy_root {
                Some(root.join("runtime"))
            } else {
                let name = profile_database.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "database has no file name")
                })?;
                let standard_name = if cfg!(windows) {
                    name.eq_ignore_ascii_case("cshell.db")
                } else {
                    name == "cshell.db"
                };
                if standard_name {
                    Some(data_root.join("runtime"))
                } else {
                    let mut runtime_name = name.to_os_string();
                    runtime_name.push(".runtime");
                    Some(data_root.join(runtime_name))
                }
            }
        } else {
            None
        };
        Ok(Self {
            profile_database,
            data_root,
            runtime_root,
        })
    }
}

fn absolute(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty storage path",
        ));
    }
    std::path::absolute(path)
}

fn legacy_trial_root(executable: &Path) -> io::Result<Option<PathBuf>> {
    let Some(name) = executable.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if !["cshell.exe", "cshell-gui.exe", "cshelld.exe"]
        .iter()
        .any(|expected| name.eq_ignore_ascii_case(expected))
    {
        return Ok(None);
    }
    let Some(bin) = executable
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("bin")))
    else {
        return Ok(None);
    };
    let Some(root) = bin.parent() else {
        return Ok(None);
    };
    // Recognize only the original launcher layout; never scan unrelated disks
    // or choose a database by size, recency, or whether its catalog is empty.
    match std::fs::metadata(root.join("Start-Trial.cmd")) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    match std::fs::metadata(root.join("data")) {
        Ok(metadata) if metadata.is_dir() => {
            if !std::fs::metadata(root.join("data/cshell.db"))?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "trial database is not a file",
                ));
            }
            Ok(Some(absolute(root)?))
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trial data path is not a directory",
        )),
        Err(error) => Err(error),
    }
}

fn default_data_root() -> io::Result<PathBuf> {
    fn required(name: &str) -> io::Result<PathBuf> {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{name} is required for storage"),
                )
            })
    }
    #[cfg(windows)]
    {
        Ok(required("LOCALAPPDATA")?.join("CShell"))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(required("HOME")?.join("Library/Application Support/CShell"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map_or_else(
                || required("HOME").map(|home| home.join(".local/share")),
                Ok,
            )?
            .join("cshell"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> io::Result<tempfile::TempDir> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir_all(temp.path().join("bin"))?;
        std::fs::create_dir(temp.path().join("data"))?;
        std::fs::write(temp.path().join("Start-Trial.cmd"), "existing launcher")?;
        Ok(temp)
    }

    #[test]
    fn direct_gui_and_daemon_reuse_trial_data_without_mutating_any_files() -> io::Result<()> {
        let temp = fixture()?;
        for name in [
            "cshell.db",
            "vault.bin",
            "known_hosts",
            "display-settings.json",
        ] {
            std::fs::write(temp.path().join("data").join(name), name)?;
        }
        let default = temp.path().join("system");
        std::fs::create_dir(&default)?;
        std::fs::write(default.join("cshell.db"), "other empty catalog")?;
        for exe in ["CShell.exe", "cshell-gui.exe", "cshelld.exe"] {
            let paths =
                StoragePaths::resolve(None, None, &temp.path().join("bin").join(exe), || {
                    Ok(default.clone())
                })?;
            assert_eq!(paths.profile_database, temp.path().join("data/cshell.db"));
            assert_eq!(paths.runtime_root, Some(temp.path().join("runtime")));
        }
        for name in [
            "cshell.db",
            "vault.bin",
            "known_hosts",
            "display-settings.json",
        ] {
            assert_eq!(
                std::fs::read(temp.path().join("data").join(name))?,
                name.as_bytes()
            );
        }
        assert_eq!(
            std::fs::read(default.join("cshell.db"))?,
            b"other empty catalog"
        );
        assert!(!temp.path().join("runtime").exists());
        Ok(())
    }

    #[test]
    fn explicit_database_and_directory_take_precedence_and_scope_runtime() -> io::Result<()> {
        let temp = fixture()?;
        let exe = temp.path().join("bin/CShell.exe");
        let custom = temp.path().join("custom");
        let paths = StoragePaths::resolve(
            Some(custom.join("profiles.sqlite")),
            Some(temp.path().join("ignored")),
            &exe,
            || unreachable!(),
        )?;
        assert_eq!(paths.data_root, custom);
        assert_eq!(
            paths.runtime_root,
            Some(custom.join("profiles.sqlite.runtime"))
        );
        let paths = StoragePaths::resolve(None, Some(custom.clone()), &exe, || unreachable!())?;
        assert_eq!(paths.profile_database, custom.join("cshell.db"));
        assert!(
            StoragePaths::resolve(Some(PathBuf::new()), None, &exe, || unreachable!()).is_err()
        );
        Ok(())
    }

    #[test]
    fn separate_databases_in_one_directory_do_not_share_a_daemon() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let first = StoragePaths::resolve(
            Some(temp.path().join("first.sqlite")),
            None,
            Path::new("CShell.exe"),
            || unreachable!(),
        )?;
        let second = StoragePaths::resolve(
            Some(temp.path().join("second.sqlite")),
            None,
            Path::new("CShell.exe"),
            || unreachable!(),
        )?;
        assert_ne!(first.runtime_root, second.runtime_root);
        assert!(
            !first
                .runtime_root
                .ok_or_else(|| io::Error::other("missing runtime"))?
                .exists()
        );
        assert!(
            !second
                .runtime_root
                .ok_or_else(|| io::Error::other("missing runtime"))?
                .exists()
        );
        Ok(())
    }

    #[test]
    fn moving_normal_app_preserves_default_store_and_does_not_create_it() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let default = temp.path().join("user-data");
        for exe in [
            "old/CShell.exe",
            "new/CShell.exe",
            "Applications/CShell.app/Contents/MacOS/CShell",
        ] {
            let paths =
                StoragePaths::resolve(None, None, &temp.path().join(exe), || Ok(default.clone()))?;
            assert_eq!(paths.profile_database, default.join("cshell.db"));
            assert!(paths.runtime_root.is_none());
        }
        assert!(!default.exists());
        Ok(())
    }

    #[test]
    fn broken_legacy_layout_errors_instead_of_opening_empty_default_store() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir(temp.path().join("bin"))?;
        std::fs::write(temp.path().join("Start-Trial.cmd"), "existing")?;
        assert!(
            StoragePaths::resolve(
                None,
                None,
                &temp.path().join("bin/CShell.exe"),
                || unreachable!()
            )
            .is_err()
        );
        std::fs::create_dir(temp.path().join("data"))?;
        assert!(
            StoragePaths::resolve(
                None,
                None,
                &temp.path().join("bin/CShell.exe"),
                || unreachable!()
            )
            .is_err()
        );
        assert!(!temp.path().join("data/cshell.db").exists());
        Ok(())
    }
}
