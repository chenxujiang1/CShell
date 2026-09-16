//! Cross-platform local terminal profiles and portable-pty adapter.

use cshell_domain::TerminalSize;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use thiserror::Error;

#[cfg(unix)]
mod unix_process_tree;
#[cfg(windows)]
mod windows_process_tree;

#[cfg(unix)]
use unix_process_tree::ProcessTree;
#[cfg(windows)]
use windows_process_tree::ProcessTree;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkingDirectoryPolicy {
    Inherit,
    Home,
    Explicit(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LocalProfile {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd_policy: WorkingDirectoryPolicy,
    pub env_overrides: BTreeMap<String, String>,
}

impl LocalProfile {
    #[must_use]
    pub fn platform_default() -> Self {
        #[cfg(windows)]
        let (name, program) = find_on_path("pwsh.exe").map_or_else(
            || {
                (
                    "Command Prompt".to_owned(),
                    std::env::var_os("COMSPEC")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("cmd.exe")),
                )
            },
            |path| ("PowerShell 7".to_owned(), path),
        );

        #[cfg(not(windows))]
        let (name, program) = {
            let program = std::env::var_os("SHELL")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/bin/sh"));
            let name = program
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("Shell")
                .to_owned();
            (name, program)
        };

        Self {
            name,
            program,
            args: Vec::new(),
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum LocalPtyError {
    #[error("cannot open local PTY: {0}")]
    Open(String),
    #[error("cannot spawn local command: {0}")]
    Spawn(String),
    #[error("local PTY I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot resize local PTY: {0}")]
    Resize(String),
    #[error("local PTY output reader was already taken")]
    ReaderAlreadyTaken,
    #[error("local PTY input is already closed")]
    InputClosed,
    #[error("cannot supervise local process tree: {0}")]
    ProcessTree(std::io::Error),
}

pub struct PtySession {
    // Dropped first: descendants cannot retain the PTY after its owner exits.
    process_tree: ProcessTree,
    master: Box<dyn MasterPty + Send>,
    writer: Option<Box<dyn Write + Send>>,
    reader: Option<Box<dyn Read + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl fmt::Debug for PtySession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PtySession")
            .field("process_id", &self.child.process_id())
            .field("reader_available", &self.reader.is_some())
            .finish_non_exhaustive()
    }
}

impl PtySession {
    pub fn spawn(profile: &LocalProfile, size: TerminalSize) -> Result<Self, LocalPtyError> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            })
            .map_err(|error| LocalPtyError::Open(error.to_string()))?;

        let mut command = CommandBuilder::new(&profile.program);
        command.args(&profile.args);
        for (key, value) in &profile.env_overrides {
            command.env(key, value);
        }
        if let WorkingDirectoryPolicy::Explicit(path) = &profile.cwd_policy {
            command.cwd(path);
        }

        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| LocalPtyError::Spawn(error.to_string()))?;
        let process_tree = ProcessTree::attach(&*child).map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            LocalPtyError::ProcessTree(error)
        })?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| LocalPtyError::Io(std::io::Error::other(error.to_string())))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| LocalPtyError::Io(std::io::Error::other(error.to_string())))?;
        Ok(Self {
            process_tree,
            master: pair.master,
            writer: Some(writer),
            reader: Some(reader),
            child,
        })
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), LocalPtyError> {
        let writer = self.writer.as_mut().ok_or(LocalPtyError::InputClosed)?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn close_input(&mut self) {
        self.writer.take();
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), LocalPtyError> {
        self.master
            .resize(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            })
            .map_err(|error| LocalPtyError::Resize(error.to_string()))
    }

    pub fn take_reader(&mut self) -> Result<Box<dyn Read + Send>, LocalPtyError> {
        self.reader.take().ok_or(LocalPtyError::ReaderAlreadyTaken)
    }

    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>, LocalPtyError> {
        self.child.try_wait().map_err(Into::into)
    }

    pub fn wait(&mut self) -> Result<portable_pty::ExitStatus, LocalPtyError> {
        self.child.wait().map_err(Into::into)
    }

    pub fn kill(&mut self) -> Result<(), LocalPtyError> {
        #[cfg(unix)]
        self.process_tree
            .terminate(self.master.process_group_leader())
            .map_err(LocalPtyError::ProcessTree)?;
        #[cfg(windows)]
        self.process_tree
            .terminate()
            .map_err(LocalPtyError::ProcessTree)?;
        self.child.wait()?;
        Ok(())
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = self
            .process_tree
            .terminate(self.master.process_group_leader());
        #[cfg(windows)]
        let _ = self.process_tree.terminate();
    }
}

