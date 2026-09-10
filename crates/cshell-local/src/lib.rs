//! Cross-platform local terminal profiles and portable-pty adapter.

use cshell_domain::TerminalSize;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use thiserror::Error;

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
}

pub struct PtySession {
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

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| LocalPtyError::Spawn(error.to_string()))?;
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
        self.child.kill().map_err(Into::into)
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
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
    use std::io::Read;
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
}
