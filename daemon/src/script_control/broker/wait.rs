use cshell_automation::OutputDecoder;
use cshell_output_store::JournalRawSlice;
use cshell_script_runtime::ScriptWorkerError;
use std::collections::VecDeque;
use zeroize::Zeroizing;

pub(crate) struct WaitCursor {
    sequence: u64,
    offset: u64,
    decoder: OutputDecoder,
    pending: VecDeque<u8>,
}

impl WaitCursor {
    pub(crate) fn new(sequence: u64) -> Self {
        Self {
            sequence,
            offset: 0,
            decoder: OutputDecoder::default(),
            pending: VecDeque::new(),
        }
    }

    pub(super) fn position(&self) -> (u64, u64) {
        (self.sequence, self.offset)
    }

    pub(super) fn match_pending(&mut self, matcher: &mut LiteralWaitMatcher) -> bool {
        while let Some(byte) = self.pending.pop_front() {
            if matcher.feed_byte(byte) {
                return true;
            }
        }
        false
    }

    pub(super) fn consume(
        &mut self,
        raw: &JournalRawSlice,
        matcher: &mut LiteralWaitMatcher,
    ) -> Result<bool, ScriptWorkerError> {
        let end = self
            .offset
            .checked_add(raw.data.len() as u64)
            .ok_or(ScriptWorkerError::Protocol)?;
        if raw.sequence != self.sequence
            || raw.offset != self.offset
            || end > raw.record_bytes
            || raw.data.is_empty() && raw.record_bytes != 0
        {
            return Err(ScriptWorkerError::Protocol);
        }
        let mut matched = None;
        for (index, byte) in raw.data.iter().enumerate() {
            // One raw byte can finish invalid UTF-8 and emit both a replacement
            // character and a following character. Keep any decoded tail too.
            self.pending
                .extend(self.decoder.feed(std::slice::from_ref(byte)).bytes());
            if self.match_pending(matcher) {
                matched = Some(index + 1);
                break;
            }
        }
        self.offset += matched.unwrap_or(raw.data.len()) as u64;
        if self.offset == raw.record_bytes {
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or(ScriptWorkerError::Protocol)?;
            self.offset = 0;
        }
        Ok(matched.is_some())
    }
}

// KMP keeps only the literal and its prefix table. Matching is linear in the
// decoded output, including repetitive output, and accepts the full script
// pattern limit without changing the limits for ordinary trigger rules.
pub(super) struct LiteralWaitMatcher {
    text: Zeroizing<String>,
    prefix: Vec<usize>,
    matched: usize,
}

impl LiteralWaitMatcher {
    pub(super) fn new(text: Zeroizing<String>) -> Result<Self, ScriptWorkerError> {
        if !cshell_domain::valid_script_wait_pattern(&text) {
            return Err(ScriptWorkerError::Invalid);
        }
        let bytes = text.as_bytes();
        let mut prefix = vec![0; bytes.len()];
        let mut matched = 0;
        for i in 1..bytes.len() {
            while matched > 0 && bytes[i] != bytes[matched] {
                matched = prefix[matched - 1];
            }
            if bytes[i] == bytes[matched] {
                matched += 1;
            }
            prefix[i] = matched;
        }
        Ok(Self {
            text,
            prefix,
            matched: 0,
        })
    }

