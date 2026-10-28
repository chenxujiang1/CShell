//! Bounded OSC 7 observation, with passive per-shell Linux discovery as fallback.
use super::*;
#[derive(Debug, Default)]
pub(super) struct DirectoryTracker {
    state: u8,
    bytes: Vec<u8>,
    pub path: Option<String>,
}
impl DirectoryTracker {
    pub fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match self.state {
                0 => {
                    if b == 27 {
                        self.state = 1
                    }
                }
                1 => {
                    self.bytes.clear();
                    self.state = if b == b']' {
                        2
                    } else if b == 27 {
                        1
                    } else {
                        0
                    };
                }
                2 | 3 => {
                    if b == 24 || b == 26 {
                        self.state = 0;
                        self.bytes.clear();
                        continue;
                    }
                    if b == 7 || self.state == 3 && b == b'\\' {
                        if let Some(path) = decode(&self.bytes) {
                            self.path = Some(path);
                        }
                        self.state = 0;
                        self.bytes.clear();
                    } else if b == 27 {
                        self.state = 3;
                    } else if self.state == 3 {
                        self.bytes.clear();
                        self.state = if b == b']' { 2 } else { 0 };
                    } else if self.bytes.len() < 8192 {
                        self.bytes.push(b);
                    } else {
                        self.state = 4;
                        self.bytes.clear();
                    }
                }
                _ => {
                    if b == 7 {
                        self.state = 0;
                    } else if b == 27 {
                        self.state = 5;
                    } else if self.state == 5 {
                        self.state = if b == b'\\' { 0 } else { 4 };
                    }
                }
            }
        }
    }
    pub fn reset(&mut self) {
        self.path = None;
        self.state = 0;
        self.bytes.clear();
    }
}
fn decode(data: &[u8]) -> Option<String> {
    let uri = data.strip_prefix(b"7;file://")?;
    let start = uri.iter().position(|b| *b == b'/')?;
    let mut out = Vec::new();
    let mut i = start;
    while i < uri.len() {
        if uri[i] == b'%' {
            let h = std::str::from_utf8(uri.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(uri[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    cshell_application::sftp_remote_path(&path, true).then_some(path)
}
impl SshSession {
    pub(crate) async fn current_remote_directory(&self) -> Option<String> {
        if !self.running() {
            return None;
        }
        let epoch = self.connection_epoch.load(Ordering::Acquire);
        let reported = self
            .directory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .path
            .clone();
        let path = if let Some(path) = reported {
            Some(path)
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(4),
                self.active_transport()
                    .client
                    .remote_working_directory(&self.id.to_string()),
            )
            .await
            .ok()
            .flatten()
        };
        if self.connection_epoch.load(Ordering::Acquire) != epoch || !self.running() {
            None
        } else {
            path
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn osc7_is_fragmented_bounded_decoded_and_reset_on_reconnect() {
        let mut tracker = DirectoryTracker::default();
        for part in [
            b"text\x1b]7;fi".as_slice(),
            b"le://host/home/a%20b",
            b"\x1b\\",
        ] {
            tracker.feed(part);
        }
        assert_eq!(tracker.path.as_deref(), Some("/home/a b"));
        tracker.feed(b"\x1b]7;file://host/etc%00/passwd\x07");
        assert_eq!(tracker.path.as_deref(), Some("/home/a b"));
        tracker.feed(b"\x1b]7;file://host/");
        tracker.feed(&vec![b'x'; 100_000]);
        tracker.feed(b"\x1b\\");
        assert!(tracker.bytes.capacity() <= 8192);
        tracker.feed(b"\x1b]7;file:///srv/next\x07");
        assert_eq!(tracker.path.as_deref(), Some("/srv/next"));
        tracker.reset();
        assert!(tracker.path.is_none());
    }
}
