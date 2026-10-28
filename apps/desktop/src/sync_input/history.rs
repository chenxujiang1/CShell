//! Bounded, process-only history. Never serialize command bodies into preferences or audit.
use zeroize::Zeroizing;

const MAX_ENTRIES: usize = 100;
const MAX_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct CommandHistory {
    entries: Vec<Zeroizing<String>>,
    cursor: Option<usize>,
    draft: Zeroizing<String>,
}

impl CommandHistory {
    pub(super) fn record(&mut self, body: &str) {
        self.reset_cursor();
        if body.is_empty() || body.len() > MAX_BYTES {
            return;
        }
        // A repeated command remains useful once; move it to the newest position.
        self.entries.retain(|entry| entry.as_str() != body);
        self.entries.push(Zeroizing::new(body.to_owned()));
        while self.entries.len() > MAX_ENTRIES
            || self.entries.iter().map(|entry| entry.len()).sum::<usize>() > MAX_BYTES
        {
            self.entries.remove(0);
        }
    }

    pub(super) fn reset_cursor(&mut self) {
        self.cursor = None;
        self.draft = Zeroizing::default();
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.reset_cursor();
    }

    pub(super) fn entries(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.entries.iter().map(|entry| entry.as_str())
    }

    pub(super) fn navigate(&mut self, older: bool, current: &str) -> Option<Zeroizing<String>> {
        if self.entries.is_empty() {
            return None;
        }
        if older {
            let next = match self.cursor {
                None => {
                    self.draft = Zeroizing::new(current.to_owned());
                    self.entries.len() - 1
                }
                Some(index) => index.saturating_sub(1),
            };
            self.cursor = Some(next);
            Some(self.entries[next].clone())
        } else {
            let index = self.cursor?;
            if index + 1 == self.entries.len() {
                self.cursor = None;
                Some(std::mem::take(&mut self.draft))
            } else {
                self.cursor = Some(index + 1);
                Some(self.entries[index + 1].clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_restores_unsent_draft_and_never_wraps() {
        let mut h = CommandHistory::default();
        h.record("hostname");
        h.record("whoami");
        assert_eq!(h.navigate(true, "draft").unwrap().as_str(), "whoami");
        assert_eq!(h.navigate(true, "whoami").unwrap().as_str(), "hostname");
        assert_eq!(h.navigate(true, "hostname").unwrap().as_str(), "hostname");
        assert_eq!(h.navigate(false, "hostname").unwrap().as_str(), "whoami");
        assert_eq!(h.navigate(false, "whoami").unwrap().as_str(), "draft");
        assert!(h.navigate(false, "draft").is_none());
    }

    #[test]
    fn bounds_apply_to_count_and_utf8_bytes_and_deduplicate() {
        let mut h = CommandHistory::default();
        for i in 0..150 {
            h.record(&format!("hostname {i}"));
        }
        assert_eq!(h.entries().count(), MAX_ENTRIES);
        h.record("hostname 149");
        assert_eq!(h.entries().count(), MAX_ENTRIES);
        for i in 0..10 {
            h.record(&format!("{i}{}", "你".repeat(20000)));
        }
        assert!(h.entries().map(str::len).sum::<usize>() <= MAX_BYTES);
        h.clear();
        assert_eq!(h.entries().count(), 0);
        assert!(h.navigate(true, "draft").is_none());
    }
}
