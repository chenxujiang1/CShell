//! Read-only Linux shell directory discovery on a separate non-PTY channel.
use super::*;
impl RusshClient {
    pub async fn remote_working_directory(&self, marker: &str) -> Option<String> {
        if marker.is_empty()
            || marker.len() > 64
            || !marker
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return None;
        }
        let command = directory_command(marker)?;
        let mut opening =
            OpeningTerminalChannel(Some(self.session.channel_open_session().await.ok()?));
        let channel = opening.0.as_mut()?;
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            channel.exec(true, command.as_bytes()).await.ok()?;
            channel.eof().await.ok()?;
            let mut output = Vec::new();
            let mut success = false;
            while let Some(event) = channel.wait().await {
                match event {
                    russh::ChannelMsg::Data { data } => {
                        if output.len() + data.len() > 8192 {
                            return None;
                        }
                        output.extend_from_slice(&data);
                    }
                    russh::ChannelMsg::Failure => return None,
                    russh::ChannelMsg::ExitStatus { exit_status } => success = exit_status == 0,
                    russh::ChannelMsg::Close => break,
                    _ => {}
                }
            }
            if !success {
                return None;
            }
            let prefix = b"\0CSHELL_CWD\0";
            let start = output.windows(prefix.len()).position(|w| w == prefix)? + prefix.len();
            let path = std::str::from_utf8(&output[start..])
                .ok()?
                .strip_suffix('\n')?;
            if !path.starts_with('/') || path.len() > 4000 || path.chars().any(char::is_control) {
                return None;
            }
            Some(path.to_owned())
        })
        .await
        .ok()
        .flatten();
        result
    }
}

fn directory_command(marker: &str) -> Option<String> {
    if marker.is_empty()
        || marker.len() > 64
        || !marker
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return None;
    }
    let script = format!(
        r#"files=$(grep -azlxF -- 'LC_CSHELL_SESSION={marker}' /proc/[0-9]*/environ 2>/dev/null); if [ -z "$files" ] && [ -n "$SSH_CONNECTION" ]; then candidates=$(grep -azlxF -- "SSH_CONNECTION=$SSH_CONNECTION" /proc/[0-9]*/environ 2>/dev/null); tty=; for e in $candidates; do line=$(tr '\000' '\n' < "$e" 2>/dev/null | grep '^SSH_TTY=' | head -n 1); [ -n "$line" ] || continue; if [ -n "$tty" ] && [ "$tty" != "$line" ]; then exit 1; fi; tty=$line; files="$files $e"; done; fi; first=; for e in $files; do p=${{e%/environ}}; p=${{p#/proc/}}; case "$p" in ''|*[!0-9]*) continue;; esac; if [ -z "$first" ] || [ "$p" -lt "$first" ]; then first=$p; fi; done; [ -n "$first" ] || exit 1; printf '\000CSHELL_CWD\000'; readlink -- "/proc/$first/cwd""#
    );
    Some(format!("sh -c '{}'", script.replace('\'', "'\\''")))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cwd_probe_accepts_only_an_opaque_channel_marker() {
        for marker in ["", "a'; id", "a$(id)", "a/b", "a\n"] {
            assert!(directory_command(marker).is_none());
        }
        let command = directory_command("test-channel-123").unwrap();
        assert!(command.contains("LC_CSHELL_SESSION=test-channel-123"));
        assert!(command.contains("SSH_CONNECTION=$SSH_CONNECTION"));
        assert!(!command.contains("pwd"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_probe_reads_a_live_marked_shell_cwd_without_input() {
        use std::process::{Command, Stdio};
        let marker = format!("cshell-probe-{}", std::process::id());
        let root = tempfile::tempdir().unwrap();
        let mut shell = Command::new("sh")
            .args(["-c", "sleep 30"])
            .env("LC_CSHELL_SESSION", &marker)
            .current_dir(root.path())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let result = Command::new("sh")
            .arg("-c")
            .arg(directory_command(&marker).unwrap())
            .output();
        let _ = shell.kill();
        let _ = shell.wait();
        let result = result.unwrap();
        assert!(result.status.success());
        let mut expected = b"\0CSHELL_CWD\0".to_vec();
        expected.extend_from_slice(root.path().to_str().unwrap().as_bytes());
        expected.push(b'\n');
        assert_eq!(result.stdout, expected);
    }
}
