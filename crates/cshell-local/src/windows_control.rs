//! A hidden daemon may inherit a launcher that ignores Ctrl+C. Do not pass that
//! inheritable process attribute to interactive ConPTY shells and their children.
#![allow(unsafe_code)]

use std::io;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

pub(super) fn restore_interrupt_inheritance() -> io::Result<()> {
    // SAFETY: NULL + FALSE only restores the calling process's inheritable
    // Ctrl+C attribute. It neither sends a signal nor removes registered
    // application/Tokio handlers, and does not modify another process.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{LocalProfile, PtySession, WorkingDirectoryPolicy};
    use cshell_domain::TerminalSize;
    use cshell_terminal::{AlacrittyTerminalEngine, FrameSnapshot, TerminalEngine};
    use std::{
        collections::BTreeMap,
        io::Read,
        os::windows::process::CommandExt,
        path::PathBuf,
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn wait_for(
        pty: &mut PtySession,
        rx: &mpsc::Receiver<Vec<u8>>,
        terminal: &mut AlacrittyTerminalEngine,
        ready: impl Fn(&FrameSnapshot) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            for bytes in rx.try_iter() {
                for response in terminal.feed(&bytes).terminal_responses {
                    pty.write_all(&response).unwrap();
                }
            }
            if ready(&terminal.snapshot()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "owned PTY did not reach the expected shell state"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn line(snapshot: &FrameSnapshot, row: u16) -> String {
        snapshot
            .row(row)
            .unwrap()
            .iter()
            .flat_map(cshell_terminal::Cell::characters)
            .collect::<String>()
            .trim()
            .to_owned()
    }

    #[test]
    fn inherited_ignore_flag_does_not_disable_child_interrupts() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows_control::tests::inherited_flag_probe_child",
                "--ignored",
                "--nocapture",
            ])
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "private isolated subprocess helper for inherited Ctrl+C state"]
    fn inherited_flag_probe_child() {
        // SAFETY: only this dedicated helper process is made to ignore Ctrl+C.
        assert_ne!(unsafe { SetConsoleCtrlHandler(None, 1) }, 0);
        let size = TerminalSize::cells(24, 100);
        let mut pty = PtySession::spawn(
            &LocalProfile {
                name: "interrupt inheritance fixture".into(),
                program: PathBuf::from("cmd.exe"),
                args: vec![
                    "/D".into(),
                    "/Q".into(),
                    "/K".into(),
                    "prompt CSHELL_INTERRUPT_READY$G".into(),
                ],
                cwd_policy: WorkingDirectoryPolicy::Inherit,
                env_overrides: BTreeMap::new(),
            },
            size,
        )
        .unwrap();
        let mut reader = pty.take_reader().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 || tx.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut terminal = AlacrittyTerminalEngine::new(size);
        let prompt = |s: &FrameSnapshot| line(s, s.cursor_row) == "CSHELL_INTERRUPT_READY>";
        wait_for(&mut pty, &rx, &mut terminal, prompt);
        pty.write_all(b"ping -t 127.0.0.1\r").unwrap();
        wait_for(&mut pty, &rx, &mut terminal, |s| {
            s.cells
                .iter()
                .flat_map(cshell_terminal::Cell::characters)
                .collect::<String>()
                .contains("TTL=")
        });
        pty.write_all(&[3]).unwrap();
        wait_for(&mut pty, &rx, &mut terminal, prompt);
        pty.write_all(b"echo CSHELL_INTERRUPT_RETURNED\r").unwrap();
        wait_for(&mut pty, &rx, &mut terminal, |s| {
            (0..s.rows).any(|r| line(s, r) == "CSHELL_INTERRUPT_RETURNED")
        });
    }
}
