//! Bound control-string storage before bytes reach VTE's growable OSC Vec.
const MAX_OSC_BYTES: usize = 64 * 1024;
#[derive(Debug, Default)]
pub(super) struct OutputGuard {
    escape: bool,
    osc: bool,
    osc_escape: bool,
    discarded: bool,
    buffer: Vec<u8>,
    pub(super) events: Vec<(usize, Vec<u8>)>,
}
impl OutputGuard {
    pub(super) fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        if !self.osc && !self.escape && !bytes.contains(&0x1b) {
            return bytes.to_vec();
        }
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.osc {
                if byte == 0x18 || byte == 0x1a {
                    self.finish(&mut out, false);
                    continue;
                }
                if self.osc_escape {
                    self.osc_escape = false;
                    if byte == b'\\' {
                        self.finish(&mut out, true);
                        continue;
                    }
                    // ESC with a non-ST final aborts OSC and starts another escape.
                    self.finish(&mut out, false);
                    if byte == b']' {
                        self.osc = true;
                    } else if byte == 0x1b {
                        self.escape = true;
                    } else {
                        out.push(0x1b);
                        out.push(byte);
                    }
                    continue;
                }
                if byte == 0x1b {
                    self.osc_escape = true;
                    continue;
                }
                if byte == 0x07 {
                    self.finish(&mut out, true);
                    continue;
                }
                if !self.discarded {
                    if self.buffer.len() == MAX_OSC_BYTES {
                        self.buffer.clear();
                        self.discarded = true;
                    } else {
                        self.buffer.push(byte);
                        // OSC52 is observed separately with its own consent and size policy.
                        // It must never be accumulated or decoded by the terminal parser.
                        if self.buffer == b"52;" {
                            self.buffer.clear();
                            self.discarded = true;
                        }
                    }
                }
            } else if self.escape {
                self.escape = false;
                if byte == b']' {
                    self.osc = true;
                    self.buffer.clear();
                    self.discarded = false;
                } else {
                    out.push(0x1b);
                    if byte == 0x1b {
                        self.escape = true;
                    } else {
                        out.push(byte);
                    }
                }
            } else if byte == 0x1b {
                self.escape = true;
            } else {
                out.push(byte);
            }
        }
        out
    }
    fn finish(&mut self, out: &mut Vec<u8>, complete: bool) {
        if complete && !self.discarded {
            if self.buffer.len() <= 4096 && (self.buffer.starts_with(b"133;") || self.buffer.starts_with(b"7;file://")) {
                self.events.push((out.len(), self.buffer.clone()));
            }
            out.extend_from_slice(b"\x1b]");
            out.extend_from_slice(&self.buffer);
            out.push(0x07);
        }
        self.osc = false;
        self.osc_escape = false;
        self.buffer.clear();
        self.discarded = false;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_osc_is_bounded_and_parser_recovers_without_payload_leak() {
        let mut guard = OutputGuard::default();
        assert!(guard.filter(b"\x1b]0;").is_empty());
        for _ in 0..64 {
            assert!(guard.filter(&vec![b'x'; 4096]).is_empty());
        }
        assert!(guard.buffer.capacity() <= MAX_OSC_BYTES);
        assert_eq!(guard.filter(b"\x1b\\normal"), b"normal");
        assert!(guard.filter(b"\x1b]52;c;").is_empty());
        assert!(guard.filter(&vec![b'A'; 131072]).is_empty());
        assert_eq!(guard.filter(b"\x07text"), b"text");
        assert_eq!(
            guard.filter(b"\x1b]8;;https://example.org\x07"),
            b"\x1b]8;;https://example.org\x07"
        );
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;
    #[test]
    fn escape_restarting_an_osc_cannot_bypass_the_preparser_budget() {
        let mut guard = OutputGuard::default();
        assert!(guard.filter(b"\x1b]0;old\x1b]2;new").is_empty());
        for _ in 0..128 {
            assert!(guard.filter(&vec![b'x'; 65536]).is_empty());
        }
        assert!(guard.buffer.capacity() <= MAX_OSC_BYTES);
        assert_eq!(
            guard.filter(b"\x1b]8;;https://example.org\x07ok"),
            b"\x1b]8;;https://example.org\x07ok"
        );
        assert!(guard.filter(b"\x1b]0;aborted\x18").is_empty());
        assert_eq!(guard.filter(b"normal"), b"normal");
    }
}