    fn feed_byte(&mut self, byte: u8) -> bool {
        let pattern = self.text.as_bytes();
        while self.matched > 0 && byte != pattern[self.matched] {
            self.matched = self.prefix[self.matched - 1];
        }
        if byte == pattern[self.matched] {
            self.matched += 1;
        }
        self.matched == pattern.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn matcher(text: &str) -> LiteralWaitMatcher {
        LiteralWaitMatcher::new(Zeroizing::new(text.to_owned())).unwrap()
    }

    fn wait(cursor: &mut WaitCursor, records: &[&[u8]], text: &str) -> bool {
        let mut matcher = matcher(text);
        if cursor.match_pending(&mut matcher) {
            return true;
        }
        while (cursor.sequence as usize) < records.len() {
            let record = records[cursor.sequence as usize];
            let raw = JournalRawSlice {
                sequence: cursor.sequence,
                offset: cursor.offset,
                record_bytes: record.len() as u64,
                total_records: records.len() as u64,
                data: record[cursor.offset as usize..].to_vec(),
                timing: None,
            };
            if cursor.consume(&raw, &mut matcher).unwrap() {
                return true;
            }
        }
        false
    }

    #[test]
    fn consecutive_waits_preserve_the_tail_of_the_same_record() {
        let mut cursor = WaitCursor::new(0);
        assert!(wait(&mut cursor, &[b"READY DONE"], "READY"));
        assert_eq!(cursor.position(), (0, 5));
        assert!(wait(&mut cursor, &[b"READY DONE"], "DONE"));
        assert_eq!(cursor.position(), (1, 0));
        assert!(!wait(&mut cursor, &[b"READY DONE"], "DONE"));
    }

    #[test]
    fn consecutive_waits_are_independent_of_ansi_and_utf8_record_boundaries() {
        let bytes = "\x1b]0;hidden\x07\x1b[31m就绪\x1b[0m 完成".as_bytes();
        for split in 0..=bytes.len() {
            let records = [&bytes[..split], &bytes[split..]];
            let mut cursor = WaitCursor::new(0);
            assert!(wait(&mut cursor, &records, "就绪"), "split {split}");
            assert!(wait(&mut cursor, &records, "完成"), "split {split}");
        }
    }

    #[test]
    fn decoder_state_survives_a_wait_that_does_not_match() {
        let mut cursor = WaitCursor::new(0);
        assert!(!wait(&mut cursor, &[b"\x1b]0;"], "absent"));
        assert!(wait(
            &mut cursor,
            &[b"\x1b]0;", b"HIDDEN\x07VISIBLE"],
            "VISIBLE"
        ));
        let mut cursor = WaitCursor::new(0);
        assert!(!wait(&mut cursor, &[b"\x1b]0;"], "absent"));
        assert!(!wait(
            &mut cursor,
            &[b"\x1b]0;", b"HIDDEN\x07VISIBLE"],
            "HIDDEN"
        ));
    }

    #[test]
    fn invalid_utf8_does_not_consume_the_next_visible_character() {
        let mut cursor = WaitCursor::new(0);
        assert!(wait(&mut cursor, &[b"\xe4DONE"], "\u{fffd}"));
        assert!(wait(&mut cursor, &[b"\xe4DONE"], "DONE"));
        let mut cursor = WaitCursor::new(0);
        assert!(wait(&mut cursor, &[b"\xe4A"], "\u{fffd}"));
        assert!(wait(&mut cursor, &[b"\xe4A"], "A"));
    }

    #[test]
    fn full_script_pattern_limit_matches_across_records() {
        for length in [512, 513, cshell_domain::MAX_SCRIPT_WAIT_PATTERN_BYTES] {
            let text = format!("{}b", "a".repeat(length - 1));
            let records = [
                &text.as_bytes()[..length / 2],
                &text.as_bytes()[length / 2..],
            ];
            assert!(wait(&mut WaitCursor::new(0), &records, &text));
        }
        let unicode = "界".repeat(1365) + "x";
        assert_eq!(unicode.len(), cshell_domain::MAX_SCRIPT_WAIT_PATTERN_BYTES);
        assert!(wait(
            &mut WaitCursor::new(0),
            &[unicode.as_bytes()],
            &unicode
        ));
    }

    #[test]
    fn invalid_script_patterns_are_rejected_consistently() {
        for text in [
            String::new(),
            "a".repeat(cshell_domain::MAX_SCRIPT_WAIT_PATTERN_BYTES + 1),
            "a\nb".into(),
            "a\0b".into(),
        ] {
            assert!(!cshell_domain::valid_script_wait_pattern(&text));
            assert!(matches!(
                LiteralWaitMatcher::new(Zeroizing::new(text)),
                Err(ScriptWorkerError::Invalid)
            ));
        }
    }

    #[test]
    fn overlapping_prefixes_and_repetitive_output_match() {
        assert!(wait(&mut WaitCursor::new(0), &[b"aaaaaab"], "aaaab"));
        assert!(wait(&mut WaitCursor::new(0), &[b"ababababac"], "ababac"));
        let mut bytes = vec![b'a'; 128 * 1024];
        bytes.push(b'b');
        let text = format!("{}b", "a".repeat(4095));
        assert!(wait(&mut WaitCursor::new(0), &[&bytes], &text));
    }
}
