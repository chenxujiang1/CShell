//! Bounded read-only commands on an authenticated, non-PTY channel.
use super::*;
impl RusshClient {
    pub async fn file_checksum_probe(&self, path: &str, nonce: &[u8]) -> bool {
        let Some(path) = quote_path(path) else {
            return false;
        };
        self.checksum_command(format!("LC_ALL=C cat -- {path}"), Duration::from_secs(3))
            .await
            .as_deref()
            == Some(nonce)
    }
    pub async fn file_sha256(&self, path: &str) -> Option<[u8; 32]> {
        let path = quote_path(path)?;
        let output = self
            .checksum_command(
                format!("LC_ALL=C sha256sum < {path}"),
                Duration::from_secs(1800),
            )
            .await?;
        parse_sha256(&output)
    }
    async fn checksum_command(&self, command: String, limit: Duration) -> Option<Vec<u8>> {
        tokio::time::timeout(limit, async {
            let mut opening =
                OpeningTerminalChannel(Some(self.session.channel_open_session().await.ok()?));
            let channel = opening.0.as_mut()?;
            channel.exec(true, command.as_bytes()).await.ok()?;
            channel.eof().await.ok()?;
            let mut output = Vec::new();
            let mut success = false;
            while let Some(event) = channel.wait().await {
                match event {
                    russh::ChannelMsg::Data { data } => {
                        if output.len() + data.len() > 1024 {
                            return None;
                        }
                        output.extend_from_slice(&data);
                    }
                    russh::ChannelMsg::ExtendedData { data, .. } => {
                        if !data.is_empty() {
                            return None;
                        }
                    }
                    russh::ChannelMsg::Failure => return None,
                    russh::ChannelMsg::ExitStatus { exit_status } => {
                        if exit_status != 0 {
                            return None;
                        }
                        success = true;
                    }
                    russh::ChannelMsg::Close => break,
                    _ => {}
                }
            }
            success.then_some(output)
        })
        .await
        .ok()
        .flatten()
    }
}
fn quote_path(path: &str) -> Option<String> {
    (path.starts_with('/') && path.len() <= 4096 && !path.chars().any(char::is_control))
        .then(|| format!("'{}'", path.replace('\'', "'\\''")))
}
fn parse_sha256(output: &[u8]) -> Option<[u8; 32]> {
    if output.len() != 68 || &output[64..] != b"  -\n" {
        return None;
    }
    let mut digest = [0; 32];
    for (index, pair) in output[..64].chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).ok()?;
        digest[index] = u8::from_str_radix(text, 16).ok()?;
    }
    Some(digest)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_commands_quote_paths_and_require_exact_output() {
        assert_eq!(quote_path("/a'$(id)"), Some("'/a'\\''$(id)'".into()));
        assert!(quote_path("a").is_none());
        assert!(quote_path("/a\n").is_none());
        let good = format!("{}  -\n", "ab".repeat(32));
        assert_eq!(parse_sha256(good.as_bytes()), Some([0xab; 32]));
        assert!(parse_sha256(format!("banner\n{good}").as_bytes()).is_none());
        assert!(parse_sha256(format!("{}  file\n", "ab".repeat(32)).as_bytes()).is_none());
    }
}
