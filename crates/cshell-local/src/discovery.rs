//! Discover installed shells without executing them or modifying process environment.
use crate::{LocalProfile, WorkingDirectoryPolicy};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn discover_shells() -> Vec<LocalProfile> {
    let mut profiles = Vec::new();
    #[cfg(windows)]
    {
        add_path(&mut profiles, "PowerShell 7", "pwsh.exe");
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            add(
                &mut profiles,
                "PowerShell 7",
                Path::new(&program_files).join("PowerShell/7/pwsh.exe"),
            );
        }
        if let Some(root) = std::env::var_os("SystemRoot") {
            let system = Path::new(&root).join("System32");
            add(
                &mut profiles,
                "Windows PowerShell",
                system.join("WindowsPowerShell/v1.0/powershell.exe"),
            );
            add(&mut profiles, "Command Prompt", system.join("cmd.exe"));
            add(&mut profiles, "WSL", system.join("wsl.exe"));
        }
        if let Some(comspec) = std::env::var_os("COMSPEC") {
            add(&mut profiles, "Command Prompt", PathBuf::from(comspec));
        }
        add_path(&mut profiles, "Windows PowerShell", "powershell.exe");
        add_path(&mut profiles, "Command Prompt", "cmd.exe");
        add_path(&mut profiles, "WSL", "wsl.exe");
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            add(
                &mut profiles,
                "Git Bash",
                Path::new(&program_files).join("Git/bin/bash.exe"),
            );
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            add(
                &mut profiles,
                "Git Bash",
                Path::new(&local).join("Programs/Git/bin/bash.exe"),
            );
        }
        add_path(&mut profiles, "Bash", "bash.exe");
    }
    #[cfg(unix)]
    {
        if let Some(shell) = std::env::var_os("SHELL") {
            add(&mut profiles, "Default shell", PathBuf::from(shell));
        }
        for program in [
            "/bin/zsh",
            "/bin/bash",
            "/bin/sh",
            "/usr/bin/fish",
            "/usr/local/bin/fish",
            "/opt/homebrew/bin/fish",
            "/opt/homebrew/bin/bash",
        ] {
            let path = Path::new(program);
            add(
                &mut profiles,
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Shell"),
                path.to_path_buf(),
            );
        }
        if let Ok(file) = std::fs::File::open("/etc/shells") {
            use std::io::Read;
            let mut contents = String::new();
            if file.take(64 * 1024).read_to_string(&mut contents).is_ok() {
                for line in contents
                    .lines()
                    .take(256)
                    .map(str::trim)
                    .filter(|line| line.starts_with('/'))
                {
                    let path = Path::new(line);
                    add(
                        &mut profiles,
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("Shell"),
                        path.to_path_buf(),
                    );
                }
            }
        }
    }
    profiles
}

fn add(profiles: &mut Vec<LocalProfile>, name: &str, path: PathBuf) {
    if !executable_file(&path) || path.to_str().is_none() {
        return;
    }
    let Ok(path) = std::fs::canonicalize(path) else {
        return;
    };
    if path.to_str().is_none() || profiles.iter().any(|profile| profile.program == path) {
        return;
    }
    profiles.push(LocalProfile {
        name: name.into(),
        program: path,
        args: Vec::new(),
        cwd_policy: WorkingDirectoryPolicy::Inherit,
        env_overrides: BTreeMap::new(),
    });
}

pub(crate) fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        metadata.is_file()
    }
}

#[cfg(windows)]
fn add_path(profiles: &mut Vec<LocalProfile>, name: &str, program: &str) {
    if let Some(path) = crate::find_on_path(program) {
        add(profiles, name, path);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn discovery_only_returns_unique_existing_executables() {
        let shells = super::discover_shells();
        assert!(!shells.is_empty());
        let mut paths = std::collections::BTreeSet::new();
        for shell in shells {
            assert!(super::executable_file(&shell.program));
            assert!(shell.program.is_absolute());
            assert!(paths.insert(shell.program));
        }
    }
}