#[cfg(windows)]
fn find_on_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{LocalProfile, PtySession, WorkingDirectoryPolicy};
    use cshell_domain::TerminalSize;
    use cshell_terminal::{AlacrittyTerminalEngine, TerminalEngine};
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn platform_default_has_a_program() {
        let profile = LocalProfile::platform_default();
        assert!(!profile.name.is_empty());
        assert!(!profile.program.as_os_str().is_empty());
    }

    #[test]
    fn pty_runs_a_short_command_and_captures_output() {
        #[cfg(windows)]
        let profile = LocalProfile {
            name: "PTY probe".to_owned(),
            program: PathBuf::from("whoami.exe"),
            args: Vec::new(),
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        let profile = LocalProfile {
            name: "PTY probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-lc".to_owned(), "printf CSHELL_PTY_OK".to_owned()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        let mut session = PtySession::spawn(&profile, TerminalSize::cells(24, 80)).unwrap();
        let reader = session.take_reader().unwrap();

        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = vec![0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _send_result = sender.send(Ok(Vec::new()));
                        break;
                    }
                    Ok(count) => {
                        if sender.send(Ok(buffer[..count].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _send_result = sender.send(Err(error));
                        break;
                    }
                }
            }
        });

        let mut terminal = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
        let mut output = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            while let Ok(chunk) = receiver.try_recv() {
                let chunk = chunk.unwrap();
                if chunk.is_empty() {
                    continue;
                }
                output.extend_from_slice(&chunk);
                let delta = terminal.feed(&chunk);
                for response in delta.terminal_responses {
                    session.write_all(&response).unwrap();
                }
            }
            if let Some(status) = session.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                session.kill().unwrap();
                panic!("PTY child did not exit within five seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        session.close_input();
        drop(session);
        while let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(50)) {
            let chunk = chunk.unwrap();
            if chunk.is_empty() {
                break;
            }
            output.extend_from_slice(&chunk);
        }
        let output = String::from_utf8_lossy(&output);
        assert!(status.success(), "PTY status: {status}; output: {output:?}");
        #[cfg(windows)]
        assert!(!output.trim().is_empty(), "PTY output was empty");
        #[cfg(not(windows))]
        assert!(output.contains("CSHELL_PTY_OK"), "PTY output: {output:?}");
    }

    #[test]
    fn killing_a_pty_session_terminates_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let heartbeat = directory.path().join("descendant-heartbeat");
        let profile = LocalProfile {
            name: "PTY process tree probe".to_owned(),
            program: std::env::current_exe().unwrap(),
            args: vec![
                "--ignored".to_owned(),
                "--exact".to_owned(),
                "tests::process_tree_helper".to_owned(),
                "--nocapture".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::from([
                ("CSHELL_PROCESS_TREE_HELPER".to_owned(), "parent".to_owned()),
                (
                    "CSHELL_PROCESS_TREE_HEARTBEAT".to_owned(),
                    heartbeat.to_string_lossy().into_owned(),
                ),
            ]),
        };
        let mut session = PtySession::spawn(&profile, TerminalSize::cells(24, 80)).unwrap();
        let reader = session.take_reader().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0_u8; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        let mut answered_cursor_query = false;
        loop {
            if let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(50)) {
                output.extend_from_slice(&chunk);
            }
            if !answered_cursor_query && output.windows(4).any(|window| window == b"\x1b[6n") {
                session.write_all(b"\x1b[1;1R").unwrap();
                answered_cursor_query = true;
            }
            let descendant_started =
                String::from_utf8_lossy(&output).contains("CSHELL_DESCENDANT_STARTED");
            let heartbeat_started = heartbeat
                .metadata()
                .is_ok_and(|metadata| metadata.len() >= 3);
            if descendant_started && heartbeat_started {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "descendant did not start: {:?}",
                String::from_utf8_lossy(&output)
            );
        }

        session.kill().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let settled_length = heartbeat.metadata().unwrap().len();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            heartbeat.metadata().unwrap().len(),
            settled_length,
            "descendant continued writing after the PTY process tree was terminated"
        );
    }

    #[test]
    #[ignore = "invoked as a subprocess by the process-tree test"]
    fn process_tree_helper() {
        let Some(mode) = std::env::var_os("CSHELL_PROCESS_TREE_HELPER") else {
            return;
        };
        let heartbeat = std::env::var_os("CSHELL_PROCESS_TREE_HEARTBEAT").unwrap();
        if mode == "parent" {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "tests::process_tree_helper",
                    "--nocapture",
                ])
                .env("CSHELL_PROCESS_TREE_HELPER", "descendant")
                .env("CSHELL_PROCESS_TREE_HEARTBEAT", &heartbeat)
                .spawn()
                .unwrap();
            println!("CSHELL_DESCENDANT_STARTED={}", child.id());
            std::io::stdout().flush().unwrap();
            child.wait().unwrap();
            return;
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(heartbeat)
            .unwrap();
        #[cfg(unix)]
        crate::unix_process_tree::make_self_foreground().unwrap();
        loop {
            file.write_all(b"x").unwrap();
            file.flush().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
